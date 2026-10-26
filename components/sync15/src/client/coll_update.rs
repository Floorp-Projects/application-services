/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{
    CollState, Sync15ClientResponse, Sync15StorageClient,
    request::{NormalResponseHandler, UploadInfo},
};
use crate::bso::{IncomingBso, OutgoingBso, OutgoingEncryptedBso};
use crate::engine::{CollectionRequest, RequestOrder};
use crate::error::{self, Error, Result};
use crate::{CollectionName, KeyBundle, ServerTimestamp};
use std::collections::HashSet;

fn encrypt_outgoing(o: Vec<OutgoingBso>, key: &KeyBundle) -> Result<Vec<OutgoingEncryptedBso>> {
    o.into_iter()
        .map(|change| change.into_encrypted(key))
        .collect()
}

pub fn fetch_incoming(
    client: &Sync15StorageClient,
    state: &CollState,
    collection_request: CollectionRequest,
) -> Result<Vec<IncomingBso>> {
    fetch_incoming_checked(client, state, collection_request, &|| Ok(()))
}

pub fn fetch_incoming_checked(
    client: &Sync15StorageClient,
    state: &CollState,
    collection_request: CollectionRequest,
    check: &dyn Fn() -> Result<()>,
) -> Result<Vec<IncomingBso>> {
    check()?;
    // Discovery must never publish an incomplete collection. Ordinary engines
    // retain their existing request/limit behavior (some intentionally backfill).
    let records = if collection_request.collection.as_ref() == "floorp-workspaces" {
        collect_pages(
            state.last_modified,
            collection_request,
            |request, boundary| {
                check()?;
                // A backoff received on a page stops subsequent page GETs.
                // The initial request still follows the driver's user-action
                // policy, preserving existing manual-retry semantics.
                check_page_backoff(&client.backoff, &request)?;
                client.get_encrypted_records_page(request, boundary)
            },
        )?
    } else {
        match client.get_encrypted_records(collection_request)? {
            Sync15ClientResponse::Success {
                record,
                last_modified,
                ..
            } => {
                let _ = last_modified;
                record
            }
            other => return Err(other.create_storage_error()),
        }
    };
    let mut result = Vec::with_capacity(records.len());
    for record in records {
        check()?;
        // if we see a HMAC error, we've made an explicit decision to
        // NOT handle it here, but restart the global state machine.
        // That should cause us to re-read crypto/keys and things should
        // work (although if for some reason crypto/keys was updated but
        // not all storage was wiped we are probably screwed.)
        result.push(record.into_decrypted(&state.key)?);
    }
    Ok(result)
}

fn check_page_backoff(
    backoff: &super::storage_client::BackoffState,
    request: &CollectionRequest,
) -> Result<()> {
    if request.offset.is_some()
        && let Some(wait) = backoff.get_required_wait(false)
    {
        return Err(Error::BackoffError(std::time::SystemTime::now() + wait));
    }
    Ok(())
}

/// No partially fetched page is given to an engine. A 412, missing/mismatched
/// timestamp, offset loop, or decryption failure leaves its committed cursor
/// untouched; the scheduler retries from that cursor on the next run.
fn collect_pages<T>(
    boundary: ServerTimestamp,
    mut request: CollectionRequest,
    mut fetch: impl FnMut(
        CollectionRequest,
        ServerTimestamp,
    ) -> Result<(Sync15ClientResponse<Vec<T>>, Option<String>)>,
) -> Result<Vec<T>> {
    request = request.limit(500, RequestOrder::Oldest);
    let mut offsets = HashSet::new();
    let mut records = Vec::new();
    loop {
        let (response, next) = fetch(request.clone(), boundary)?;
        match response {
            Sync15ClientResponse::Success {
                record,
                last_modified,
                ..
            } => {
                if last_modified != boundary {
                    return Err(Error::StorageHttpError(
                        crate::error::ErrorResponse::PreconditionFailed {
                            route: format!("storage/{}", request.collection),
                        },
                    ));
                }
                records.extend(record);
            }
            other => return Err(other.create_storage_error()),
        }
        match next {
            None => return Ok(records),
            Some(offset) => {
                if offset.is_empty() || offset.len() > 4096 || !offsets.insert(offset.clone()) {
                    return Err(Error::ServerBatchProblem(
                        "invalid or repeated incoming page offset",
                    ));
                }
                request.offset = Some(offset);
            }
        }
    }
}

pub struct CollectionUpdate<'a> {
    client: &'a Sync15StorageClient,
    state: &'a CollState,
    collection: CollectionName,
    xius: ServerTimestamp,
    to_update: Vec<OutgoingEncryptedBso>,
    fully_atomic: bool,
}

impl<'a> CollectionUpdate<'a> {
    pub fn new(
        client: &'a Sync15StorageClient,
        state: &'a CollState,
        collection: CollectionName,
        xius: ServerTimestamp,
        records: Vec<OutgoingEncryptedBso>,
        fully_atomic: bool,
    ) -> CollectionUpdate<'a> {
        CollectionUpdate {
            client,
            state,
            collection,
            xius,
            to_update: records,
            fully_atomic,
        }
    }

    pub fn new_from_changeset(
        client: &'a Sync15StorageClient,
        state: &'a CollState,
        collection: CollectionName,
        changeset: Vec<OutgoingBso>,
        fully_atomic: bool,
    ) -> Result<CollectionUpdate<'a>> {
        let to_update = encrypt_outgoing(changeset, &state.key)?;
        Ok(CollectionUpdate::new(
            client,
            state,
            collection,
            state.last_modified,
            to_update,
            fully_atomic,
        ))
    }

    /// Returns a list of the IDs that failed if allowed_dropped_records is true, otherwise
    /// returns an empty vec.
    pub fn upload(self) -> error::Result<UploadInfo> {
        let mut failed = vec![];
        let mut q = self.client.new_post_queue(
            &self.collection,
            &self.state.config,
            self.xius,
            NormalResponseHandler::new(!self.fully_atomic),
        )?;

        for record in self.to_update.into_iter() {
            let enqueued = q.enqueue(&record)?;
            if !enqueued && self.fully_atomic {
                return Err(Error::RecordTooLargeError);
            }
        }

        q.flush(true)?;
        let mut info = q.completed_upload_info();
        info.failed_ids.append(&mut failed);
        if self.fully_atomic {
            assert_eq!(
                info.failed_ids.len(),
                0,
                "Bug: Should have failed by now if we aren't allowing dropped records"
            );
        }
        Ok(info)
    }
}

#[cfg(test)]
mod paging_tests {
    use super::*;
    use crate::error::ErrorResponse;
    fn page(record: Vec<u32>, timestamp: i64) -> Sync15ClientResponse<Vec<u32>> {
        Sync15ClientResponse::Success {
            status: 200,
            record,
            last_modified: ServerTimestamp(timestamp),
            route: "storage/floorp-workspaces".into(),
        }
    }
    #[test]
    fn fetches_all_opaque_offsets_with_same_boundary_before_returning() {
        let mut seen = vec![];
        let records = collect_pages(
            ServerTimestamp(100),
            CollectionRequest::new("floorp-workspaces".into()).full(),
            |request, boundary| {
                assert_eq!(boundary, ServerTimestamp(100));
                seen.push(request.offset.clone());
                Ok(match request.offset.as_deref() {
                    None => (page(vec![1, 2], 100), Some("opaque:a+b/c=".into())),
                    Some("opaque:a+b/c=") => (page(vec![3], 100), None),
                    _ => panic!(),
                })
            },
        )
        .unwrap();
        assert_eq!(records, vec![1, 2, 3]);
        assert_eq!(seen, vec![None, Some("opaque:a+b/c=".into())]);
    }
    #[test]
    fn failure_or_412_on_later_page_never_returns_partial_collection() {
        for mismatch in [false, true] {
            let result = collect_pages(
                ServerTimestamp(100),
                CollectionRequest::new("floorp-workspaces".into()).full(),
                |request, _| {
                    if request.offset.is_none() {
                        Ok((page(vec![1], 100), Some("next".into())))
                    } else if mismatch {
                        Ok((page(vec![2], 101), None))
                    } else {
                        Ok((
                            Sync15ClientResponse::Error(ErrorResponse::PreconditionFailed {
                                route: "collection".into(),
                            }),
                            None,
                        ))
                    }
                },
            );
            assert!(matches!(
                result,
                Err(Error::StorageHttpError(
                    ErrorResponse::PreconditionFailed { .. }
                ))
            ));
        }
    }
    #[test]
    fn cyclic_offsets_fail_and_retry_starts_from_original_cursor() {
        let request = CollectionRequest::new("floorp-workspaces".into())
            .full()
            .newer_than(ServerTimestamp(50));
        let result = collect_pages(ServerTimestamp(100), request.clone(), |_, _| {
            Ok((page(vec![1], 100), Some("repeat".into())))
        });
        assert!(result.is_err());
        let result = collect_pages(ServerTimestamp(100), request, |request, _| {
            assert!(request.offset.is_none());
            assert_eq!(request.newer, Some(ServerTimestamp(50)));
            Ok((page(vec![1, 2], 100), None))
        })
        .unwrap();
        assert_eq!(result, vec![1, 2]);
    }
    #[test]
    fn newly_received_backoff_stops_subsequent_pages_without_partial_result() {
        let backoff = super::super::storage_client::BackoffState::default();
        let first = CollectionRequest::new("floorp-workspaces".into()).full();
        let mut fetched = 0;
        let result = collect_pages(ServerTimestamp(100), first, |request, _| {
            check_page_backoff(&backoff, &request)?;
            fetched += 1;
            backoff.note_backoff(60);
            Ok((page(vec![1], 100), Some("next".into())))
        });
        assert!(matches!(result, Err(Error::BackoffError(_))));
        assert_eq!(fetched, 1);
    }
}

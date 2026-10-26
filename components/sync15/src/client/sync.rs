/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::request::UploadInfo;
use super::{CollectionUpdate, GlobalState, LocalCollStateMachine, Sync15StorageClient};
use crate::KeyBundle;
use crate::ServerTimestamp;
use crate::bso::OutgoingBso;
use crate::clients_engine;
use crate::engine::SyncEngine;
use crate::error::{Error, info, warn};
use crate::telemetry;
use interrupt_support::Interruptee;

#[allow(clippy::too_many_arguments)]
pub fn synchronize_with_clients_engine(
    client: &Sync15StorageClient,
    global_state: &GlobalState,
    root_sync_key: &KeyBundle,
    clients: Option<&clients_engine::Engine<'_>>,
    engine: &dyn SyncEngine,
    fully_atomic: bool,
    telem_engine: &mut telemetry::Engine,
    interruptee: &dyn Interruptee,
) -> Result<(), Error> {
    let collection = engine.collection_name();
    info!("Syncing collection {}", collection);

    // our global state machine is ready - get the collection machine going.
    let coll_state = match LocalCollStateMachine::get_state(engine, global_state, root_sync_key)? {
        Some(coll_state) => coll_state,
        None => {
            // XXX - this is either "error" or "declined".
            warn!(
                "can't setup for the {} collection - hopefully it works later",
                collection
            );
            return Ok(());
        }
    };

    if let Some(clients) = clients {
        engine.prepare_for_sync(&|| clients.get_client_data())?;
    }
    interruptee.err_if_interrupted()?;
    // We assume an "engine" manages exactly one "collection" with the engine's name.
    match engine.get_collection_request(coll_state.last_modified)? {
        None => {
            info!("skipping incoming for {} - not needed.", collection);
        }
        Some(collection_request) => {
            // fetch_incoming fully downloads and fences every Workspace page
            // before staging; a failed page leaves the prior snapshot intact.
            let incoming =
                super::fetch_incoming_checked(client, &coll_state, collection_request, &|| {
                    engine.check_incoming_request()?;
                    interruptee.err_if_interrupted()?;
                    Ok(())
                })?;
            info!("Downloaded {} remote changes", incoming.len());
            engine.stage_incoming(incoming, telem_engine)?;
            interruptee.err_if_interrupted()?;
        }
    };

    // Should consider adding a new `fetch_outgoing()` and having `apply()` only apply.
    // It *might* even make sense to only call `apply()` when something was staged,
    // but that's not clear - see the discussion at
    // https://github.com/mozilla/application-services/pull/5441/files/f36274f455a6299f10e7ce56b167882c369aa806#r1189267540
    info!("Applying changes");
    let outgoing = engine.apply(coll_state.last_modified, telem_engine)?;
    interruptee.err_if_interrupted()?;

    finish_sync(
        engine,
        coll_state.last_modified,
        outgoing,
        telem_engine,
        |outgoing| {
            // Existing write-engine batching behavior remains unchanged. No
            // CollectionUpdate or PostQueue is constructed for read-only engines.
            CollectionUpdate::new_from_changeset(
                client,
                &coll_state,
                collection,
                outgoing,
                fully_atomic,
            )?
            .upload()
        },
    )
}

fn finish_sync(
    engine: &dyn SyncEngine,
    timestamp: ServerTimestamp,
    outgoing: Vec<OutgoingBso>,
    telem_engine: &mut telemetry::Engine,
    upload: impl FnOnce(Vec<OutgoingBso>) -> Result<UploadInfo, Error>,
) -> Result<(), Error> {
    // The uploader is deliberately lazy: a receive-only engine cannot even
    // construct an HTTP write/batch request, including the empty-upload path.
    if engine.is_read_only() {
        if !outgoing.is_empty() {
            return Err(anyhow::anyhow!("read-only engine returned outgoing records").into());
        }
        engine.set_uploaded(timestamp, Vec::new())?;
        engine.sync_finished()?;
        return Ok(());
    }
    info!("Uploading {} outgoing changes", outgoing.len());
    let upload_info = upload(outgoing)?;
    info!(
        "Upload success ({} records success, {} records failed)",
        upload_info.successful_ids.len(),
        upload_info.failed_ids.len()
    );

    let mut telem_outgoing = telemetry::EngineOutgoing::new();
    telem_outgoing.sent(upload_info.successful_ids.len() + upload_info.failed_ids.len());
    telem_outgoing.failed(upload_info.failed_ids.len());
    telem_engine.outgoing(telem_outgoing);

    engine.set_uploaded(upload_info.modified_timestamp, upload_info.successful_ids)?;

    // The above should all be per-batch :(

    engine.sync_finished()?;

    info!("Sync finished!");
    Ok(())
}

#[cfg(test)]
mod read_only_transport_tests {
    use super::*;
    use crate::bso::IncomingBso;
    use crate::engine::{CollectionRequest, EngineSyncAssociation};
    use crate::{CollectionName, Guid};
    use std::cell::Cell;
    struct Engine {
        read_only: bool,
        finished: Cell<bool>,
        acknowledged: Cell<usize>,
    }
    impl SyncEngine for Engine {
        fn collection_name(&self) -> CollectionName {
            "floorp-workspaces".into()
        }
        fn is_read_only(&self) -> bool {
            self.read_only
        }
        fn stage_incoming(
            &self,
            _: Vec<IncomingBso>,
            _: &mut telemetry::Engine,
        ) -> anyhow::Result<()> {
            unreachable!()
        }
        fn apply(
            &self,
            _: ServerTimestamp,
            _: &mut telemetry::Engine,
        ) -> anyhow::Result<Vec<OutgoingBso>> {
            unreachable!()
        }
        fn set_uploaded(&self, _: ServerTimestamp, ids: Vec<Guid>) -> anyhow::Result<()> {
            self.acknowledged.set(ids.len());
            Ok(())
        }
        fn sync_finished(&self) -> anyhow::Result<()> {
            self.finished.set(true);
            Ok(())
        }
        fn get_collection_request(
            &self,
            _: ServerTimestamp,
        ) -> anyhow::Result<Option<CollectionRequest>> {
            unreachable!()
        }
        fn get_sync_assoc(&self) -> anyhow::Result<EngineSyncAssociation> {
            unreachable!()
        }
        fn reset(&self, _: &EngineSyncAssociation) -> anyhow::Result<()> {
            unreachable!()
        }
        fn wipe(&self) -> anyhow::Result<()> {
            unreachable!()
        }
    }
    #[test]
    fn readonly_transport_constructs_no_post_batch_commit_or_delete() {
        let engine = Engine {
            read_only: true,
            finished: Cell::new(false),
            acknowledged: Cell::new(99),
        };
        let writes = Cell::new(0);
        finish_sync(
            &engine,
            ServerTimestamp(100),
            vec![],
            &mut telemetry::Engine::new("floorp-workspaces"),
            |_| {
                writes.set(writes.get() + 1);
                panic!("read-only collection entered the HTTP write transport")
            },
        )
        .unwrap();
        assert_eq!(writes.get(), 0);
        assert_eq!(engine.acknowledged.get(), 0);
        assert!(engine.finished.get());
        let outgoing =
            OutgoingBso::from_content_with_id(serde_json::json!({"id":"unexpected"})).unwrap();
        assert!(
            finish_sync(
                &engine,
                ServerTimestamp(100),
                vec![outgoing],
                &mut telemetry::Engine::new("floorp-workspaces"),
                |_| panic!("must fail before transport")
            )
            .is_err()
        );
    }
    #[test]
    fn ordinary_engine_still_uses_upload_transport_and_server_ack() {
        let engine = Engine {
            read_only: false,
            finished: Cell::new(false),
            acknowledged: Cell::new(0),
        };
        let writes = Cell::new(0);
        finish_sync(
            &engine,
            ServerTimestamp(100),
            vec![],
            &mut telemetry::Engine::new("prefs"),
            |_| {
                writes.set(writes.get() + 1);
                Ok(UploadInfo {
                    successful_ids: vec![Guid::new("record")],
                    failed_ids: vec![],
                    modified_timestamp: ServerTimestamp(101),
                })
            },
        )
        .unwrap();
        assert_eq!(writes.get(), 1);
        assert_eq!(engine.acknowledged.get(), 1);
        assert!(engine.finished.get());
    }
}

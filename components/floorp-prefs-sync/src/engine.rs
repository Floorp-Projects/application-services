/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::{FloorpPrefsSyncError, Result};
use parking_lot::Mutex;
use serde::de::{Error as _, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::{to_raw_value, RawValue};
use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, OnceLock, Weak};
use sync15::bso::{IncomingBso, OutgoingBso, OutgoingEnvelope};
use sync15::engine::{
    CollSyncIds, CollectionRequest, EngineSyncAssociation, SyncEngine, SyncEngineId,
};
use sync15::{telemetry, CollectionName, Guid, ServerTimestamp};

pub const PREFS_COLLECTION_NAME: &str = "prefs";
pub const TRANSPORT_CONTRACT_VERSION: &str = "floorp-prefs-sync-v2-padded-record-id";
// Desktop derives this by Base64URL-encoding Firefox's application ID. Gecko's
// encoder keeps padding by default, so the trailing `=` is part of the Sync
// record ID and must not be stripped.
pub const PREFS_RECORD_ID: &str = "e2VjODAzMGY3LWMyMGEtNDY0Zi05YjBlLTEzYTNhOWU5NzM4NH0=";
pub const NOTES_PREF_NAME: &str = "floorp.browser.note.memos";
pub const CONTROL_PREF_NAME: &str = "services.sync.prefs.sync.floorp.browser.note.memos";

// Sync's default encrypted-payload limit is 256 KiB. A 180 KiB cleartext
// record leaves room for base64 expansion and the encrypted payload envelope.
const MAX_CLEARTEXT_RECORD_BYTES: usize = 180 * 1024;
// Leave additional room for the aggregate map, JSON string escaping and keys.
const MAX_NOTES_VALUE_BYTES: usize = 164 * 1024;
const MAX_INCOMING_CLEARTEXT_BYTES: usize = 2 * 1024 * 1024;
const MAX_TRANSACTION_TOKEN_BYTES: usize = 4 * 1024;

#[derive(Clone, Default, PartialEq, Eq, uniffi::Record)]
pub struct FloorpPrefsSyncState {
    pub global_sync_id: Option<String>,
    pub collection_sync_id: Option<String>,
    pub last_modified_millis: i64,
}

#[derive(Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FloorpPrefsRemoteNotes {
    RecordMissing,
    NotesKeyMissing,
    NotesNull,
    NotesString { value: String },
}

#[derive(Clone, PartialEq, Eq, uniffi::Record)]
pub struct FloorpPrefsSyncPrepareInput {
    pub remote_notes: FloorpPrefsRemoteNotes,
    pub remote_record_modified_millis: Option<i64>,
    pub collection_modified_millis: i64,
    /// Maximum byte length of the Notes string after encoding it as a JSON
    /// string value, including its surrounding quote bytes.
    pub maximum_notes_value_bytes: u64,
}

#[derive(Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FloorpPrefsSyncPlan {
    NoUpload {
        transaction_token: Vec<u8>,
    },
    Upload {
        transaction_token: Vec<u8>,
        notes_value: String,
    },
}

#[derive(Clone, PartialEq, Eq, uniffi::Record)]
pub struct FloorpPrefsSyncFinish {
    pub transaction_token: Vec<u8>,
    pub did_upload: bool,
    pub server_modified_millis: i64,
}

/// Synchronous boundary implemented by the embedding application.
///
/// The Swift implementation must use a thread-safe snapshot/transaction
/// adapter. It must not block this callback waiting for a Swift actor running
/// on the same executor. `prepare` returns an opaque store token; Rust returns
/// it only from `sync_finished`, after Sync 1.5 has confirmed any upload.
/// Callbacks may inspect `sync_state`, but must not synchronously re-enter a
/// prefs Sync/reset/disconnect operation before returning.
#[uniffi::export(with_foreign)]
pub trait FloorpPrefsSyncDelegate: Send + Sync + 'static {
    fn prepare(&self, input: FloorpPrefsSyncPrepareInput) -> Result<FloorpPrefsSyncPlan>;
    fn sync_finished(&self, finish: FloorpPrefsSyncFinish) -> Result<()>;
    fn sync_state_changed(&self, state: FloorpPrefsSyncState) -> Result<()>;
    fn association_reset(&self, state: FloorpPrefsSyncState) -> Result<()>;
}

#[derive(Clone, PartialEq, Eq)]
struct StoredSyncState {
    association: EngineSyncAssociation,
    last_modified: ServerTimestamp,
    revision: u64,
}

impl StoredSyncState {
    fn from_ffi(state: Option<FloorpPrefsSyncState>) -> Result<Self> {
        let state = state.unwrap_or_default();
        if state.last_modified_millis < 0 {
            return Err(FloorpPrefsSyncError::InvalidPersistedState);
        }
        let association = match (state.global_sync_id, state.collection_sync_id) {
            (None, None) => EngineSyncAssociation::Disconnected,
            (Some(global), Some(collection)) => {
                let global = Guid::new(&global);
                let collection = Guid::new(&collection);
                if !global.is_valid_for_sync_server() || !collection.is_valid_for_sync_server() {
                    return Err(FloorpPrefsSyncError::InvalidPersistedState);
                }
                EngineSyncAssociation::Connected(CollSyncIds {
                    global,
                    coll: collection,
                })
            }
            _ => return Err(FloorpPrefsSyncError::InvalidPersistedState),
        };
        Ok(Self {
            association,
            last_modified: ServerTimestamp(state.last_modified_millis),
            revision: 0,
        })
    }

    fn to_ffi(&self) -> FloorpPrefsSyncState {
        let (global_sync_id, collection_sync_id) = match &self.association {
            EngineSyncAssociation::Disconnected => (None, None),
            EngineSyncAssociation::Connected(ids) => {
                (Some(ids.global.to_string()), Some(ids.coll.to_string()))
            }
        };
        FloorpPrefsSyncState {
            global_sync_id,
            collection_sync_id,
            last_modified_millis: self.last_modified.as_millis(),
        }
    }
}

#[derive(uniffi::Object)]
pub struct FloorpPrefsSyncStore {
    delegate: Arc<dyn FloorpPrefsSyncDelegate>,
    state: Mutex<StoredSyncState>,
    transition: Mutex<()>,
}

#[uniffi::export]
impl FloorpPrefsSyncStore {
    #[uniffi::constructor]
    pub fn new(
        delegate: Arc<dyn FloorpPrefsSyncDelegate>,
        initial_state: Option<FloorpPrefsSyncState>,
    ) -> Result<Self> {
        Ok(Self {
            delegate,
            state: Mutex::new(StoredSyncState::from_ffi(initial_state)?),
            transition: Mutex::new(()),
        })
    }

    pub fn sync_state(&self) -> FloorpPrefsSyncState {
        self.state.lock().to_ffi()
    }

    pub fn register_with_sync_manager(self: Arc<Self>) {
        *registered_store().lock() = Arc::downgrade(&self);
    }
}

fn registered_store() -> &'static Mutex<Weak<FloorpPrefsSyncStore>> {
    static STORE: OnceLock<Mutex<Weak<FloorpPrefsSyncStore>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(Weak::new()))
}

pub fn get_registered_sync_engine(engine_id: &SyncEngineId) -> Option<Box<dyn SyncEngine>> {
    match engine_id {
        SyncEngineId::Prefs => registered_store()
            .lock()
            .upgrade()
            .map(|store| Box::new(FloorpPrefsEngine::new(store)) as Box<dyn SyncEngine>),
        _ => unreachable!("can't provide unknown engine: {}", engine_id),
    }
}

#[derive(Default)]
struct PreservedValueMap {
    entries: Vec<(String, Box<RawValue>)>,
}

impl PreservedValueMap {
    fn get(&self, key: &str) -> Option<&RawValue> {
        self.entries
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value.as_ref())
    }

    fn insert(&mut self, key: String, value: Box<RawValue>) {
        if let Some((_, existing)) = self
            .entries
            .iter_mut()
            .find(|(candidate, _)| candidate == &key)
        {
            *existing = value;
        } else {
            self.entries.push((key, value));
        }
    }
}

impl<'de> Deserialize<'de> for PreservedValueMap {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PreservedValueMapVisitor;

        impl<'de> Visitor<'de> for PreservedValueMapVisitor {
            type Value = PreservedValueMap;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object containing preference entries")
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut entries = Vec::with_capacity(map.size_hint().unwrap_or(0));
                let mut seen = HashSet::new();
                while let Some((key, value)) = map.next_entry::<String, Box<RawValue>>()? {
                    if !seen.insert(key.clone()) {
                        return Err(A::Error::custom("duplicate preference entry"));
                    }
                    entries.push((key, value));
                }
                Ok(PreservedValueMap { entries })
            }
        }

        deserializer.deserialize_map(PreservedValueMapVisitor)
    }
}

impl Serialize for PreservedValueMap {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.entries.len()))?;
        for (key, value) in &self.entries {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

#[derive(Serialize)]
struct PrefsPayload<T: Serialize> {
    id: &'static str,
    value: T,
}

struct ValueMapWithNotes<'a> {
    source: &'a PreservedValueMap,
    notes_value: &'a str,
}

impl Serialize for ValueMapWithNotes<'_> {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let has_control = self.source.get(CONTROL_PREF_NAME).is_some();
        let has_notes = self.source.get(NOTES_PREF_NAME).is_some();
        let entry_count =
            self.source.entries.len() + usize::from(!has_control) + usize::from(!has_notes);
        let mut map = serializer.serialize_map(Some(entry_count))?;
        for (key, value) in &self.source.entries {
            match key.as_str() {
                CONTROL_PREF_NAME => map.serialize_entry(key, &true)?,
                NOTES_PREF_NAME => map.serialize_entry(key, self.notes_value)?,
                _ => map.serialize_entry(key, value)?,
            }
        }
        if !has_control {
            map.serialize_entry(CONTROL_PREF_NAME, &true)?;
        }
        if !has_notes {
            map.serialize_entry(NOTES_PREF_NAME, self.notes_value)?;
        }
        map.end()
    }
}

enum RemoteAggregate {
    Missing,
    Present {
        value: PreservedValueMap,
        modified: ServerTimestamp,
    },
}

struct PreparedTransaction {
    transaction_token: Vec<u8>,
    expected_upload: bool,
    upload_timestamp: Option<ServerTimestamp>,
    state_revision: u64,
}

#[derive(Default)]
struct EngineSession {
    run_state_revision: Option<u64>,
    staging_started: bool,
    target_seen: bool,
    remote: Option<RemoteAggregate>,
    prepared: Option<PreparedTransaction>,
}

struct FloorpPrefsEngine {
    store: Arc<FloorpPrefsSyncStore>,
    session: Mutex<EngineSession>,
}

impl FloorpPrefsEngine {
    fn new(store: Arc<FloorpPrefsSyncStore>) -> Self {
        Self {
            store,
            session: Mutex::new(EngineSession::default()),
        }
    }

    fn ensure_run_state_revision(&self) {
        if self.session.lock().run_state_revision.is_some() {
            return;
        }
        let _transition = self.store.transition.lock();
        let revision = self.store.state.lock().revision;
        let mut session = self.session.lock();
        if session.run_state_revision.is_none() {
            session.run_state_revision = Some(revision);
        }
    }

    fn parse_target_record(incoming: IncomingBso) -> Result<RemoteAggregate> {
        if incoming.payload.len() > MAX_INCOMING_CLEARTEXT_BYTES {
            return Err(FloorpPrefsSyncError::PayloadTooLarge);
        }
        let root: PreservedValueMap = serde_json::from_str(&incoming.payload)
            .map_err(|_| FloorpPrefsSyncError::InvalidIncomingRecord)?;

        if let Some(deleted) = root.get("deleted") {
            if serde_json::from_str::<bool>(deleted.get()).ok() == Some(true) {
                return Ok(RemoteAggregate::Missing);
            }
            return Err(FloorpPrefsSyncError::InvalidIncomingRecord);
        }
        if let Some(id) = root.get("id") {
            if serde_json::from_str::<String>(id.get()).ok().as_deref() != Some(PREFS_RECORD_ID) {
                return Err(FloorpPrefsSyncError::InvalidIncomingRecord);
            }
        }
        let value: PreservedValueMap = root
            .get("value")
            .ok_or(FloorpPrefsSyncError::InvalidIncomingRecord)
            .and_then(|value| {
                serde_json::from_str(value.get())
                    .map_err(|_| FloorpPrefsSyncError::InvalidIncomingRecord)
            })?;
        if let Some(notes) = value.get(NOTES_PREF_NAME) {
            Self::decode_notes_value(notes)?;
        }
        Ok(RemoteAggregate::Present {
            value,
            modified: incoming.envelope.modified,
        })
    }

    fn decode_notes_value(value: &RawValue) -> Result<Option<String>> {
        serde_json::from_str(value.get()).map_err(|_| FloorpPrefsSyncError::InvalidRemoteNotesValue)
    }

    fn prepare_input(
        remote: &RemoteAggregate,
        timestamp: ServerTimestamp,
    ) -> Result<FloorpPrefsSyncPrepareInput> {
        let maximum_notes_value_bytes = Self::maximum_notes_value_bytes(remote)?;
        let (remote_notes, remote_record_modified_millis) = match remote {
            RemoteAggregate::Missing => (FloorpPrefsRemoteNotes::RecordMissing, None),
            RemoteAggregate::Present { value, modified } => {
                let notes = match value.get(NOTES_PREF_NAME) {
                    None => FloorpPrefsRemoteNotes::NotesKeyMissing,
                    Some(value) => match Self::decode_notes_value(value)? {
                        None => FloorpPrefsRemoteNotes::NotesNull,
                        Some(value) => FloorpPrefsRemoteNotes::NotesString { value },
                    },
                };
                (notes, Some(modified.as_millis()))
            }
        };
        Ok(FloorpPrefsSyncPrepareInput {
            remote_notes,
            remote_record_modified_millis,
            collection_modified_millis: timestamp.as_millis(),
            maximum_notes_value_bytes: maximum_notes_value_bytes as u64,
        })
    }

    fn maximum_notes_value_bytes(remote: &RemoteAggregate) -> Result<usize> {
        let empty = PreservedValueMap::default();
        let source = match remote {
            RemoteAggregate::Missing => &empty,
            RemoteAggregate::Present { value, .. } => value,
        };
        let baseline = serde_json::to_vec(&PrefsPayload {
            id: PREFS_RECORD_ID,
            value: ValueMapWithNotes {
                source,
                notes_value: "",
            },
        })
        .map_err(|_| FloorpPrefsSyncError::InvalidIncomingRecord)?
        .len();

        // The baseline includes an encoded empty Notes string (`""`), the
        // forced control value, all unknown entries, and record framing.
        // Replace those two quote bytes with the delegate's fully JSON-encoded
        // string value to get an exact, escaping-aware payload budget.
        let non_notes_framing = baseline
            .checked_sub(2)
            .ok_or(FloorpPrefsSyncError::InvalidIncomingRecord)?;
        let serialized_value_budget = MAX_CLEARTEXT_RECORD_BYTES.saturating_sub(non_notes_framing);
        Ok((MAX_NOTES_VALUE_BYTES + 2).min(serialized_value_budget))
    }

    fn outgoing_record(value: PreservedValueMap) -> Result<OutgoingBso> {
        // `from_content` first converts through `serde_json::Value`, which
        // would normalize the raw unknown preference values we deliberately
        // carry verbatim. Serialize the Desktop-compatible inner `id`
        // explicitly through the lower-level envelope-preserving path.
        let outgoing = OutgoingBso::new(
            OutgoingEnvelope {
                id: Guid::new(PREFS_RECORD_ID),
                ..Default::default()
            },
            &PrefsPayload {
                id: PREFS_RECORD_ID,
                value,
            },
        )
        .map_err(|_| FloorpPrefsSyncError::InvalidPreparation)?;
        if outgoing.payload.len() > MAX_CLEARTEXT_RECORD_BYTES {
            return Err(FloorpPrefsSyncError::PayloadTooLarge);
        }
        Ok(outgoing)
    }

    fn finish_transaction(
        &self,
        prepared: PreparedTransaction,
        timestamp: ServerTimestamp,
    ) -> Result<()> {
        // Serialize all foreign persistence callbacks for this store. The
        // state mutex itself is intentionally not held across a callback, so
        // callback implementations may inspect `sync_state` safely.
        let _transition = self.store.transition.lock();
        let current = self.store.state.lock().clone();
        if current.revision != prepared.state_revision {
            return Err(FloorpPrefsSyncError::UnexpectedSyncState);
        }
        let revision = current
            .revision
            .checked_add(1)
            .ok_or(FloorpPrefsSyncError::UnexpectedSyncState)?;
        let next = StoredSyncState {
            association: current.association.clone(),
            last_modified: timestamp,
            revision,
        };

        self.store.delegate.sync_finished(FloorpPrefsSyncFinish {
            transaction_token: prepared.transaction_token,
            did_upload: prepared.expected_upload,
            server_modified_millis: timestamp.as_millis(),
        })?;

        // The opaque transaction is now committed and must never be reused,
        // even if persisting the Sync timestamp fails afterwards.
        self.store.state.lock().revision = revision;
        self.store.delegate.sync_state_changed(next.to_ffi())?;
        *self.store.state.lock() = next;
        Ok(())
    }

    fn replace_association(&self, association: &EngineSyncAssociation) -> Result<()> {
        let _transition = self.store.transition.lock();
        let current = self.store.state.lock().clone();
        let revision = current
            .revision
            .checked_add(1)
            .ok_or(FloorpPrefsSyncError::UnexpectedSyncState)?;
        let next = StoredSyncState {
            association: association.clone(),
            last_modified: ServerTimestamp(0),
            revision,
        };
        self.store.delegate.association_reset(next.to_ffi())?;
        *self.store.state.lock() = next;
        Ok(())
    }
}

impl SyncEngine for FloorpPrefsEngine {
    fn collection_name(&self) -> CollectionName {
        PREFS_COLLECTION_NAME.into()
    }

    fn prepare_for_sync(
        &self,
        _get_client_data: &dyn Fn() -> sync15::ClientData,
    ) -> anyhow::Result<()> {
        // A previous run might have stopped before `sync_finished` because of
        // cancellation or a transport/upload failure. Never carry its staged
        // aggregate or opaque transaction into the next run.
        let _transition = self.store.transition.lock();
        let revision = self.store.state.lock().revision;
        *self.session.lock() = EngineSession {
            run_state_revision: Some(revision),
            ..EngineSession::default()
        };
        Ok(())
    }

    fn stage_incoming(
        &self,
        inbound: Vec<IncomingBso>,
        telem: &mut telemetry::Engine,
    ) -> anyhow::Result<()> {
        // Tests and non-SyncManager callers may not invoke
        // `prepare_for_sync`; staging still needs a revision guard.
        self.ensure_run_state_revision();
        let mut incoming_telem = telemetry::EngineIncoming::new();
        let mut session = self.session.lock();
        if session.prepared.is_some() {
            return Err(FloorpPrefsSyncError::UnexpectedSyncState.into());
        }
        session.staging_started = true;

        for incoming in inbound {
            if incoming.envelope.id.as_str() != PREFS_RECORD_ID {
                continue;
            }
            if session.target_seen {
                incoming_telem.failed(1);
                telem.incoming(incoming_telem);
                return Err(FloorpPrefsSyncError::DuplicateTargetRecord.into());
            }
            session.target_seen = true;
            match Self::parse_target_record(incoming) {
                Ok(remote) => {
                    session.remote = Some(remote);
                    incoming_telem.applied(1);
                }
                Err(error) => {
                    incoming_telem.failed(1);
                    telem.incoming(incoming_telem);
                    return Err(error.into());
                }
            }
        }
        telem.incoming(incoming_telem);
        Ok(())
    }

    fn apply(
        &self,
        timestamp: ServerTimestamp,
        _telem: &mut telemetry::Engine,
    ) -> anyhow::Result<Vec<OutgoingBso>> {
        let (remote, state_revision) = {
            let mut session = self.session.lock();
            if !session.staging_started || session.prepared.is_some() {
                return Err(FloorpPrefsSyncError::UnexpectedSyncState.into());
            }
            let state_revision = session
                .run_state_revision
                .ok_or(FloorpPrefsSyncError::UnexpectedSyncState)?;
            (
                session.remote.take().unwrap_or(RemoteAggregate::Missing),
                state_revision,
            )
        };

        let input = Self::prepare_input(&remote, timestamp)?;
        let maximum_notes_value_bytes = input.maximum_notes_value_bytes as usize;
        if let FloorpPrefsRemoteNotes::NotesString { value } = &input.remote_notes {
            if value.len() > MAX_NOTES_VALUE_BYTES {
                return Err(FloorpPrefsSyncError::PayloadTooLarge.into());
            }
        }
        let plan = {
            // Preparing a Swift-side transaction and capturing the Rust-side
            // association revision are one logical transition. In particular,
            // an account reset must not interleave between these operations.
            let _transition = self.store.transition.lock();
            if self.store.state.lock().revision != state_revision {
                return Err(FloorpPrefsSyncError::UnexpectedSyncState.into());
            }
            self.store.delegate.prepare(input)?
        };
        let (transaction_token, notes_value) = match plan {
            FloorpPrefsSyncPlan::NoUpload { transaction_token } => (transaction_token, None),
            FloorpPrefsSyncPlan::Upload {
                transaction_token,
                notes_value,
            } => (transaction_token, Some(notes_value)),
        };
        if transaction_token.is_empty() || transaction_token.len() > MAX_TRANSACTION_TOKEN_BYTES {
            return Err(FloorpPrefsSyncError::InvalidPreparation.into());
        }
        if let Some(value) = notes_value.as_ref() {
            let serialized_value_bytes = serde_json::to_vec(value)
                .map_err(|_| FloorpPrefsSyncError::InvalidPreparation)?
                .len();
            if serialized_value_bytes > maximum_notes_value_bytes {
                return Err(FloorpPrefsSyncError::PayloadTooLarge.into());
            }
        }

        let mut aggregate = match remote {
            RemoteAggregate::Missing => PreservedValueMap::default(),
            RemoteAggregate::Present { value, .. } => value,
        };
        let mut needs_upload = aggregate
            .get(CONTROL_PREF_NAME)
            .is_none_or(|value| serde_json::from_str::<bool>(value.get()).ok() != Some(true));
        aggregate.insert(
            CONTROL_PREF_NAME.to_string(),
            to_raw_value(&true).map_err(|_| FloorpPrefsSyncError::InvalidPreparation)?,
        );
        if let Some(notes_value) = notes_value {
            let existing_notes = aggregate
                .get(NOTES_PREF_NAME)
                .and_then(|value| serde_json::from_str::<String>(value.get()).ok());
            if existing_notes.as_deref() != Some(&notes_value) {
                aggregate.insert(
                    NOTES_PREF_NAME.to_string(),
                    to_raw_value(&notes_value)
                        .map_err(|_| FloorpPrefsSyncError::InvalidPreparation)?,
                );
                needs_upload = true;
            }
        }

        let outgoing = if needs_upload {
            vec![Self::outgoing_record(aggregate)?]
        } else {
            Vec::new()
        };
        self.session.lock().prepared = Some(PreparedTransaction {
            transaction_token,
            expected_upload: needs_upload,
            upload_timestamp: None,
            state_revision,
        });
        Ok(outgoing)
    }

    fn set_uploaded(&self, new_timestamp: ServerTimestamp, ids: Vec<Guid>) -> anyhow::Result<()> {
        let mut session = self.session.lock();
        let prepared = session
            .prepared
            .as_mut()
            .ok_or(FloorpPrefsSyncError::UnexpectedSyncState)?;
        let upload_was_confirmed = if prepared.expected_upload {
            ids.len() == 1 && ids[0].as_str() == PREFS_RECORD_ID
        } else {
            ids.is_empty()
        };
        if !upload_was_confirmed {
            return Err(FloorpPrefsSyncError::UploadNotConfirmed.into());
        }
        prepared.upload_timestamp = Some(new_timestamp);
        Ok(())
    }

    fn sync_finished(&self) -> anyhow::Result<()> {
        let prepared = {
            let mut session = self.session.lock();
            let prepared = session
                .prepared
                .take()
                .ok_or(FloorpPrefsSyncError::UnexpectedSyncState)?;
            // A SyncEngine may be reused. Clear the staged-record and duplicate
            // detection state before crossing the foreign callback boundary.
            *session = EngineSession::default();
            prepared
        };
        let finished_timestamp = prepared
            .upload_timestamp
            .ok_or(FloorpPrefsSyncError::UploadNotConfirmed)?;
        self.finish_transaction(prepared, finished_timestamp)?;
        Ok(())
    }

    fn get_collection_request(
        &self,
        _server_timestamp: ServerTimestamp,
    ) -> anyhow::Result<Option<CollectionRequest>> {
        // `synchronize` callers do not necessarily invoke
        // `prepare_for_sync`, but every run asks for its collection request.
        self.ensure_run_state_revision();
        // Always fetch the singleton aggregate. Local Notes can change while
        // the collection timestamp stays still, and a fresh copy is required
        // to preserve preference entries owned by Desktop and other products.
        Ok(Some(
            CollectionRequest::new(PREFS_COLLECTION_NAME.into())
                .full()
                .ids([Guid::new(PREFS_RECORD_ID)]),
        ))
    }

    fn get_sync_assoc(&self) -> anyhow::Result<EngineSyncAssociation> {
        Ok(self.store.state.lock().association.clone())
    }

    fn reset(&self, assoc: &EngineSyncAssociation) -> anyhow::Result<()> {
        self.replace_association(assoc)?;
        *self.session.lock() = EngineSession::default();
        Ok(())
    }

    fn wipe(&self) -> anyhow::Result<()> {
        // Disconnecting Sync must not delete the user's local Notes.
        self.reset(&EngineSyncAssociation::Disconnected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::collections::VecDeque;
    use sync15::bso::IncomingEnvelope;

    #[derive(Default)]
    struct FakeDelegate {
        plans: Mutex<VecDeque<FloorpPrefsSyncPlan>>,
        inputs: Mutex<Vec<FloorpPrefsSyncPrepareInput>>,
        finishes: Mutex<Vec<FloorpPrefsSyncFinish>>,
        state_changes: Mutex<Vec<FloorpPrefsSyncState>>,
        association_resets: Mutex<Vec<FloorpPrefsSyncState>>,
        reject_finish: Mutex<bool>,
    }

    impl FloorpPrefsSyncDelegate for FakeDelegate {
        fn prepare(&self, input: FloorpPrefsSyncPrepareInput) -> Result<FloorpPrefsSyncPlan> {
            self.inputs.lock().push(input);
            self.plans
                .lock()
                .pop_front()
                .ok_or(FloorpPrefsSyncError::DelegateRejected)
        }

        fn sync_finished(&self, finish: FloorpPrefsSyncFinish) -> Result<()> {
            if *self.reject_finish.lock() {
                return Err(FloorpPrefsSyncError::DelegateRejected);
            }
            self.finishes.lock().push(finish);
            Ok(())
        }

        fn sync_state_changed(&self, state: FloorpPrefsSyncState) -> Result<()> {
            self.state_changes.lock().push(state);
            Ok(())
        }

        fn association_reset(&self, state: FloorpPrefsSyncState) -> Result<()> {
            self.association_resets.lock().push(state);
            Ok(())
        }
    }

    fn no_upload(token: &[u8]) -> FloorpPrefsSyncPlan {
        FloorpPrefsSyncPlan::NoUpload {
            transaction_token: token.to_vec(),
        }
    }

    fn upload(token: &[u8], notes_value: &str) -> FloorpPrefsSyncPlan {
        FloorpPrefsSyncPlan::Upload {
            transaction_token: token.to_vec(),
            notes_value: notes_value.to_string(),
        }
    }

    fn test_engine(
        plan: FloorpPrefsSyncPlan,
    ) -> (
        Arc<FakeDelegate>,
        Arc<FloorpPrefsSyncStore>,
        FloorpPrefsEngine,
    ) {
        let delegate = Arc::new(FakeDelegate::default());
        delegate.plans.lock().push_back(plan);
        let delegate_for_store: Arc<dyn FloorpPrefsSyncDelegate> = delegate.clone();
        let store = Arc::new(FloorpPrefsSyncStore::new(delegate_for_store, None).unwrap());
        let engine = FloorpPrefsEngine::new(store.clone());
        (delegate, store, engine)
    }

    fn incoming(id: &str, modified: i64, payload: Value) -> IncomingBso {
        IncomingBso::new(
            IncomingEnvelope {
                id: Guid::new(id),
                modified: ServerTimestamp(modified),
                sortindex: None,
                ttl: None,
            },
            payload.to_string(),
        )
    }

    fn target(modified: i64, value: Value) -> IncomingBso {
        incoming(
            PREFS_RECORD_ID,
            modified,
            serde_json::json!({ "id": PREFS_RECORD_ID, "value": value }),
        )
    }

    fn raw_target(modified: i64, payload: &str) -> IncomingBso {
        IncomingBso::new(
            IncomingEnvelope {
                id: Guid::new(PREFS_RECORD_ID),
                modified: ServerTimestamp(modified),
                sortindex: None,
                ttl: None,
            },
            payload.to_string(),
        )
    }

    fn stage(engine: &FloorpPrefsEngine, inbound: Vec<IncomingBso>) -> anyhow::Result<()> {
        engine.stage_incoming(inbound, &mut telemetry::Engine::new(PREFS_COLLECTION_NAME))
    }

    fn apply(engine: &FloorpPrefsEngine, timestamp: i64) -> anyhow::Result<Vec<OutgoingBso>> {
        engine.apply(
            ServerTimestamp(timestamp),
            &mut telemetry::Engine::new(PREFS_COLLECTION_NAME),
        )
    }

    fn is_error(error: &anyhow::Error, predicate: impl FnOnce(&FloorpPrefsSyncError) -> bool) {
        assert!(
            error
                .downcast_ref::<FloorpPrefsSyncError>()
                .is_some_and(predicate),
            "unexpected error type"
        );
    }

    #[test]
    fn incoming_notes_states_are_typed_without_conflation() {
        error_support::init_for_tests();
        let cases = [
            (None, FloorpPrefsRemoteNotes::RecordMissing, None),
            (
                Some(serde_json::json!({ CONTROL_PREF_NAME: true })),
                FloorpPrefsRemoteNotes::NotesKeyMissing,
                Some(41),
            ),
            (
                Some(serde_json::json!({
                    CONTROL_PREF_NAME: true,
                    NOTES_PREF_NAME: null,
                })),
                FloorpPrefsRemoteNotes::NotesNull,
                Some(41),
            ),
            (
                Some(serde_json::json!({
                    CONTROL_PREF_NAME: true,
                    NOTES_PREF_NAME: "desktop-json",
                })),
                FloorpPrefsRemoteNotes::NotesString {
                    value: "desktop-json".to_string(),
                },
                Some(41),
            ),
        ];

        for (remote, expected_notes, expected_modified) in cases {
            let (delegate, _store, engine) = test_engine(no_upload(b"prepared"));
            let inbound = remote
                .map(|value| vec![target(41, value)])
                .unwrap_or_default();
            stage(&engine, inbound).unwrap();
            apply(&engine, 50).unwrap();
            let inputs = delegate.inputs.lock();
            assert!(inputs[0].remote_notes == expected_notes);
            assert!(inputs[0].remote_record_modified_millis == expected_modified);
            assert!(inputs[0].collection_modified_millis == 50);
            assert!(inputs[0].maximum_notes_value_bytes == (MAX_NOTES_VALUE_BYTES + 2) as u64);
        }
    }

    #[test]
    fn invalid_remote_notes_types_and_invalid_aggregate_are_rejected() {
        error_support::init_for_tests();
        for invalid in [
            serde_json::json!(false),
            serde_json::json!(7),
            serde_json::json!(["not", "a", "string"]),
            serde_json::json!({ "unexpected": true }),
        ] {
            let (_delegate, _store, engine) = test_engine(no_upload(b"prepared"));
            let error = stage(
                &engine,
                vec![target(41, serde_json::json!({ NOTES_PREF_NAME: invalid }))],
            )
            .unwrap_err();
            is_error(&error, |error| {
                matches!(error, FloorpPrefsSyncError::InvalidRemoteNotesValue)
            });
        }

        let (_delegate, _store, engine) = test_engine(no_upload(b"prepared"));
        let error = stage(
            &engine,
            vec![incoming(
                PREFS_RECORD_ID,
                41,
                serde_json::json!({ "id": PREFS_RECORD_ID }),
            )],
        )
        .unwrap_err();
        is_error(&error, |error| {
            matches!(error, FloorpPrefsSyncError::InvalidIncomingRecord)
        });

        for deleted in [
            serde_json::json!(false),
            serde_json::json!(null),
            serde_json::json!("not-a-tombstone"),
        ] {
            let (_delegate, _store, engine) = test_engine(no_upload(b"prepared"));
            let error = stage(
                &engine,
                vec![incoming(
                    PREFS_RECORD_ID,
                    41,
                    serde_json::json!({ "deleted": deleted }),
                )],
            )
            .unwrap_err();
            is_error(&error, |error| {
                matches!(error, FloorpPrefsSyncError::InvalidIncomingRecord)
            });
        }
    }

    #[test]
    fn other_application_records_are_ignored_before_payload_parsing() {
        error_support::init_for_tests();
        let (delegate, _store, engine) = test_engine(no_upload(b"prepared"));
        stage(
            &engine,
            vec![IncomingBso::new(
                IncomingEnvelope {
                    id: Guid::new("another-application-record"),
                    modified: ServerTimestamp(12),
                    sortindex: None,
                    ttl: None,
                },
                "not-json-and-must-not-be-inspected".to_string(),
            )],
        )
        .unwrap();
        apply(&engine, 20).unwrap();
        assert!(matches!(
            delegate.inputs.lock()[0].remote_notes,
            FloorpPrefsRemoteNotes::RecordMissing
        ));
    }

    #[test]
    fn duplicate_target_records_fail_closed() {
        error_support::init_for_tests();
        let (_delegate, _store, engine) = test_engine(no_upload(b"prepared"));
        let value = serde_json::json!({ CONTROL_PREF_NAME: true });
        let error = stage(&engine, vec![target(40, value.clone()), target(41, value)]).unwrap_err();
        is_error(&error, |error| {
            matches!(error, FloorpPrefsSyncError::DuplicateTargetRecord)
        });
    }

    #[test]
    fn no_op_is_confirmed_without_an_upload() {
        error_support::init_for_tests();
        let (delegate, store, engine) = test_engine(no_upload(b"store-proof"));
        stage(
            &engine,
            vec![target(
                70,
                serde_json::json!({
                    CONTROL_PREF_NAME: true,
                    NOTES_PREF_NAME: "unchanged",
                }),
            )],
        )
        .unwrap();
        assert!(apply(&engine, 80).unwrap().is_empty());
        engine
            .set_uploaded(ServerTimestamp(80), Vec::new())
            .unwrap();
        engine.sync_finished().unwrap();

        let finishes = delegate.finishes.lock();
        assert!(finishes.len() == 1);
        assert!(finishes[0].transaction_token == b"store-proof");
        assert!(!finishes[0].did_upload);
        assert!(finishes[0].server_modified_millis == 80);
        assert!(store.sync_state().last_modified_millis == 80);
    }

    #[test]
    fn upload_success_preserves_the_complete_aggregate() {
        error_support::init_for_tests();
        let (delegate, _store, engine) = test_engine(upload(b"store-proof", "merged-notes"));
        let unknown = serde_json::json!({
            "nested": [1, true, null, { "future": "value" }],
        });
        stage(
            &engine,
            vec![target(
                90,
                serde_json::json!({
                    CONTROL_PREF_NAME: false,
                    NOTES_PREF_NAME: "old-notes",
                    "browser.future.preference": unknown,
                    "services.sync.prefs.sync.browser.future.preference": true,
                }),
            )],
        )
        .unwrap();
        let outgoing = apply(&engine, 100).unwrap();
        assert!(outgoing.len() == 1);
        assert!(outgoing[0].envelope.id.as_str() == PREFS_RECORD_ID);
        let payload: Value = serde_json::from_str(&outgoing[0].payload).unwrap();
        assert!(payload.get("id").and_then(Value::as_str) == Some(PREFS_RECORD_ID));
        let aggregate = payload.get("value").and_then(Value::as_object).unwrap();
        assert!(aggregate.get(NOTES_PREF_NAME) == Some(&Value::String("merged-notes".into())));
        assert!(aggregate.get(CONTROL_PREF_NAME) == Some(&Value::Bool(true)));
        assert!(aggregate.get("browser.future.preference") == Some(&unknown));
        assert!(
            aggregate.get("services.sync.prefs.sync.browser.future.preference")
                == Some(&Value::Bool(true))
        );

        engine
            .set_uploaded(ServerTimestamp(101), vec![Guid::new(PREFS_RECORD_ID)])
            .unwrap();
        engine.sync_finished().unwrap();
        assert!(delegate.finishes.lock()[0].did_upload);
        assert!(delegate.finishes.lock()[0].server_modified_millis == 101);
    }

    #[test]
    fn upload_preserves_unknown_values_without_reserializing_them() {
        error_support::init_for_tests();
        let (_delegate, _store, engine) = test_engine(no_upload(b"store-proof"));
        let unknown = r#"{"future-number":1.2300e+04, "nested" : [3, 2, 1]}"#;
        let payload = format!(
            r#"{{"id":"{PREFS_RECORD_ID}","value":{{"{CONTROL_PREF_NAME}":false,"future.preference":{unknown}}}}}"#
        );
        stage(&engine, vec![raw_target(90, &payload)]).unwrap();

        let outgoing = apply(&engine, 100).unwrap();
        assert!(outgoing.len() == 1);
        assert!(outgoing[0]
            .payload
            .contains(&format!(r#""future.preference":{unknown}"#)));
    }

    #[test]
    fn upload_failure_never_confirms_the_prepared_transaction() {
        error_support::init_for_tests();
        let (delegate, _store, engine) = test_engine(upload(b"store-proof", "merged-notes"));
        stage(&engine, Vec::new()).unwrap();
        assert!(apply(&engine, 110).unwrap().len() == 1);

        let error = engine
            .set_uploaded(ServerTimestamp(111), Vec::new())
            .unwrap_err();
        is_error(&error, |error| {
            matches!(error, FloorpPrefsSyncError::UploadNotConfirmed)
        });
        assert!(delegate.finishes.lock().is_empty());

        let error = engine.sync_finished().unwrap_err();
        is_error(&error, |error| {
            matches!(error, FloorpPrefsSyncError::UploadNotConfirmed)
        });
        assert!(delegate.finishes.lock().is_empty());
    }

    #[test]
    fn unexpected_upload_ids_fail_closed_for_upload_and_no_upload_plans() {
        error_support::init_for_tests();
        let (_delegate, _store, engine) = test_engine(upload(b"store-proof", "merged-notes"));
        stage(&engine, Vec::new()).unwrap();
        assert!(apply(&engine, 110).unwrap().len() == 1);
        let error = engine
            .set_uploaded(
                ServerTimestamp(111),
                vec![Guid::new(PREFS_RECORD_ID), Guid::new("unexpected-record")],
            )
            .unwrap_err();
        is_error(&error, |error| {
            matches!(error, FloorpPrefsSyncError::UploadNotConfirmed)
        });

        let (_delegate, _store, engine) = test_engine(no_upload(b"store-proof"));
        stage(
            &engine,
            vec![target(110, serde_json::json!({ CONTROL_PREF_NAME: true }))],
        )
        .unwrap();
        assert!(apply(&engine, 111).unwrap().is_empty());
        let error = engine
            .set_uploaded(ServerTimestamp(111), vec![Guid::new(PREFS_RECORD_ID)])
            .unwrap_err();
        is_error(&error, |error| {
            matches!(error, FloorpPrefsSyncError::UploadNotConfirmed)
        });
    }

    #[test]
    fn delegate_rejection_does_not_advance_the_sync_timestamp() {
        error_support::init_for_tests();
        let (delegate, store, engine) = test_engine(no_upload(b"store-proof"));
        *delegate.reject_finish.lock() = true;
        stage(
            &engine,
            vec![target(110, serde_json::json!({ CONTROL_PREF_NAME: true }))],
        )
        .unwrap();
        assert!(apply(&engine, 111).unwrap().is_empty());
        engine
            .set_uploaded(ServerTimestamp(111), Vec::new())
            .unwrap();

        let error = engine.sync_finished().unwrap_err();
        is_error(&error, |error| {
            matches!(error, FloorpPrefsSyncError::DelegateRejected)
        });
        assert!(delegate.finishes.lock().is_empty());
        assert!(delegate.state_changes.lock().is_empty());
        assert!(store.sync_state().last_modified_millis == 0);
    }

    #[test]
    fn a_successful_engine_session_can_be_reused() {
        error_support::init_for_tests();
        let (delegate, _store, engine) = test_engine(no_upload(b"first-proof"));
        delegate.plans.lock().push_back(no_upload(b"second-proof"));

        for (timestamp, token) in [(120, b"first-proof".as_slice()), (130, b"second-proof")] {
            stage(
                &engine,
                vec![target(
                    timestamp - 1,
                    serde_json::json!({ CONTROL_PREF_NAME: true }),
                )],
            )
            .unwrap();
            assert!(apply(&engine, timestamp).unwrap().is_empty());
            engine
                .set_uploaded(ServerTimestamp(timestamp), Vec::new())
                .unwrap();
            engine.sync_finished().unwrap();
            assert!(delegate.finishes.lock().last().unwrap().transaction_token == token);
        }

        assert!(delegate.inputs.lock().len() == 2);
        assert!(delegate.finishes.lock().len() == 2);
    }

    #[test]
    fn preparing_a_new_sync_discards_an_aborted_session() {
        error_support::init_for_tests();
        let (delegate, _store, engine) = test_engine(upload(b"aborted-proof", "first-notes"));
        delegate.plans.lock().push_back(no_upload(b"retry-proof"));
        stage(&engine, Vec::new()).unwrap();
        assert!(apply(&engine, 140).unwrap().len() == 1);

        engine
            .prepare_for_sync(&|| panic!("prefs engine must not request client data"))
            .unwrap();
        stage(
            &engine,
            vec![target(141, serde_json::json!({ CONTROL_PREF_NAME: true }))],
        )
        .unwrap();
        assert!(apply(&engine, 142).unwrap().is_empty());
        engine
            .set_uploaded(ServerTimestamp(142), Vec::new())
            .unwrap();
        engine.sync_finished().unwrap();

        let finishes = delegate.finishes.lock();
        assert!(finishes.len() == 1);
        assert!(finishes[0].transaction_token == b"retry-proof");
    }

    #[test]
    fn reset_invalidates_an_in_flight_transaction_before_confirmation() {
        error_support::init_for_tests();
        let (delegate, store, stale_engine) = test_engine(upload(b"stale-proof", "stale-notes"));
        stage(&stale_engine, Vec::new()).unwrap();
        assert!(apply(&stale_engine, 150).unwrap().len() == 1);

        let reset_engine = FloorpPrefsEngine::new(store);
        reset_engine
            .reset(&EngineSyncAssociation::Disconnected)
            .unwrap();
        stale_engine
            .set_uploaded(ServerTimestamp(151), vec![Guid::new(PREFS_RECORD_ID)])
            .unwrap();
        let error = stale_engine.sync_finished().unwrap_err();
        is_error(&error, |error| {
            matches!(error, FloorpPrefsSyncError::UnexpectedSyncState)
        });

        assert!(delegate.finishes.lock().is_empty());
        assert!(delegate.state_changes.lock().is_empty());
        assert!(delegate.association_resets.lock().len() == 1);
    }

    #[test]
    fn reset_during_download_invalidates_the_run_before_prepare() {
        error_support::init_for_tests();
        let (delegate, store, stale_engine) = test_engine(no_upload(b"stale-proof"));
        stale_engine
            .get_collection_request(ServerTimestamp(160))
            .unwrap();

        let reset_engine = FloorpPrefsEngine::new(store);
        reset_engine
            .reset(&EngineSyncAssociation::Disconnected)
            .unwrap();
        stage(
            &stale_engine,
            vec![target(161, serde_json::json!({ CONTROL_PREF_NAME: true }))],
        )
        .unwrap();
        let error = apply(&stale_engine, 162).unwrap_err();
        is_error(&error, |error| {
            matches!(error, FloorpPrefsSyncError::UnexpectedSyncState)
        });

        assert!(delegate.inputs.lock().is_empty());
        assert!(delegate.finishes.lock().is_empty());
    }

    #[test]
    fn only_one_concurrent_preparation_can_advance_the_store_revision() {
        error_support::init_for_tests();
        let (delegate, store, first_engine) = test_engine(no_upload(b"first-proof"));
        delegate.plans.lock().push_back(no_upload(b"stale-proof"));
        let second_engine = FloorpPrefsEngine::new(store);
        let remote = || vec![target(159, serde_json::json!({ CONTROL_PREF_NAME: true }))];

        stage(&first_engine, remote()).unwrap();
        assert!(apply(&first_engine, 160).unwrap().is_empty());
        stage(&second_engine, remote()).unwrap();
        assert!(apply(&second_engine, 160).unwrap().is_empty());
        first_engine
            .set_uploaded(ServerTimestamp(160), Vec::new())
            .unwrap();
        second_engine
            .set_uploaded(ServerTimestamp(160), Vec::new())
            .unwrap();

        first_engine.sync_finished().unwrap();
        let error = second_engine.sync_finished().unwrap_err();
        is_error(&error, |error| {
            matches!(error, FloorpPrefsSyncError::UnexpectedSyncState)
        });
        let finishes = delegate.finishes.lock();
        assert!(finishes.len() == 1);
        assert!(finishes[0].transaction_token == b"first-proof");
    }

    #[test]
    fn association_reset_and_disconnect_are_persisted_without_wiping_notes() {
        error_support::init_for_tests();
        let (delegate, store, engine) = test_engine(no_upload(b"prepared"));
        let connected = EngineSyncAssociation::Connected(CollSyncIds {
            global: Guid::new("global123456"),
            coll: Guid::new("prefs1234567"),
        });
        engine.reset(&connected).unwrap();
        let connected_state = store.sync_state();
        assert!(connected_state.global_sync_id.as_deref() == Some("global123456"));
        assert!(connected_state.collection_sync_id.as_deref() == Some("prefs1234567"));
        assert!(connected_state.last_modified_millis == 0);

        engine.wipe().unwrap();
        let disconnected_state = store.sync_state();
        assert!(disconnected_state.global_sync_id.is_none());
        assert!(disconnected_state.collection_sync_id.is_none());
        assert!(delegate.association_resets.lock().len() == 2);
    }

    #[test]
    fn payload_and_transaction_token_limits_fail_before_confirmation() {
        error_support::init_for_tests();
        let oversized_notes = "n".repeat(MAX_NOTES_VALUE_BYTES + 1);
        let (_delegate, _store, engine) = test_engine(no_upload(b"prepared"));
        stage(
            &engine,
            vec![target(
                10,
                serde_json::json!({
                    CONTROL_PREF_NAME: true,
                    NOTES_PREF_NAME: oversized_notes,
                }),
            )],
        )
        .unwrap();
        let error = apply(&engine, 11).unwrap_err();
        is_error(&error, |error| {
            matches!(error, FloorpPrefsSyncError::PayloadTooLarge)
        });

        // The raw string is below the old fixed limit, but its quote-heavy
        // outer JSON representation exceeds the advertised encoded budget.
        let quote_heavy = "\"".repeat(MAX_NOTES_VALUE_BYTES / 2 + 1);
        let (_delegate, _store, engine) = test_engine(upload(b"prepared", &quote_heavy));
        stage(&engine, Vec::new()).unwrap();
        let error = apply(&engine, 11).unwrap_err();
        is_error(&error, |error| {
            matches!(error, FloorpPrefsSyncError::PayloadTooLarge)
        });

        let oversized_upload = "u".repeat(MAX_NOTES_VALUE_BYTES + 1);
        let (_delegate, _store, engine) = test_engine(upload(b"prepared", &oversized_upload));
        stage(&engine, Vec::new()).unwrap();
        let error = apply(&engine, 11).unwrap_err();
        is_error(&error, |error| {
            matches!(error, FloorpPrefsSyncError::PayloadTooLarge)
        });

        let (_delegate, _store, engine) =
            test_engine(no_upload(&vec![0; MAX_TRANSACTION_TOKEN_BYTES + 1]));
        stage(&engine, Vec::new()).unwrap();
        let error = apply(&engine, 11).unwrap_err();
        is_error(&error, |error| {
            matches!(error, FloorpPrefsSyncError::InvalidPreparation)
        });
    }

    #[test]
    fn full_record_limit_includes_unknown_aggregate_entries() {
        error_support::init_for_tests();
        let (_delegate, _store, engine) = test_engine(no_upload(b"prepared"));
        stage(
            &engine,
            vec![target(
                10,
                serde_json::json!({
                    CONTROL_PREF_NAME: false,
                    "unknown-large-preference": "x".repeat(MAX_CLEARTEXT_RECORD_BYTES),
                }),
            )],
        )
        .unwrap();
        let error = apply(&engine, 11).unwrap_err();
        is_error(&error, |error| {
            matches!(error, FloorpPrefsSyncError::PayloadTooLarge)
        });
    }

    #[test]
    fn advertised_notes_budget_subtracts_preserved_unknown_entries() {
        error_support::init_for_tests();
        let (delegate, _store, engine) = test_engine(no_upload(b"prepared"));
        stage(
            &engine,
            vec![target(
                10,
                serde_json::json!({
                    CONTROL_PREF_NAME: true,
                    "unknown-large-preference": "x".repeat(120 * 1024),
                }),
            )],
        )
        .unwrap();
        assert!(apply(&engine, 11).unwrap().is_empty());

        let budget = delegate.inputs.lock()[0].maximum_notes_value_bytes;
        assert!(budget > 0);
        assert!(budget < (MAX_NOTES_VALUE_BYTES + 2) as u64);
    }

    #[test]
    fn collection_request_is_full_and_scoped_to_the_desktop_record() {
        error_support::init_for_tests();
        let (_delegate, _store, engine) = test_engine(no_upload(b"prepared"));
        let request = engine
            .get_collection_request(ServerTimestamp(123))
            .unwrap()
            .unwrap();
        assert!(request.collection.as_ref() == PREFS_COLLECTION_NAME);
        assert!(request.full);
        let ids = request.ids.unwrap();
        assert!(ids.len() == 1);
        assert_eq!(
            ids[0].as_str(),
            "e2VjODAzMGY3LWMyMGEtNDY0Zi05YjBlLTEzYTNhOWU5NzM4NH0="
        );
        assert_ne!(
            ids[0].as_str(),
            "e2VjODAzMGY3LWMyMGEtNDY0Zi05YjBlLTEzYTNhOWU5NzM4NH0"
        );
        assert!(request.newer.is_none());
    }

    #[test]
    fn desktop_record_id_keeps_base64url_padding() {
        assert_eq!(
            TRANSPORT_CONTRACT_VERSION,
            "floorp-prefs-sync-v2-padded-record-id"
        );
        assert_eq!(
            PREFS_RECORD_ID,
            "e2VjODAzMGY3LWMyMGEtNDY0Zi05YjBlLTEzYTNhOWU5NzM4NH0="
        );
        assert!(PREFS_RECORD_ID.ends_with('='));
    }

    #[test]
    fn malformed_persisted_association_is_rejected() {
        error_support::init_for_tests();
        let delegate: Arc<dyn FloorpPrefsSyncDelegate> = Arc::new(FakeDelegate::default());
        let result = FloorpPrefsSyncStore::new(
            delegate,
            Some(FloorpPrefsSyncState {
                global_sync_id: Some("global123456".to_string()),
                collection_sync_id: None,
                last_modified_millis: 0,
            }),
        );
        assert!(matches!(
            result,
            Err(FloorpPrefsSyncError::InvalidPersistedState)
        ));
    }

    #[test]
    fn failed_upload_retries_with_the_same_transaction_and_base_advances_only_after_confirmation() {
        error_support::init_for_tests();
        let (delegate, store, engine) = test_engine(upload(b"retry-proof", "merged-notes"));
        stage(&engine, Vec::new()).unwrap();
        let outgoing = apply(&engine, 200).unwrap();
        assert!(outgoing.len() == 1);

        // A transport/upload failure (unexpected upload ids) must not confirm
        // the prepared transaction nor advance the successful base.
        let error = engine
            .set_uploaded(
                ServerTimestamp(201),
                vec![Guid::new(PREFS_RECORD_ID), Guid::new("extra-record")],
            )
            .unwrap_err();
        is_error(&error, |error| {
            matches!(error, FloorpPrefsSyncError::UploadNotConfirmed)
        });
        assert!(delegate.finishes.lock().is_empty());
        assert!(
            store.sync_state().last_modified_millis == 0,
            "base must not advance on a failed upload"
        );

        // The retry confirms the SAME prepared transaction and only then
        // advances the successful base to the retried server timestamp.
        engine
            .set_uploaded(ServerTimestamp(202), vec![Guid::new(PREFS_RECORD_ID)])
            .unwrap();
        engine.sync_finished().unwrap();
        let finishes = delegate.finishes.lock();
        assert!(finishes.len() == 1);
        assert!(finishes[0].transaction_token == b"retry-proof");
        assert!(finishes[0].server_modified_millis == 202);
        assert!(
            store.sync_state().last_modified_millis == 202,
            "base advances only after the confirmed retry"
        );
    }

    #[test]
    fn account_associations_are_isolated_between_stores() {
        error_support::init_for_tests();
        let (delegate_a, store_a, engine_a) = test_engine(no_upload(b"account-a-proof"));
        let (delegate_b, store_b, engine_b) = test_engine(no_upload(b"account-b-proof"));

        let connected_a = EngineSyncAssociation::Connected(CollSyncIds {
            global: Guid::new("account-a-global"),
            coll: Guid::new("account-a-prefs"),
        });
        let connected_b = EngineSyncAssociation::Connected(CollSyncIds {
            global: Guid::new("account-b-global"),
            coll: Guid::new("account-b-prefs"),
        });
        engine_a.reset(&connected_a).unwrap();
        engine_b.reset(&connected_b).unwrap();

        // Disconnecting account A must not touch account B's association.
        engine_a.wipe().unwrap();

        let state_a = store_a.sync_state();
        assert!(state_a.global_sync_id.is_none());
        assert!(state_a.collection_sync_id.is_none());

        let state_b = store_b.sync_state();
        assert!(state_b.global_sync_id.as_deref() == Some("account-b-global"));
        assert!(state_b.collection_sync_id.as_deref() == Some("account-b-prefs"));

        // Each store reports only its own association changes: account A
        // connected then disconnected (2), account B only connected (1).
        assert!(delegate_a.association_resets.lock().len() == 2);
        assert!(delegate_b.association_resets.lock().len() == 1);
    }

    #[test]
    fn a_cancelled_run_never_confirms_and_the_store_stays_reusable() {
        error_support::init_for_tests();
        let delegate = Arc::new(FakeDelegate::default());
        delegate
            .plans
            .lock()
            .push_back(upload(b"cancelled-proof", "cancelled-notes"));
        let store = Arc::new(FloorpPrefsSyncStore::new(delegate.clone(), None).unwrap());
        let cancelled_engine = FloorpPrefsEngine::new(store.clone());

        stage(&cancelled_engine, Vec::new()).unwrap();
        assert!(apply(&cancelled_engine, 210).unwrap().len() == 1);

        // The run is cancelled (the engine is dropped without set_uploaded /
        // sync_finished): nothing may be confirmed and the base must not move.
        drop(cancelled_engine);
        assert!(delegate.finishes.lock().is_empty());
        assert!(store.sync_state().last_modified_millis == 0);

        // A fresh engine on the same store starts a clean session; the base
        // advances only after its own confirmation.
        delegate.plans.lock().push_back(no_upload(b"fresh-proof"));
        let fresh_engine = FloorpPrefsEngine::new(store.clone());
        stage(
            &fresh_engine,
            vec![target(210, serde_json::json!({ CONTROL_PREF_NAME: true }))],
        )
        .unwrap();
        assert!(apply(&fresh_engine, 211).unwrap().is_empty());
        fresh_engine
            .set_uploaded(ServerTimestamp(211), Vec::new())
            .unwrap();
        fresh_engine.sync_finished().unwrap();

        let finishes = delegate.finishes.lock();
        assert!(finishes.len() == 1);
        assert!(finishes[0].transaction_token == b"fresh-proof");
        assert!(store.sync_state().last_modified_millis == 211);
    }
}

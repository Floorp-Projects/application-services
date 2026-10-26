/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */
use crate::model::{parse_record, Cache, Record, WorkspacePreview};
use crate::{FloorpWorkspacesError as Error, Result, COLLECTION_NAME, ENGINE_VERSION};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};
use sync15::bso::{IncomingBso, OutgoingBso};
use sync15::engine::{
    CollSyncIds, CollectionRequest, EngineSyncAssociation, SyncEngine, SyncEngineId,
};
use sync15::{telemetry, CollectionName, Guid, ServerTimestamp};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct State {
    schema_version: u32,
    account_uid: Option<String>,
    revision: u64,
    global_sync_id: Option<String>,
    collection_sync_id: Option<String>,
    engine_version: Option<u32>,
    status: String,
    last_received_millis: Option<i64>,
    cursor_millis: i64,
    cache: Cache,
}
impl Default for State {
    fn default() -> Self {
        Self {
            schema_version: 1,
            account_uid: None,
            revision: 0,
            global_sync_id: None,
            collection_sync_id: None,
            engine_version: None,
            status: "unavailable".into(),
            last_received_millis: None,
            cursor_millis: 0,
            cache: Cache::default(),
        }
    }
}
impl State {
    fn valid(&self) -> bool {
        self.schema_version == 1
            && self.cursor_millis >= 0
            && self.cache.validate()
            && self.last_received_millis.is_none_or(|v| v >= 0)
            && self
                .account_uid
                .as_ref()
                .is_none_or(|uid| !uid.is_empty() && uid.len() <= 256)
            && (self.status != "ready"
                || (self.account_uid.is_some()
                    && self.global_sync_id.is_some()
                    && self.engine_version == Some(1)
                    && self.last_received_millis.is_some()))
            && (self.status == "ready"
                || (self.cache.records.is_empty()
                    && self.cache.tombstones.is_empty()
                    && self.cursor_millis == 0
                    && self.last_received_millis.is_none()))
            && matches!(
                self.status.as_str(),
                "ready" | "unavailable" | "unpublished" | "unsupportedVersion"
            )
            && match (&self.global_sync_id, &self.collection_sync_id) {
                (None, None) => true,
                (Some(global), Some(coll)) => {
                    Guid::new(global).is_valid_for_sync_server()
                        && Guid::new(coll).is_valid_for_sync_server()
                }
                _ => false,
            }
    }
    fn assoc(&self) -> EngineSyncAssociation {
        match (&self.global_sync_id, &self.collection_sync_id) {
            (Some(global), Some(coll)) => EngineSyncAssociation::Connected(CollSyncIds {
                global: Guid::new(global),
                coll: Guid::new(coll),
            }),
            _ => EngineSyncAssociation::Disconnected,
        }
    }
}
struct Inner {
    state: State,
    active: bool,
}

/// Account-scoped read-only cache. The caller must retain the store strongly
/// while registered, and bind the current FxA UID before exposing snapshots.
#[derive(uniffi::Object)]
pub struct FloorpWorkspacesStore {
    path: PathBuf,
    inner: Mutex<Inner>,
}

#[uniffi::export]
impl FloorpWorkspacesStore {
    #[uniffi::constructor]
    pub fn new(path: String) -> Result<Self> {
        let path = PathBuf::from(path);
        // A journal survives a failed cache replacement and takes precedence
        // over an older ready snapshot. Never fall back to ready cache when a
        // journal is corrupt or partially written.
        let journal = path.with_extension("invalidated");
        let bytes = match fs::read(&journal) {
            Ok(bytes) => {
                let state: State =
                    serde_json::from_slice(&bytes).map_err(|_| Error::InvalidCache)?;
                if !state.valid() || state.status == "ready" {
                    return Err(Error::InvalidCache);
                }
                Some(bytes)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err(Error::Storage),
        };
        let state = match bytes.map(Ok).unwrap_or_else(|| fs::read(&path)) {
            Ok(bytes) => {
                serde_json::from_slice::<State>(&bytes).map_err(|_| Error::InvalidCache)?
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(_) => return Err(Error::Storage),
        };
        if !state.valid() {
            return Err(Error::InvalidCache);
        }
        // A process restart never grants access to the prior owner's cache
        // until the embedding application has explicitly bound its account.
        Ok(Self {
            path,
            inner: Mutex::new(Inner {
                state,
                active: false,
            }),
        })
    }
    pub fn bind_account(&self, uid: String) -> Result<()> {
        if uid.is_empty() || uid.len() > 256 {
            return Err(Error::Inactive);
        }
        let mut inner = self.inner.lock();
        if inner.active && inner.state.account_uid.as_deref() == Some(&uid) {
            return Ok(());
        }
        let revision = next_revision(inner.state.revision)?;
        let mut next = if inner.state.account_uid.as_deref() == Some(&uid) {
            inner.state.clone()
        } else {
            State::default()
        };
        next.revision = revision;
        next.account_uid = Some(uid);
        // Fence an already running engine even if persistence fails.
        inner.active = false;
        inner.state.revision = revision;
        if inner.state.account_uid != next.account_uid {
            persist_negative(&self.path, &next)?;
        } else {
            persist(&self.path, &next)?;
        }
        inner.state = next;
        inner.active = true;
        Ok(())
    }
    pub fn invalidate(&self) -> Result<()> {
        let mut inner = self.inner.lock();
        let next = State {
            revision: next_revision(inner.state.revision)?,
            ..State::default()
        };
        inner.active = false;
        inner.state = next.clone();
        persist_negative(&self.path, &next)
    }
    pub fn register_with_sync_manager(self: Arc<Self>) {
        *registered_store().lock() = Arc::downgrade(&self);
    }
    pub fn snapshot_json(&self) -> Result<String> {
        self.serialize_snapshot(true)
    }

    /// Lightweight live association/status fence for copy admission. This
    /// never enumerates, clones, sorts, or serializes workspace/tab records.
    pub fn copy_eligibility_json(&self) -> Result<String> {
        self.serialize_snapshot(false)
    }
}

impl FloorpWorkspacesStore {
    fn serialize_snapshot(&self, include_workspaces: bool) -> Result<String> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Snapshot<'a> {
            schema_version: u32,
            account_uid: &'a Option<String>,
            revision: u64,
            global_sync_id: &'a Option<String>,
            collection_sync_id: &'a Option<String>,
            engine_version: Option<u32>,
            status: &'a str,
            last_received_millis: Option<i64>,
            workspaces: Vec<WorkspacePreview>,
        }
        let inner = self.inner.lock();
        let hidden = State {
            revision: inner.state.revision,
            ..State::default()
        };
        let s = if inner.active { &inner.state } else { &hidden };
        let workspaces = if include_workspaces && s.status == "ready" {
            s.cache.workspaces()
        } else {
            Vec::new()
        };
        serde_json::to_string(&Snapshot {
            schema_version: 1,
            account_uid: &s.account_uid,
            revision: s.revision,
            global_sync_id: &s.global_sync_id,
            collection_sync_id: &s.collection_sync_id,
            engine_version: s.engine_version,
            status: &s.status,
            last_received_millis: s.last_received_millis,
            workspaces,
        })
        .map_err(|_| Error::InvalidCache)
    }
}
fn next_revision(old: u64) -> Result<u64> {
    old.checked_add(1).ok_or(Error::StaleRun)
}
fn persist(path: &Path, state: &State) -> Result<()> {
    let parent = path.parent().ok_or(Error::Storage)?;
    fs::create_dir_all(parent).map_err(|_| Error::Storage)?;
    let temp = path.with_extension("pending");
    let result = (|| -> std::io::Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        let bytes = serde_json::to_vec(state).map_err(std::io::Error::other)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        File::open(parent)?.sync_all()?;
        match fs::remove_file(path.with_extension("invalidated")) {
            Ok(()) => File::open(parent)?.sync_all()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(|_| Error::Storage)
}
// Record a negative eligibility decision before replacing a ready cache.
// This separate journal deliberately does not share the `.pending` path whose
// failure may be what caused the cache commit to fail. It contains no URLs.
fn persist_negative(path: &Path, state: &State) -> Result<()> {
    let parent = path.parent().ok_or(Error::Storage)?;
    let journal = path.with_extension("invalidated");
    let write = (|| -> std::io::Result<()> {
        fs::create_dir_all(parent)?;
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&journal)?;
        file.write_all(&serde_json::to_vec(state).map_err(std::io::Error::other)?)?;
        file.sync_all()?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if write.is_err() {
        // If a journal itself could not be created, remove the old eligible
        // snapshot where possible. A partial journal is retained and makes the
        // next constructor fail closed rather than use the ready snapshot.
        if fs::remove_file(path).is_ok() {
            let _ = File::open(parent).and_then(|file| file.sync_all());
        }
        return Err(Error::Storage);
    }
    persist(path, state)
}

fn registered_store() -> &'static Mutex<Weak<FloorpWorkspacesStore>> {
    static STORE: OnceLock<Mutex<Weak<FloorpWorkspacesStore>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(Weak::new()))
}
pub fn get_registered_sync_engine(id: &SyncEngineId) -> Option<Box<dyn SyncEngine>> {
    assert_eq!(id, &SyncEngineId::FloorpWorkspaces);
    registered_store()
        .lock()
        .upgrade()
        .map(|store| Box::new(WorkspaceEngine::new(store)) as Box<dyn SyncEngine>)
}
#[derive(Default)]
struct Session {
    revision: Option<u64>,
    incoming: Vec<Record>,
    prepared: Option<State>,
    failed: bool,
}
struct WorkspaceEngine {
    store: Arc<FloorpWorkspacesStore>,
    session: Mutex<Session>,
}
impl WorkspaceEngine {
    fn new(store: Arc<FloorpWorkspacesStore>) -> Self {
        // Bind the engine before the shared client starts fetching global
        // control state. Account changes during that fetch must not rebind it.
        let revision = store.inner.lock().state.revision;
        Self {
            store,
            session: Mutex::new(Session {
                revision: Some(revision),
                ..Session::default()
            }),
        }
    }
    fn capture_revision(&self) -> Result<()> {
        let inner = self.store.inner.lock();
        if !inner.active {
            return Err(Error::Inactive);
        }
        let mut session = self.session.lock();
        match session.revision {
            Some(revision) if revision != inner.state.revision => Err(Error::StaleRun),
            Some(_) => Ok(()),
            None => {
                session.revision = Some(inner.state.revision);
                Ok(())
            }
        }
    }
}
impl SyncEngine for WorkspaceEngine {
    fn collection_name(&self) -> CollectionName {
        COLLECTION_NAME.into()
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn validate_remote_engine(&self, version: Option<usize>) -> anyhow::Result<bool> {
        let mut inner = self.store.inner.lock();
        if !inner.active {
            return Err(Error::Inactive.into());
        }
        let mut session = self.session.lock();
        if session.revision != Some(inner.state.revision) {
            return Err(Error::StaleRun.into());
        }
        if version == Some(ENGINE_VERSION) {
            return Ok(true);
        }
        let status = if version.is_none() {
            "unpublished"
        } else {
            "unsupportedVersion"
        };
        let next = State {
            account_uid: inner.state.account_uid.clone(),
            revision: next_revision(inner.state.revision)?,
            status: status.into(),
            engine_version: version.and_then(|v| u32::try_from(v).ok()),
            ..State::default()
        };
        // Mark the in-memory cache ineligible immediately. A failed save is an
        // error, never permission to expose stale content from this generation.
        inner.state = next.clone();
        *session = Session {
            revision: Some(next.revision),
            ..Session::default()
        };
        persist_negative(&self.store.path, &next)?;
        Ok(false)
    }
    fn check_incoming_request(&self) -> anyhow::Result<()> {
        self.capture_revision()?;
        Ok(())
    }
    fn sync_setup_unavailable(&self) -> anyhow::Result<()> {
        let mut inner = self.store.inner.lock();
        let mut session = self.session.lock();
        if !inner.active || session.revision != Some(inner.state.revision) {
            return Err(Error::StaleRun.into());
        }
        let next = State {
            account_uid: inner.state.account_uid.clone(),
            revision: next_revision(inner.state.revision)?,
            ..State::default()
        };
        inner.state = next.clone();
        *session = Session {
            revision: Some(next.revision),
            ..Session::default()
        };
        persist_negative(&self.store.path, &next)?;
        Ok(())
    }
    fn prepare_for_sync(&self, _: &dyn Fn() -> sync15::ClientData) -> anyhow::Result<()> {
        self.capture_revision()?;
        let mut session = self.session.lock();
        let revision = session.revision;
        *session = Session {
            revision,
            ..Session::default()
        };
        Ok(())
    }
    fn stage_incoming(
        &self,
        inbound: Vec<IncomingBso>,
        _: &mut telemetry::Engine,
    ) -> anyhow::Result<()> {
        self.capture_revision()?;
        let records = inbound
            .into_iter()
            .map(|b| parse_record(b.envelope.id.as_str(), &b.payload))
            .collect::<Result<Vec<_>>>();
        let mut session = self.session.lock();
        match records {
            Ok(records) if !session.failed => session.incoming.extend(records),
            _ => {
                session.failed = true;
                return Err(Error::InvalidRecord.into());
            }
        }
        Ok(())
    }
    fn apply(
        &self,
        timestamp: ServerTimestamp,
        _: &mut telemetry::Engine,
    ) -> anyhow::Result<Vec<OutgoingBso>> {
        self.capture_revision()?;
        let inner = self.store.inner.lock();
        let mut session = self.session.lock();
        if session.failed || session.revision != Some(inner.state.revision) {
            return Err(Error::StaleRun.into());
        }
        let mut next = inner.state.clone();
        for record in std::mem::take(&mut session.incoming) {
            if let Err(error) = next.cache.merge(record) {
                session.failed = true;
                return Err(error.into());
            }
        }
        next.cursor_millis = timestamp.as_millis();
        next.engine_version = Some(ENGINE_VERSION as u32);
        next.status = "ready".into();
        next.last_received_millis = Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| Error::Storage)?
                .as_millis()
                .try_into()
                .map_err(|_| Error::Storage)?,
        );
        next.revision = next_revision(next.revision)?;
        session.prepared = Some(next);
        Ok(Vec::new())
    }
    fn set_uploaded(&self, _: ServerTimestamp, ids: Vec<Guid>) -> anyhow::Result<()> {
        if !ids.is_empty() {
            return Err(Error::UnexpectedUpload.into());
        }
        Ok(())
    }
    fn sync_finished(&self) -> anyhow::Result<()> {
        let mut inner = self.store.inner.lock();
        let mut session = self.session.lock();
        if !inner.active || session.revision != Some(inner.state.revision) {
            return Err(Error::StaleRun.into());
        }
        let next = session.prepared.take().ok_or(Error::StaleRun)?;
        persist(&self.store.path, &next)?;
        inner.state = next;
        *session = Session {
            revision: Some(inner.state.revision),
            ..Session::default()
        };
        Ok(())
    }
    fn get_collection_request(
        &self,
        _: ServerTimestamp,
    ) -> anyhow::Result<Option<CollectionRequest>> {
        // A new download discards staging from any aborted prior run, even
        // when a caller does not use prepare_for_sync (the trait allows this).
        self.capture_revision()?;
        {
            let mut session = self.session.lock();
            let revision = session.revision;
            *session = Session {
                revision,
                ..Session::default()
            };
        }
        let inner = self.store.inner.lock();
        let mut request = CollectionRequest::new(COLLECTION_NAME.into()).full();
        if inner.state.cursor_millis > 0 {
            request = request.newer_than(ServerTimestamp(inner.state.cursor_millis));
        }
        Ok(Some(request))
    }
    fn get_sync_assoc(&self) -> anyhow::Result<EngineSyncAssociation> {
        self.capture_revision()?;
        Ok(self.store.inner.lock().state.assoc())
    }
    fn reset(&self, assoc: &EngineSyncAssociation) -> anyhow::Result<()> {
        let mut inner = self.store.inner.lock();
        if !inner.active {
            return if *assoc == EngineSyncAssociation::Disconnected {
                Ok(())
            } else {
                Err(Error::Inactive.into())
            };
        }
        let mut session = self.session.lock();
        if session.revision != Some(inner.state.revision) {
            return Err(Error::StaleRun.into());
        }
        let (global_sync_id, collection_sync_id) = match assoc {
            EngineSyncAssociation::Disconnected => (None, None),
            EngineSyncAssociation::Connected(ids) => {
                (Some(ids.global.to_string()), Some(ids.coll.to_string()))
            }
        };
        let next = State {
            account_uid: inner.state.account_uid.clone(),
            revision: next_revision(inner.state.revision)?,
            global_sync_id,
            collection_sync_id,
            ..State::default()
        };
        inner.state = next.clone();
        *session = Session {
            revision: Some(next.revision),
            ..Session::default()
        };
        persist_negative(&self.store.path, &next)?;
        Ok(())
    }
    fn wipe(&self) -> anyhow::Result<()> {
        let mut inner = self.store.inner.lock();
        if !inner.active {
            return Ok(());
        }
        let mut session = self.session.lock();
        if session.revision != Some(inner.state.revision) {
            return Err(Error::StaleRun.into());
        }
        let next = State {
            revision: next_revision(inner.state.revision)?,
            ..State::default()
        };
        inner.active = false;
        inner.state = next.clone();
        *session = Session {
            revision: Some(next.revision),
            ..Session::default()
        };
        persist_negative(&self.store.path, &next)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use sync15::bso::IncomingEnvelope;
    fn fixture() -> (
        tempfile::TempDir,
        Arc<FloorpWorkspacesStore>,
        WorkspaceEngine,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            FloorpWorkspacesStore::new(dir.path().join("cache.json").to_string_lossy().into())
                .unwrap(),
        );
        store.bind_account("account-a".into()).unwrap();
        let engine = WorkspaceEngine::new(store.clone());
        assert!(engine.validate_remote_engine(Some(1)).unwrap());
        engine
            .reset(&EngineSyncAssociation::Connected(CollSyncIds {
                global: Guid::new("global-a"),
                coll: Guid::new("collection-a"),
            }))
            .unwrap();
        (dir, store, engine)
    }
    fn stage(engine: &WorkspaceEngine) {
        let id = "aaaaaaaa-0000-0000-0000-000000000001";
        let payload=json!({"id":id,"entity":{"version":1,"kind":"workspace","workspaceId":id,"name":"Desktop","icon":null,"metadata":{"modifiedAt":0,"modifiedBy":id}}}).to_string();
        engine
            .stage_incoming(
                vec![IncomingBso {
                    envelope: IncomingEnvelope {
                        id: Guid::new(id),
                        modified: ServerTimestamp(10),
                        sortindex: None,
                        ttl: None,
                    },
                    payload,
                }],
                &mut telemetry::Engine::new("floorp-workspaces"),
            )
            .unwrap();
    }
    fn apply(engine: &WorkspaceEngine) {
        assert!(engine
            .apply(
                ServerTimestamp(100),
                &mut telemetry::Engine::new("floorp-workspaces")
            )
            .unwrap()
            .is_empty());
    }
    fn snapshot(store: &FloorpWorkspacesStore) -> Value {
        serde_json::from_str(&store.snapshot_json().unwrap()).unwrap()
    }
    #[test]
    fn cache_and_cursor_commit_together_only_at_success_and_survive_restart() {
        let (dir, store, engine) = fixture();
        stage(&engine);
        apply(&engine);
        assert!(snapshot(&store)["workspaces"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(store.inner.lock().state.cursor_millis, 0);
        engine.set_uploaded(ServerTimestamp(100), vec![]).unwrap();
        engine.sync_finished().unwrap();
        assert_eq!(snapshot(&store)["workspaces"][0]["name"], "Desktop");
        assert_eq!(store.inner.lock().state.cursor_millis, 100);
        let reopened =
            FloorpWorkspacesStore::new(dir.path().join("cache.json").to_string_lossy().into())
                .unwrap();
        assert!(snapshot(&reopened)["workspaces"]
            .as_array()
            .unwrap()
            .is_empty());
        reopened.bind_account("account-a".into()).unwrap();
        assert_eq!(snapshot(&reopened)["workspaces"][0]["name"], "Desktop");
        assert_eq!(reopened.inner.lock().state.cursor_millis, 100);
        assert_eq!(
            engine
                .get_collection_request(ServerTimestamp(100))
                .unwrap()
                .unwrap()
                .newer,
            Some(ServerTimestamp(100))
        );
    }
    #[test]
    fn account_change_and_feature_stop_fence_late_commits() {
        let (_dir, store, engine) = fixture();
        stage(&engine);
        apply(&engine);
        store.bind_account("account-b".into()).unwrap();
        assert!(engine.sync_finished().is_err());
        assert_eq!(snapshot(&store)["accountUid"], "account-b");
        assert!(snapshot(&store)["workspaces"]
            .as_array()
            .unwrap()
            .is_empty());
        let engine = WorkspaceEngine::new(store.clone());
        stage(&engine);
        apply(&engine);
        store.invalidate().unwrap();
        assert!(engine.sync_finished().is_err());
        assert_eq!(snapshot(&store)["accountUid"], Value::Null);
    }
    #[test]
    fn same_account_sync_id_reset_fences_download_and_prepared_snapshot() {
        let (_dir, store, engine) = fixture();
        stage(&engine);
        apply(&engine);
        let reset = WorkspaceEngine::new(store.clone());
        reset
            .reset(&EngineSyncAssociation::Connected(CollSyncIds {
                global: Guid::new("global-b"),
                coll: Guid::new("collection-b"),
            }))
            .unwrap();
        assert!(engine.sync_finished().is_err());
        assert_eq!(snapshot(&store)["globalSyncId"], "global-b");
        assert!(snapshot(&store)["workspaces"]
            .as_array()
            .unwrap()
            .is_empty());
    }
    #[test]
    fn absent_or_unsupported_version_withdraws_old_cache_and_fences_run() {
        for version in [None, Some(0), Some(2)] {
            let (_dir, store, engine) = fixture();
            stage(&engine);
            apply(&engine);
            engine.sync_finished().unwrap();
            let stale = WorkspaceEngine::new(store.clone());
            stage(&stale);
            apply(&stale);
            let preflight = WorkspaceEngine::new(store.clone());
            assert!(!preflight.validate_remote_engine(version).unwrap());
            assert!(stale.sync_finished().is_err());
            let snapshot = snapshot(&store);
            assert_eq!(
                snapshot["status"],
                if version.is_none() {
                    "unpublished"
                } else {
                    "unsupportedVersion"
                }
            );
            assert!(snapshot["workspaces"].as_array().unwrap().is_empty());
            assert_eq!(store.inner.lock().state.cursor_millis, 0);
        }
    }
    #[test]
    fn malformed_record_preserves_prior_snapshot_and_cursor() {
        let (_dir, store, engine) = fixture();
        stage(&engine);
        apply(&engine);
        engine.sync_finished().unwrap();
        let before = store.snapshot_json().unwrap();
        let retry = WorkspaceEngine::new(store.clone());
        assert!(retry
            .stage_incoming(
                vec![IncomingBso {
                    envelope: IncomingEnvelope {
                        id: Guid::new("bad"),
                        modified: ServerTimestamp(0),
                        sortindex: None,
                        ttl: None
                    },
                    payload: "{}".into()
                }],
                &mut telemetry::Engine::new("floorp-workspaces")
            )
            .is_err());
        assert_eq!(store.snapshot_json().unwrap(), before);
    }
    #[test]
    fn readonly_engine_rejects_upload_ack_and_corrupt_cache_fails_closed() {
        let (dir, _store, engine) = fixture();
        assert!(engine.is_read_only());
        assert!(engine
            .set_uploaded(ServerTimestamp(100), vec![Guid::new("unexpected")])
            .is_err());
        fs::write(dir.path().join("cache.json"), b"{\"schemaVersion\":99}").unwrap();
        assert!(
            FloorpWorkspacesStore::new(dir.path().join("cache.json").to_string_lossy().into())
                .is_err()
        );
    }
    #[test]
    fn durable_save_failure_never_advances_confirmed_cache_or_cursor() {
        let (dir, store, engine) = fixture();
        stage(&engine);
        apply(&engine);
        engine.sync_finished().unwrap();
        let before = store.snapshot_json().unwrap();
        let retry = WorkspaceEngine::new(store.clone());
        stage(&retry);
        assert!(retry
            .apply(
                ServerTimestamp(200),
                &mut telemetry::Engine::new("floorp-workspaces")
            )
            .unwrap()
            .is_empty());
        // Turn the destination into a directory to force atomic rename to fail.
        fs::remove_file(dir.path().join("cache.json")).unwrap();
        fs::create_dir(dir.path().join("cache.json")).unwrap();
        assert!(retry.sync_finished().is_err());
        assert_eq!(store.inner.lock().state.cursor_millis, 100);
        assert_eq!(store.snapshot_json().unwrap(), before);
    }

    #[test]
    fn invalid_staging_cannot_be_applied_and_new_download_discards_it() {
        let (_dir, store, engine) = fixture();
        stage(&engine);
        let invalid = IncomingBso {
            envelope: IncomingEnvelope {
                id: Guid::new("invalid"),
                modified: ServerTimestamp(0),
                sortindex: None,
                ttl: None,
            },
            payload: "{}".into(),
        };
        assert!(engine
            .stage_incoming(
                vec![invalid],
                &mut telemetry::Engine::new("floorp-workspaces")
            )
            .is_err());
        assert!(engine
            .apply(
                ServerTimestamp(100),
                &mut telemetry::Engine::new("floorp-workspaces")
            )
            .is_err());
        assert!(snapshot(&store)["workspaces"]
            .as_array()
            .unwrap()
            .is_empty());
        engine.get_collection_request(ServerTimestamp(100)).unwrap();
        stage(&engine);
        apply(&engine);
        engine.sync_finished().unwrap();
        assert_eq!(snapshot(&store)["workspaces"][0]["name"], "Desktop");
    }
    #[test]
    fn account_switch_during_global_setup_cannot_reset_or_prepare_old_engine() {
        let (_dir, store, engine) = fixture();
        store.bind_account("account-b".into()).unwrap();
        let before = store.snapshot_json().unwrap();
        assert!(engine.validate_remote_engine(Some(1)).is_err());
        assert!(engine
            .reset(&EngineSyncAssociation::Connected(CollSyncIds {
                global: Guid::new("account-a-global"),
                coll: Guid::new("account-a-collection")
            }))
            .is_err());
        assert!(engine.prepare_for_sync(&|| panic!()).is_err());
        assert!(engine.wipe().is_err());
        assert!(engine.check_incoming_request().is_err());
        assert!(engine.get_collection_request(ServerTimestamp(100)).is_err());
        assert_eq!(store.snapshot_json().unwrap(), before);
    }
    #[test]
    fn failed_negative_commits_never_restore_old_ready_cache_after_restart() {
        for decision in ["missing", "future", "reset", "logout", "setup"] {
            let (dir, store, engine) = fixture();
            stage(&engine);
            apply(&engine);
            engine.sync_finished().unwrap();
            let engine = WorkspaceEngine::new(store.clone());
            let pending = dir.path().join("cache.pending");
            fs::create_dir(&pending).unwrap();
            let result = match decision {
                "missing" => engine.validate_remote_engine(None).map(|_| ()),
                "future" => engine.validate_remote_engine(Some(2)).map(|_| ()),
                "reset" => engine.reset(&EngineSyncAssociation::Connected(CollSyncIds {
                    global: Guid::new("new-global"),
                    coll: Guid::new("new-collection"),
                })),
                "setup" => engine.sync_setup_unavailable(),
                _ => store.invalidate().map_err(anyhow::Error::from),
            };
            assert!(result.is_err(), "{decision}");
            assert!(dir.path().join("cache.invalidated").is_file());
            fs::remove_dir(&pending).unwrap();
            let reopened =
                FloorpWorkspacesStore::new(dir.path().join("cache.json").to_string_lossy().into())
                    .unwrap();
            reopened.bind_account("account-a".into()).unwrap();
            let restored = snapshot(&reopened);
            assert_ne!(restored["status"], "ready", "{decision}");
            assert!(
                restored["workspaces"].as_array().unwrap().is_empty(),
                "{decision}"
            );
            assert_eq!(reopened.inner.lock().state.cursor_millis, 0);
            assert!(!dir.path().join("cache.invalidated").exists());
        }
    }

    #[test]
    fn corrupt_invalidation_journal_never_falls_back_to_ready_cache() {
        let (dir, _store, engine) = fixture();
        stage(&engine);
        apply(&engine);
        engine.sync_finished().unwrap();
        fs::write(dir.path().join("cache.invalidated"), b"partial").unwrap();
        assert!(matches!(
            FloorpWorkspacesStore::new(dir.path().join("cache.json").to_string_lossy().into()),
            Err(Error::InvalidCache)
        ));
    }
    #[test]
    fn copy_eligibility_is_small_and_preserves_header_for_large_cache() {
        use crate::model::{Entity, Tab};
        let (_dir, store, engine) = fixture();
        stage(&engine);
        apply(&engine);
        engine.sync_finished().unwrap();
        let mut expected = snapshot(&store);
        expected["workspaces"] = json!([]);
        {
            let mut inner = store.inner.lock();
            for index in 0..5000 {
                let id = format!("bbbbbbbb-0000-0000-0000-{index:012x}");
                inner
                    .state
                    .cache
                    .merge(Record::Live(Entity::Tab {
                        version: 1,
                        workspace_id: "aaaaaaaa-0000-0000-0000-000000000001".into(),
                        tab: Tab {
                            id,
                            url: format!("https://example.org/{index}"),
                            title: "x".repeat(1024),
                            pinned: false,
                            position: index,
                            deleted: false,
                            modified_at: 0,
                            modified_by: "cccccccc-0000-0000-0000-000000000003".into(),
                        },
                    }))
                    .unwrap();
            }
        }
        let header = store.copy_eligibility_json().unwrap();
        assert!(header.len() < 512);
        assert_eq!(serde_json::from_str::<Value>(&header).unwrap(), expected);
        store.invalidate().unwrap();
        let revoked: Value = serde_json::from_str(&store.copy_eligibility_json().unwrap()).unwrap();
        assert_eq!(revoked["status"], "unavailable");
        assert_eq!(revoked["accountUid"], Value::Null);
        assert!(revoked["revision"].as_u64().unwrap() > expected["revision"].as_u64().unwrap());
    }
}

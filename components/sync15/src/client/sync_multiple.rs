/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

// This helps you perform a sync of multiple engines and helps you manage
// global and local state between syncs.

use super::state::{EngineChangesNeeded, GlobalState, PersistedGlobalState, SetupStateMachine};
use super::status::{ServiceStatus, SyncResult};
use super::storage_client::{
    BackoffListener, SetupStorageClient, Sync15StorageClient, Sync15StorageClientInit,
};
use crate::KeyBundle;
use crate::clients_engine::{self, CLIENTS_TTL_REFRESH, CommandProcessor};
use crate::engine::{EngineSyncAssociation, SyncEngine};
use crate::error::{Error, debug, info, trace, warn};
use crate::telemetry;
use interrupt_support::Interruptee;
use std::collections::HashMap;
use std::result;
use std::time::{Duration, SystemTime};

/// Info about the client to use. We reuse the client unless
/// we discover the client_init has changed, in which case we re-create one.
#[derive(Debug)]
struct ClientInfo {
    // the client_init used to create `client`.
    client_init: Sync15StorageClientInit,
    // the client (our tokenserver state machine state, and our http library's state)
    client: Sync15StorageClient,
}

impl ClientInfo {
    fn new(ci: &Sync15StorageClientInit) -> Result<Self, Error> {
        Ok(Self {
            client_init: ci.clone(),
            client: Sync15StorageClient::new(ci.clone())?,
        })
    }
}

/// Info we want callers to engine *in memory* for us so that subsequent
/// syncs are faster. This should never be persisted to storage as it holds
/// sensitive information, such as the sync decryption keys.
#[derive(Debug, Default)]
pub struct MemoryCachedState {
    last_client_info: Option<ClientInfo>,
    last_global_state: Option<GlobalState>,
    // These are just engined in memory, as persisting an invalid value far in the
    // future has the potential to break sync for good.
    next_sync_after: Option<SystemTime>,
    next_client_refresh_after: Option<SystemTime>,
}

impl MemoryCachedState {
    // Called we notice the cached state is stale.
    pub fn clear_sensitive_info(&mut self) {
        self.last_client_info = None;
        self.last_global_state = None;
        // Leave the backoff time, as there's no reason to think it's not still
        // true.
    }
    pub fn get_next_sync_after(&self) -> Option<SystemTime> {
        self.next_sync_after
    }
    pub fn should_refresh_client(&self) -> bool {
        match self.next_client_refresh_after {
            Some(t) => SystemTime::now() > t,
            None => true,
        }
    }
    pub fn note_client_refresh(&mut self) {
        self.next_client_refresh_after =
            Some(SystemTime::now() + Duration::from_secs(CLIENTS_TTL_REFRESH));
    }
}

/// Sync multiple engines
/// * `engines` - The engines to sync
/// * `persisted_global_state` - The global state to use, or None if never
///   before provided. At the end of the sync, and even when the sync fails,
///   the value in this cell should be persisted to permanent storage and
///   provided next time the sync is called.
/// * `last_client_info` - The client state to use, or None if never before
///   provided. At the end of the sync, the value should be persisted
///   *in memory only* - it should not be persisted to disk.
/// * `storage_init` - Information about how the sync http client should be
///   configured.
/// * `root_sync_key` - The KeyBundle used for encryption.
///
/// Returns a map, keyed by name and holding an error value - if any engine
/// fails, the sync will continue on to other engines, but the error will be
/// places in this map. The absence of a name in the map implies the engine
/// succeeded.
pub fn sync_multiple(
    engines: &[&dyn SyncEngine],
    persisted_global_state: &mut Option<String>,
    mem_cached_state: &mut MemoryCachedState,
    storage_init: &Sync15StorageClientInit,
    root_sync_key: &KeyBundle,
    interruptee: &dyn Interruptee,
    req_info: Option<SyncRequestInfo<'_>>,
) -> SyncResult {
    sync_multiple_with_command_processor(
        None,
        engines,
        persisted_global_state,
        mem_cached_state,
        storage_init,
        root_sync_key,
        interruptee,
        req_info,
    )
}

/// Like `sync_multiple`, but specifies an optional command processor to handle
/// commands from the clients collection. This function is called by the sync
/// manager, which provides its own processor.
#[allow(clippy::too_many_arguments)]
pub fn sync_multiple_with_command_processor(
    command_processor: Option<&dyn CommandProcessor>,
    engines: &[&dyn SyncEngine],
    persisted_global_state: &mut Option<String>,
    mem_cached_state: &mut MemoryCachedState,
    storage_init: &Sync15StorageClientInit,
    root_sync_key: &KeyBundle,
    interruptee: &dyn Interruptee,
    req_info: Option<SyncRequestInfo<'_>>,
) -> SyncResult {
    info!("Syncing {} engines", engines.len());
    let mut sync_result = SyncResult {
        service_status: ServiceStatus::OtherError,
        result: Ok(()),
        declined: None,
        next_sync_after: None,
        engine_results: HashMap::with_capacity(engines.len()),
        telemetry: telemetry::SyncTelemetryPing::new(),
    };
    let backoff = super::storage_client::new_backoff_listener();
    let req_info = req_info.unwrap_or_default();
    let driver = SyncMultipleDriver {
        command_processor,
        engines,
        storage_init,
        interruptee,
        engines_to_state_change: req_info.engines_to_state_change,
        backoff: backoff.clone(),
        root_sync_key,
        result: &mut sync_result,
        persisted_global_state,
        mem_cached_state,
        saw_auth_error: false,
        ignore_soft_backoff: req_info.is_user_action,
    };
    match driver.sync() {
        Ok(()) => {
            debug!(
                "sync was successful, final status={:?}",
                sync_result.service_status
            );
        }
        Err(e) => {
            warn!(
                "sync failed: {}, final status={:?}",
                e, sync_result.service_status,
            );
            sync_result.result = Err(e);
        }
    }
    // Respect `backoff` value when computing the next sync time even if we were
    // ignoring it during the sync
    sync_result.set_sync_after(backoff.get_required_wait(false).unwrap_or_default());
    mem_cached_state.next_sync_after = sync_result.next_sync_after;
    trace!("Sync result: {:?}", sync_result);
    sync_result
}

/// This is essentially a bag of information that the sync manager knows, but
/// otherwise we won't. It should probably be rethought if it gains many more
/// fields.
#[derive(Debug, Default)]
pub struct SyncRequestInfo<'a> {
    pub engines_to_state_change: Option<&'a HashMap<String, bool>>,
    pub is_user_action: bool,
}

fn setup_state_machine_for_engines<'a>(
    engines: &[&dyn SyncEngine],
    client: &'a dyn SetupStorageClient,
    key: &'a KeyBundle,
    pgs: &'a mut PersistedGlobalState,
    changes: Option<&'a HashMap<String, bool>>,
    interruptee: &'a dyn Interruptee,
) -> SetupStateMachine<'a> {
    if !engines.is_empty() && engines.iter().all(|engine| engine.is_read_only()) {
        SetupStateMachine::for_read_only_sync(client, key, pgs, interruptee)
    } else {
        SetupStateMachine::for_full_sync(client, key, pgs, changes, interruptee)
    }
}

// The sync multiple driver
struct SyncMultipleDriver<'info, 'res, 'pgs, 'mcs> {
    command_processor: Option<&'info dyn CommandProcessor>,
    engines: &'info [&'info dyn SyncEngine],
    storage_init: &'info Sync15StorageClientInit,
    root_sync_key: &'info KeyBundle,
    interruptee: &'info dyn Interruptee,
    backoff: BackoffListener,
    engines_to_state_change: Option<&'info HashMap<String, bool>>,
    result: &'res mut SyncResult,
    persisted_global_state: &'pgs mut Option<String>,
    mem_cached_state: &'mcs mut MemoryCachedState,
    ignore_soft_backoff: bool,
    saw_auth_error: bool,
}

impl SyncMultipleDriver<'_, '_, '_, '_> {
    /// The actual worker for sync_multiple.
    fn sync(mut self) -> result::Result<(), Error> {
        if self.engines.iter().any(|engine| {
            engine.is_read_only()
                && self
                    .engines_to_state_change
                    .is_some_and(|changes| changes.contains_key(engine.collection_name().as_ref()))
        }) {
            return Err(
                anyhow::anyhow!("read-only engine enablement cannot change remote state").into(),
            );
        }
        info!("Loading/initializing persisted state");
        let mut pgs = self.prepare_persisted_state();

        info!("Preparing client info");
        let client_info = self.prepare_client_info()?;

        if self.was_interrupted() {
            return Ok(());
        }

        info!("Entering sync state machine");
        // Advance the state machine to the point where it can perform a full
        // sync. This may involve uploading meta/global, crypto/keys etc.
        let mut global_state = self.run_state_machine(&client_info, &mut pgs)?;

        // Withdraw stale discovery eligibility from confirmed control state
        // before clients sync, backoff, or interruption can defer collection
        // downloads. This preflight performs no HTTP requests.
        self.preflight_read_only_engines(&global_state);

        if self.was_interrupted() {
            return Ok(());
        }

        // Set the service status to OK here - we may adjust it based on an individual
        // engine failing.
        self.result.service_status = ServiceStatus::Ok;

        let clients_engine = if let Some(command_processor) = self.command_processor {
            info!("Synchronizing clients engine");
            let should_refresh = self.mem_cached_state.should_refresh_client();
            let mut engine = clients_engine::Engine::new(command_processor, self.interruptee);
            if let Err(e) = engine.sync(
                &client_info.client,
                &global_state,
                self.root_sync_key,
                should_refresh,
            ) {
                // Record telemetry with the error just in case...
                let mut telem_sync = telemetry::SyncTelemetry::new();
                let mut telem_engine = telemetry::Engine::new("clients");
                telem_engine.failure(&e);
                telem_sync.engine(telem_engine);
                self.result.service_status = ServiceStatus::from_err(&e);

                // ...And bail, because a clients engine sync failure is fatal.
                return Err(e);
            }
            // We don't record telemetry for successful clients engine
            // syncs, since we only keep client records in memory, we
            // expect the counts to be the same most times, and a
            // failure aborts the entire sync.
            if self.was_interrupted() {
                return Ok(());
            }
            self.mem_cached_state.note_client_refresh();
            Some(engine)
        } else {
            None
        };

        info!("Synchronizing engines");

        let telem_sync =
            self.sync_engines(&client_info, &mut global_state, clients_engine.as_ref());
        self.result.telemetry.sync(telem_sync);

        info!("Finished syncing engines.");

        if !self.saw_auth_error {
            trace!("Updating persisted global state");
            self.mem_cached_state.last_client_info = Some(client_info);
            self.mem_cached_state.last_global_state = Some(global_state);
        }

        Ok(())
    }

    fn was_interrupted(&mut self) -> bool {
        if self.interruptee.was_interrupted() {
            info!("Interrupted, bailing out");
            self.result.service_status = ServiceStatus::Interrupted;
            true
        } else {
            false
        }
    }

    fn preflight_read_only_engines(&mut self, global_state: &GlobalState) {
        for engine in self.engines.iter().filter(|engine| engine.is_read_only()) {
            let name = engine.collection_name();
            match super::LocalCollStateMachine::get_state(*engine, global_state, self.root_sync_key)
            {
                Ok(Some(_)) => {}
                // Missing/unsupported/declined definitions have already been
                // durably withdrawn by the provider, without any GET/POST.
                Ok(None) => {
                    self.result.engine_results.insert(name.into(), Ok(()));
                }
                Err(error) => {
                    self.result.engine_results.insert(name.into(), Err(error));
                }
            }
        }
    }

    fn sync_engines(
        &mut self,
        client_info: &ClientInfo,
        global_state: &mut GlobalState,
        clients: Option<&clients_engine::Engine<'_>>,
    ) -> telemetry::SyncTelemetry {
        let mut telem_sync = telemetry::SyncTelemetry::new();
        for engine in self.engines {
            let name = engine.collection_name();
            if engine.is_read_only() {
                if let Some(outcome) = self.result.engine_results.get(name.as_ref()) {
                    let mut telem_engine = telemetry::Engine::new(name.as_ref());
                    if let Err(error) = outcome {
                        telem_engine.failure(error);
                    }
                    telem_sync.engine(telem_engine);
                    continue;
                }
            }
            if self
                .backoff
                .get_required_wait(self.ignore_soft_backoff)
                .is_some()
            {
                warn!("Got backoff, bailing out of sync early");
                break;
            }
            if !engine.is_read_only() && global_state.global.declined.iter().any(|e| e == &*name) {
                info!("The {} engine is declined. Skipping", name);
                continue;
            }
            info!("Syncing {} engine!", name);

            let mut telem_engine = telemetry::Engine::new(&*name);
            let result = super::sync::synchronize_with_clients_engine(
                &client_info.client,
                global_state,
                self.root_sync_key,
                clients,
                *engine,
                true,
                &mut telem_engine,
                self.interruptee,
            );

            match result {
                Ok(()) => info!("Sync of {} was successful!", name),
                Err(ref e) => {
                    warn!("Sync of {} failed! {:?}", name, e);
                    let this_status = ServiceStatus::from_err(e);
                    // The only error which forces us to discard our state is an
                    // auth error.
                    self.saw_auth_error =
                        self.saw_auth_error || this_status == ServiceStatus::AuthenticationError;
                    telem_engine.failure(e);
                    // If the failure from the engine looks like anything other than
                    // a "engine error" we don't bother trying the others.
                    if this_status != ServiceStatus::OtherError {
                        telem_sync.engine(telem_engine);
                        self.result.engine_results.insert(name.into(), result);
                        self.result.service_status = this_status;
                        break;
                    }
                }
            }
            telem_sync.engine(telem_engine);
            self.result.engine_results.insert(name.into(), result);
            if self.was_interrupted() {
                break;
            }
        }
        telem_sync
    }

    fn run_state_machine(
        &mut self,
        client_info: &ClientInfo,
        pgs: &mut PersistedGlobalState,
    ) -> result::Result<GlobalState, Error> {
        let last_state = self.mem_cached_state.last_global_state.take();

        let mut state_machine = setup_state_machine_for_engines(
            self.engines,
            &client_info.client,
            self.root_sync_key,
            pgs,
            self.engines_to_state_change,
            self.interruptee,
        );

        info!("Advancing state machine to ready (full)");
        let res = state_machine.run_to_ready(last_state);
        // Grab this now even though we don't need it until later to avoid a
        // lifetime issue
        let changes = state_machine.changes_needed.take();
        // The state machine might have updated our persisted_global_state, so
        // update the caller's repr of it.
        *self.persisted_global_state = Some(serde_json::to_string(&pgs)?);

        // Now that we've gone through the state machine, engine the declined list in
        // the sync_result
        self.result.declined = Some(pgs.get_declined().to_vec());
        debug!(
            "Declined engines list after state machine set to: {:?}",
            self.result.declined,
        );

        if let Some(c) = changes {
            self.wipe_or_reset_engines(c, &client_info.client)?;
        }
        let state = match res {
            Err(e) => {
                if matches!(&e, Error::SetupRequired | Error::ClientUpgradeRequired) {
                    for engine in self.engines.iter().filter(|engine| engine.is_read_only()) {
                        engine.sync_setup_unavailable()?;
                    }
                }
                self.result.service_status = ServiceStatus::from_err(&e);
                return Err(e);
            }
            Ok(state) => state,
        };
        self.result.telemetry.uid(client_info.client.hashed_uid()?);
        // As for client_info, put None back now so we start from scratch on error.
        self.mem_cached_state.last_global_state = None;
        Ok(state)
    }

    fn wipe_or_reset_engines(
        &mut self,
        changes: EngineChangesNeeded,
        client: &Sync15StorageClient,
    ) -> result::Result<(), Error> {
        if changes.local_resets.is_empty() && changes.remote_wipes.is_empty() {
            return Ok(());
        }
        for e in &changes.remote_wipes {
            info!("Engine {:?} just got disabled locally, wiping server", e);
            client.wipe_remote_engine(e)?;
        }

        for s in self.engines {
            let name = s.collection_name();
            // Read-only providers withdraw their cache through version
            // preflight, rather than resetting before validating the remote
            // definition (which may be absent or unsupported).
            if !s.is_read_only() && changes.local_resets.contains(&*name) {
                info!("Resetting engine {}, as it was declined remotely", name);
                s.reset(&EngineSyncAssociation::Disconnected)?;
            }
        }

        Ok(())
    }

    fn prepare_client_info(&mut self) -> result::Result<ClientInfo, Error> {
        let mut client_info = match self.mem_cached_state.last_client_info.take() {
            Some(client_info) => {
                // if our storage_init has changed it probably means the user has
                // changed, courtesy of the 'kid' in the structure. Thus, we can't
                // reuse the client or the memory cached state. We do keep the disk
                // state as currently that's only the declined list.
                if client_info.client_init != *self.storage_init {
                    info!("Discarding all state as the account might have changed");
                    *self.mem_cached_state = MemoryCachedState::default();
                    ClientInfo::new(self.storage_init)?
                } else {
                    debug!("Reusing memory-cached client_info");
                    // we can reuse it (which should be the common path)
                    client_info
                }
            }
            None => {
                debug!("mem_cached_state was stale or missing, need setup");
                // We almost certainly have no other state here, but to be safe, we
                // throw away any memory state we do have.
                self.mem_cached_state.clear_sensitive_info();
                ClientInfo::new(self.storage_init)?
            }
        };
        // Ensure we use the correct listener here rather than on all the branches
        // above, since it seems less error prone.
        client_info.client.backoff = self.backoff.clone();
        Ok(client_info)
    }

    fn prepare_persisted_state(&mut self) -> PersistedGlobalState {
        // Note that any failure to use a persisted state means we also decline
        // to use our memory cached state, so that we fully rebuild that
        // persisted state for next time.
        match self.persisted_global_state {
            Some(persisted_string) if !persisted_string.is_empty() => {
                match serde_json::from_str::<PersistedGlobalState>(persisted_string) {
                    Ok(state) => {
                        trace!("Read persisted state: {:?}", state);
                        // Note that we don't set `result.declined` from the
                        // data in state - it remains None, which explicitly
                        // indicates "we don't have updated info".
                        state
                    }
                    _ => {
                        // Don't log the error since it might contain sensitive
                        // info (although currently it only contains the declined engines list)
                        error_support::report_error!(
                            "sync15-prepare-persisted-state",
                            "Failed to parse PersistedGlobalState from JSON! Falling back to default"
                        );
                        *self.mem_cached_state = MemoryCachedState::default();
                        PersistedGlobalState::default()
                    }
                }
            }
            _ => {
                info!(
                    "The application didn't give us persisted state - \
                     this is only expected on the very first run for a given user."
                );
                *self.mem_cached_state = MemoryCachedState::default();
                PersistedGlobalState::default()
            }
        }
    }
}

#[cfg(test)]
mod read_only_driver_tests {
    use super::super::request::{InfoCollections, InfoConfiguration};
    use super::super::{CollectionKeys, Sync15ClientResponse};
    use super::*;
    use crate::bso::{
        IncomingBso, IncomingEncryptedBso, IncomingEnvelope, OutgoingBso, OutgoingEncryptedBso,
    };
    use crate::engine::CollectionRequest;
    use crate::record_types::{MetaGlobalEngine, MetaGlobalRecord};
    use crate::{CollectionName, Guid, ServerTimestamp};
    use interrupt_support::NeverInterrupts;
    use std::cell::{Cell, RefCell};
    struct Engine {
        readonly: bool,
        ready: Cell<bool>,
        preflights: RefCell<Vec<Option<usize>>>,
    }
    impl SyncEngine for Engine {
        fn collection_name(&self) -> CollectionName {
            "floorp-workspaces".into()
        }
        fn is_read_only(&self) -> bool {
            self.readonly
        }
        fn validate_remote_engine(&self, version: Option<usize>) -> anyhow::Result<bool> {
            self.preflights.borrow_mut().push(version);
            if version != Some(1) {
                self.ready.set(false);
                return Ok(false);
            }
            Ok(true)
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
        fn set_uploaded(&self, _: ServerTimestamp, _: Vec<Guid>) -> anyhow::Result<()> {
            unreachable!()
        }
        fn get_collection_request(
            &self,
            _: ServerTimestamp,
        ) -> anyhow::Result<Option<CollectionRequest>> {
            unreachable!()
        }
        fn get_sync_assoc(&self) -> anyhow::Result<EngineSyncAssociation> {
            panic!("declined discovery must be rejected before association lookup")
        }
        fn reset(&self, _: &EngineSyncAssociation) -> anyhow::Result<()> {
            unreachable!()
        }
        fn wipe(&self) -> anyhow::Result<()> {
            unreachable!()
        }
    }
    fn engine(readonly: bool) -> Engine {
        Engine {
            readonly,
            ready: Cell::new(true),
            preflights: RefCell::new(Vec::new()),
        }
    }
    fn global(key: &KeyBundle) -> GlobalState {
        GlobalState {
            config: InfoConfiguration::default(),
            collections: InfoCollections::default(),
            global: MetaGlobalRecord {
                sync_id: "global-sync-id".into(),
                storage_version: 5,
                engines: HashMap::from([(
                    "floorp-workspaces".into(),
                    MetaGlobalEngine {
                        version: 1,
                        sync_id: "collection-sync-id".into(),
                    },
                )]),
                declined: vec![],
            },
            global_timestamp: ServerTimestamp(100),
            keys: CollectionKeys::new_random()
                .unwrap()
                .to_encrypted_payload(key)
                .unwrap(),
            keys_timestamp: ServerTimestamp(100),
        }
    }
    #[test]
    fn driver_with_already_persisted_decline_withdraws_ready_discovery_cache() {
        nss_as::ensure_initialized();
        let key = KeyBundle::new_random().unwrap();
        let init = Sync15StorageClientInit {
            key_id: "kid".into(),
            access_token: "token".into(),
            tokenserver_url: "https://example.org/token".parse().unwrap(),
        };
        let client = ClientInfo::new(&init).unwrap();
        let discovery = engine(true);
        let engines: [&dyn SyncEngine; 1] = [&discovery];
        let mut global = global(&key);
        global.global.declined.push("floorp-workspaces".into());
        // This reproduces a tabs-only run already having persisted the decline.
        let mut pgs = Some(
            serde_json::to_string(&PersistedGlobalState::V2 {
                declined: Some(vec!["floorp-workspaces".into()]),
            })
            .unwrap(),
        );
        let mut cache = MemoryCachedState::default();
        let mut result = SyncResult {
            service_status: ServiceStatus::Ok,
            result: Ok(()),
            declined: None,
            next_sync_after: None,
            engine_results: HashMap::new(),
            telemetry: telemetry::SyncTelemetryPing::new(),
        };
        let mut driver = SyncMultipleDriver {
            command_processor: None,
            engines: &engines,
            storage_init: &init,
            root_sync_key: &key,
            interruptee: &NeverInterrupts,
            backoff: super::super::storage_client::new_backoff_listener(),
            engines_to_state_change: None,
            result: &mut result,
            persisted_global_state: &mut pgs,
            mem_cached_state: &mut cache,
            ignore_soft_backoff: false,
            saw_auth_error: false,
        };
        driver.preflight_read_only_engines(&global);
        driver.backoff.note_backoff(60);
        driver.sync_engines(&client, &mut global, None);
        assert!(!discovery.ready.get());
        assert_eq!(*discovery.preflights.borrow(), vec![None]);
        assert!(
            result
                .engine_results
                .get("floorp-workspaces")
                .unwrap()
                .is_ok()
        );
    }
    struct SetupTransport {
        state: GlobalState,
        missing_meta: bool,
        missing_keys: bool,
        writes: RefCell<Vec<&'static str>>,
    }
    fn success<T>(record: T) -> Result<Sync15ClientResponse<T>, Error> {
        Ok(Sync15ClientResponse::Success {
            status: 200,
            record,
            last_modified: ServerTimestamp(100),
            route: "mock".into(),
        })
    }
    impl SetupStorageClient for SetupTransport {
        fn fetch_info_configuration(
            &self,
        ) -> Result<Sync15ClientResponse<InfoConfiguration>, Error> {
            success(self.state.config.clone())
        }
        fn fetch_info_collections(&self) -> Result<Sync15ClientResponse<InfoCollections>, Error> {
            success(self.state.collections.clone())
        }
        fn fetch_meta_global(&self) -> Result<Sync15ClientResponse<MetaGlobalRecord>, Error> {
            if self.missing_meta {
                Ok(Sync15ClientResponse::Error(
                    crate::error::ErrorResponse::NotFound {
                        route: "meta/global".into(),
                    },
                ))
            } else {
                success(self.state.global.clone())
            }
        }
        fn fetch_crypto_keys(&self) -> Result<Sync15ClientResponse<IncomingEncryptedBso>, Error> {
            if self.missing_keys {
                Ok(Sync15ClientResponse::Error(
                    crate::error::ErrorResponse::NotFound {
                        route: "crypto/keys".into(),
                    },
                ))
            } else {
                success(IncomingEncryptedBso::new(
                    IncomingEnvelope {
                        id: Guid::new("keys"),
                        modified: ServerTimestamp(100),
                        sortindex: None,
                        ttl: None,
                    },
                    self.state.keys.clone(),
                ))
            }
        }
        fn put_meta_global(
            &self,
            _: ServerTimestamp,
            _: &MetaGlobalRecord,
        ) -> Result<ServerTimestamp, Error> {
            self.writes.borrow_mut().push("PUT meta/global");
            Err(Error::SetupRequired)
        }
        fn put_crypto_keys(
            &self,
            _: ServerTimestamp,
            _: &OutgoingEncryptedBso,
        ) -> Result<(), Error> {
            self.writes.borrow_mut().push("PUT crypto/keys");
            Err(Error::SetupRequired)
        }
        fn wipe_all_remote(&self) -> Result<(), Error> {
            self.writes.borrow_mut().push("DELETE storage");
            Err(Error::SetupRequired)
        }
    }
    #[test]
    fn discovery_only_driver_setup_never_wipes_or_repairs_account_storage() {
        nss_as::ensure_initialized();
        let key = KeyBundle::new_random().unwrap();
        let discovery = engine(true);
        for case in ["missing-meta", "missing-keys", "old-storage", "ready"] {
            let mut state = global(&key);
            if case == "old-storage" {
                state.global.storage_version = 4;
            }
            let transport = SetupTransport {
                state,
                missing_meta: case == "missing-meta",
                missing_keys: case == "missing-keys",
                writes: RefCell::new(Vec::new()),
            };
            let mut pgs = PersistedGlobalState::default();
            let mut machine = setup_state_machine_for_engines(
                &[&discovery],
                &transport,
                &key,
                &mut pgs,
                None,
                &NeverInterrupts,
            );
            let result = machine.run_to_ready(None);
            if case == "ready" {
                assert!(result.is_ok());
            } else {
                assert!(matches!(result, Err(Error::SetupRequired)));
            }
            assert!(
                transport.writes.borrow().is_empty(),
                "{case}: unexpected writes {:?}",
                transport.writes.borrow()
            );
        }
        // The established full-sync policy is preserved for ordinary engines.
        let ordinary = engine(false);
        let transport = SetupTransport {
            state: global(&key),
            missing_meta: true,
            missing_keys: false,
            writes: RefCell::new(Vec::new()),
        };
        let mut pgs = PersistedGlobalState::default();
        let mut machine = setup_state_machine_for_engines(
            &[&ordinary],
            &transport,
            &key,
            &mut pgs,
            None,
            &NeverInterrupts,
        );
        assert!(machine.run_to_ready(None).is_err());
        assert_eq!(*transport.writes.borrow(), vec!["DELETE storage"]);
    }
}

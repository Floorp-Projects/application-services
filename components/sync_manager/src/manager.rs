/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::error::*;
use crate::types::{ServiceStatus, SyncEngineSelection, SyncParams, SyncReason, SyncResult};
use crate::{reset, reset_all, wipe};
use error_support::{breadcrumb, debug, info, warn};
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::convert::TryFrom;
use std::time::SystemTime;
use sync15::client::{
    sync_multiple_with_command_processor, MemoryCachedState, Sync15StorageClientInit,
    SyncRequestInfo,
};
use sync15::clients_engine::{Command, CommandProcessor, CommandStatus, Settings};
use sync15::engine::{EngineSyncAssociation, SyncEngine, SyncEngineId};

#[derive(Default)]
pub struct SyncManager {
    operation: Mutex<()>,
    mem_cached_state: Mutex<Option<MemoryCachedState>>,
}

impl SyncManager {
    pub fn new() -> Self {
        Self::default()
    }

    fn get_engine_id(engine_name: &str) -> Result<SyncEngineId> {
        SyncEngineId::try_from(engine_name).map_err(SyncManagerError::UnknownEngine)
    }

    fn get_engine(engine_id: &SyncEngineId) -> Option<Box<dyn SyncEngine>> {
        match engine_id {
            SyncEngineId::History => places::get_registered_sync_engine(engine_id),
            SyncEngineId::Bookmarks => places::get_registered_sync_engine(engine_id),
            SyncEngineId::Addresses => autofill::get_registered_sync_engine(engine_id),
            SyncEngineId::CreditCards => autofill::get_registered_sync_engine(engine_id),
            SyncEngineId::Passwords => logins::get_registered_sync_engine(engine_id),
            SyncEngineId::Tabs => tabs::get_registered_sync_engine(engine_id),
            SyncEngineId::Prefs => floorp_prefs_sync::get_registered_sync_engine(engine_id),
        }
    }

    fn validate_enabled_changes(enabled_changes: &HashMap<String, bool>) -> Result<()> {
        // `prefs` is a shared aggregate collection used by Desktop and other
        // products. It is a per-run special engine, never a toggleable engine.
        // In particular, the generic disable path declines it globally and
        // wipes the entire remote collection, not just Floorp Notes.
        if enabled_changes.contains_key(floorp_prefs_sync::PREFS_COLLECTION_NAME) {
            return Err(SyncManagerError::UnsupportedFeature(
                "changing enablement of the shared prefs collection".to_string(),
            ));
        }
        Ok(())
    }

    pub fn wipe(&self, engine_name: &str) -> Result<()> {
        let _operation = self.operation.lock();
        if let Some(engine) = Self::get_engine(&Self::get_engine_id(engine_name)?) {
            engine.wipe()?;
        }
        Ok(())
    }

    pub fn reset(&self, engine_name: &str) -> Result<()> {
        let _operation = self.operation.lock();
        if let Some(engine) = Self::get_engine(&Self::get_engine_id(engine_name)?) {
            engine.reset(&EngineSyncAssociation::Disconnected)?;
        }
        Ok(())
    }

    pub fn reset_all(&self) -> Result<()> {
        let _operation = self.operation.lock();
        for (_, engine) in self.iter_registered_engines() {
            engine.reset(&EngineSyncAssociation::Disconnected)?;
        }
        Ok(())
    }

    fn disconnect_failures(&self) -> Vec<(SyncEngineId, anyhow::Error)> {
        // A disconnect/reset must not invalidate a prepared prefs transaction
        // after it has returned an outgoing record but before sync15 uploads
        // it. Serialize the complete operation with `sync`.
        let _operation = self.operation.lock();
        let mut failures = Vec::new();
        for engine_id in SyncEngineId::iter() {
            if let Some(engine) = Self::get_engine(&engine_id) {
                if let Err(e) = engine.reset(&EngineSyncAssociation::Disconnected) {
                    failures.push((engine_id, e));
                }
            } else {
                warn!("Unable to reset {}, be sure to call register_with_sync_manager before disconnect if this is surprising", engine_id);
            }
        }
        failures
    }

    /// Disconnect engines from sync, deleting/resetting the sync-related data.
    ///
    /// This legacy entry point reports reset failures but cannot return them to
    /// its caller. New callers that must persist an association reset should
    /// use [`Self::disconnect_checked`].
    pub fn disconnect(&self) {
        breadcrumb!("SyncManager disconnect()");
        for (engine_id, error) in self.disconnect_failures() {
            error_support::report_error!(
                "sync-manager-reset",
                "Failed to reset {}: {}",
                engine_id,
                error
            );
        }
    }

    /// Disconnect all registered engines and return the first reset failure.
    ///
    /// All engines are attempted before this returns. Additional failures are
    /// still reported so that none are silently discarded.
    pub fn disconnect_checked(&self) -> Result<()> {
        breadcrumb!("SyncManager disconnect_checked()");
        let mut failures = self.disconnect_failures().into_iter();
        let Some((first_engine, first_error)) = failures.next() else {
            return Ok(());
        };
        for (engine_id, error) in failures {
            error_support::report_error!(
                "sync-manager-reset",
                "Failed to reset {}: {}",
                engine_id,
                error
            );
        }
        Err(first_error
            .context(format!("Failed to reset {first_engine}"))
            .into())
    }

    /// Perform a sync.  See [SyncParams] and [SyncResult] for details on how this works
    pub fn sync(&self, params: SyncParams) -> Result<SyncResult> {
        breadcrumb!("SyncManager::sync started");
        Self::validate_enabled_changes(&params.enabled_changes)?;
        let _operation = self.operation.lock();
        let mut state = self.mem_cached_state.lock();
        let engines = self.calc_engines_to_sync(&params.engines)?;
        let next_sync_after = state.as_ref().and_then(|mcs| mcs.get_next_sync_after());
        let result = if !backoff_in_effect(next_sync_after, &params) {
            info!("No backoff in effect (or we decided to ignore it), starting sync");
            self.do_sync(params, &mut state, engines)
        } else {
            breadcrumb!(
                "Backoff still in effect (until {:?}), bailing out early",
                next_sync_after
            );
            Ok(SyncResult {
                status: ServiceStatus::BackedOff,
                successful: Default::default(),
                failures: Default::default(),
                declined: None,
                next_sync_allowed_at: next_sync_after,
                persisted_state: params.persisted_state.unwrap_or_default(),
                // It would be nice to record telemetry here.
                telemetry_json: None,
            })
        };
        breadcrumb!("SyncManager sync ended");
        result
    }

    fn do_sync(
        &self,
        mut params: SyncParams,
        state: &mut Option<MemoryCachedState>,
        mut engines: Vec<Box<dyn SyncEngine>>,
    ) -> Result<SyncResult> {
        let key_bundle = sync15::KeyBundle::from_ksync_base64(&params.auth_info.sync_key)?;
        let tokenserver_url = url::Url::parse(&params.auth_info.tokenserver_url)?;
        let interruptee = interrupt_support::ShutdownInterruptee;
        let mut mem_cached_state = state.take().unwrap_or_default();
        let mut disk_cached_state = params.persisted_state.take();

        // tell engines about the local encryption key.
        for engine in engines.iter_mut() {
            if let Some(key) = params.local_encryption_keys.get(&*engine.collection_name()) {
                engine.set_local_encryption_key(key)?
            }
        }

        let engine_refs: Vec<&dyn SyncEngine> = engines.iter().map(|s| &**s).collect();

        let client_init = Sync15StorageClientInit {
            key_id: params.auth_info.kid.clone(),
            access_token: params.auth_info.fxa_access_token.clone(),
            tokenserver_url,
        };
        let engines_to_change = if params.enabled_changes.is_empty() {
            None
        } else {
            Some(&params.enabled_changes)
        };

        let settings = Settings {
            fxa_device_id: params.device_settings.fxa_device_id,
            device_name: params.device_settings.name,
            device_type: params.device_settings.kind,
        };
        let c = SyncClient::new(settings);
        let result = sync_multiple_with_command_processor(
            Some(&c),
            &engine_refs,
            &mut disk_cached_state,
            &mut mem_cached_state,
            &client_init,
            &key_bundle,
            &interruptee,
            Some(SyncRequestInfo {
                engines_to_state_change: engines_to_change,
                is_user_action: matches!(params.reason, SyncReason::User),
            }),
        );
        *state = Some(mem_cached_state);

        info!("Sync finished with status {:?}", result.service_status);
        let status = ServiceStatus::from(result.service_status);
        for (engine, result) in result.engine_results.iter() {
            info!("engine {:?} status: {:?}", engine, result);
        }
        let mut successful: Vec<String> = Vec::new();
        let mut failures: HashMap<String, String> = HashMap::new();
        for (engine, result) in result.engine_results.into_iter() {
            match result {
                Ok(_) => {
                    successful.push(engine);
                }
                Err(err) => {
                    failures.insert(engine, err.to_string());
                }
            }
        }
        let telemetry_json = serde_json::to_string(&result.telemetry).unwrap();

        Ok(SyncResult {
            status,
            successful,
            failures,
            declined: result.declined,
            next_sync_allowed_at: result.next_sync_after,
            persisted_state: disk_cached_state.unwrap_or_default(),
            telemetry_json: Some(telemetry_json),
        })
    }

    fn iter_registered_engines(&self) -> impl Iterator<Item = (SyncEngineId, Box<dyn SyncEngine>)> {
        SyncEngineId::iter().filter_map(|id| Self::get_engine(&id).map(|engine| (id, engine)))
    }

    pub fn get_available_engines(&self) -> Vec<String> {
        self.iter_registered_engines()
            // The embedding app treats this list as engines that can be
            // toggled. `prefs` is intentionally selectable only by an explicit
            // per-run request, because globally disabling it wipes shared data.
            .filter(|(name, _)| name != &SyncEngineId::Prefs)
            .map(|(name, _)| name.to_string())
            .collect()
    }

    fn calc_engines_to_sync(
        &self,
        selection: &SyncEngineSelection,
    ) -> Result<Vec<Box<dyn SyncEngine>>> {
        // BTreeMap to ensure we sync the engines in priority order.
        let mut engine_map: BTreeMap<_, _> = self.iter_registered_engines().collect();
        breadcrumb!(
            "Checking engines requested ({:?}) vs local engines ({:?})",
            selection,
            engine_map
                .keys()
                .map(|engine_id| engine_id.name())
                .collect::<Vec<_>>(),
        );
        if let SyncEngineSelection::Some {
            engines: engine_names,
        } = selection
        {
            // Validate selection and convert to SyncEngineId
            let mut selected_engine_ids: HashSet<SyncEngineId> = HashSet::new();
            for name in engine_names {
                let engine_id = Self::get_engine_id(name)?;
                if !engine_map.contains_key(&engine_id) {
                    return Err(SyncManagerError::UnsupportedFeature(name.to_string()));
                }
                selected_engine_ids.insert(engine_id);
            }
            // Filter engines based on the selection
            engine_map.retain(|engine_id, _| selected_engine_ids.contains(engine_id))
        } else {
            // The shared prefs transport is opt-in per run. `All` means all
            // ordinary engines; a Floorp caller must name `prefs` explicitly.
            engine_map.remove(&SyncEngineId::Prefs);
        }
        Ok(engine_map.into_values().collect())
    }
}

fn backoff_in_effect(next_sync_after: Option<SystemTime>, p: &SyncParams) -> bool {
    let now = SystemTime::now();
    if let Some(nsa) = next_sync_after {
        if nsa > now {
            return if matches!(p.reason, SyncReason::User | SyncReason::EnabledChange) {
                info!(
                    "Still under backoff, but syncing anyway because reason is {:?}",
                    p.reason
                );
                false
            } else if !p.enabled_changes.is_empty() {
                info!("Still under backoff, but syncing because we have enabled state changes.");
                false
            } else {
                info!("Still under backoff, and there's no compelling reason for us to ignore it");
                true
            };
        }
    }
    debug!("Not under backoff");
    false
}

impl From<sync15::client::ServiceStatus> for ServiceStatus {
    fn from(s15s: sync15::client::ServiceStatus) -> Self {
        use sync15::client::ServiceStatus::*;
        match s15s {
            Ok => ServiceStatus::Ok,
            NetworkError => ServiceStatus::NetworkError,
            ServiceError => ServiceStatus::ServiceError,
            AuthenticationError => ServiceStatus::AuthError,
            BackedOff => ServiceStatus::BackedOff,
            Interrupted => ServiceStatus::OtherError, // Eh...
            OtherError => ServiceStatus::OtherError,
        }
    }
}

struct SyncClient(Settings);

impl SyncClient {
    pub fn new(settings: Settings) -> SyncClient {
        SyncClient(settings)
    }
}

impl CommandProcessor for SyncClient {
    fn settings(&self) -> &Settings {
        &self.0
    }

    fn apply_incoming_command(&self, command: Command) -> anyhow::Result<CommandStatus> {
        let result = match command {
            Command::Wipe(engine) => wipe(&engine),
            Command::Reset(engine) => reset(&engine),
            Command::ResetAll => reset_all(),
        };
        match result {
            Ok(()) => Ok(CommandStatus::Applied),
            Err(err) => match err {
                SyncManagerError::UnknownEngine(_) => Ok(CommandStatus::Unsupported),
                _ => Err(err.into()),
            },
        }
    }

    fn fetch_outgoing_commands(&self) -> anyhow::Result<HashSet<Command>> {
        Ok(HashSet::new())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;

    #[derive(Default)]
    struct PrefsBoundaryDelegate {
        association_resets: AtomicUsize,
        reject_association_reset: AtomicBool,
    }

    impl floorp_prefs_sync::FloorpPrefsSyncDelegate for PrefsBoundaryDelegate {
        fn prepare(
            &self,
            _input: floorp_prefs_sync::FloorpPrefsSyncPrepareInput,
        ) -> floorp_prefs_sync::Result<floorp_prefs_sync::FloorpPrefsSyncPlan> {
            Err(floorp_prefs_sync::FloorpPrefsSyncError::DelegateRejected)
        }

        fn sync_finished(
            &self,
            _finish: floorp_prefs_sync::FloorpPrefsSyncFinish,
        ) -> floorp_prefs_sync::Result<()> {
            Ok(())
        }

        fn sync_state_changed(
            &self,
            _state: floorp_prefs_sync::FloorpPrefsSyncState,
        ) -> floorp_prefs_sync::Result<()> {
            Ok(())
        }

        fn association_reset(
            &self,
            _state: floorp_prefs_sync::FloorpPrefsSyncState,
        ) -> floorp_prefs_sync::Result<()> {
            self.association_resets.fetch_add(1, Ordering::Relaxed);
            if self.reject_association_reset.load(Ordering::Relaxed) {
                return Err(floorp_prefs_sync::FloorpPrefsSyncError::DelegateRejected);
            }
            Ok(())
        }
    }

    #[test]
    fn test_engine_id_sanity() {
        for engine_id in SyncEngineId::iter() {
            assert_eq!(engine_id, SyncEngineId::try_from(engine_id.name()).unwrap());
        }
    }

    #[test]
    fn prefs_enablement_changes_are_always_rejected() {
        let mut changes = HashMap::new();
        changes.insert(floorp_prefs_sync::PREFS_COLLECTION_NAME.to_string(), true);
        assert!(matches!(
            SyncManager::validate_enabled_changes(&changes),
            Err(SyncManagerError::UnsupportedFeature(_))
        ));

        changes.insert(floorp_prefs_sync::PREFS_COLLECTION_NAME.to_string(), false);
        assert!(matches!(
            SyncManager::validate_enabled_changes(&changes),
            Err(SyncManagerError::UnsupportedFeature(_))
        ));
    }

    #[test]
    fn prefs_is_hidden_from_toggles_but_explicitly_selectable_and_locally_resettable() {
        let delegate = Arc::new(PrefsBoundaryDelegate::default());
        let foreign_delegate: Arc<dyn floorp_prefs_sync::FloorpPrefsSyncDelegate> =
            delegate.clone();
        let store =
            Arc::new(floorp_prefs_sync::FloorpPrefsSyncStore::new(foreign_delegate, None).unwrap());
        Arc::clone(&store).register_with_sync_manager();

        let manager = Arc::new(SyncManager::new());
        assert!(!manager
            .get_available_engines()
            .iter()
            .any(|engine| engine == floorp_prefs_sync::PREFS_COLLECTION_NAME));
        let selected = manager
            .calc_engines_to_sync(&SyncEngineSelection::Some {
                engines: vec![floorp_prefs_sync::PREFS_COLLECTION_NAME.to_string()],
            })
            .unwrap();
        assert!(selected.len() == 1);
        assert!(selected[0].collection_name().as_ref() == floorp_prefs_sync::PREFS_COLLECTION_NAME);
        let all = manager
            .calc_engines_to_sync(&SyncEngineSelection::All)
            .unwrap();
        assert!(!all
            .iter()
            .any(|engine| engine.collection_name().as_ref()
                == floorp_prefs_sync::PREFS_COLLECTION_NAME));

        manager
            .reset(floorp_prefs_sync::PREFS_COLLECTION_NAME)
            .unwrap();
        manager
            .wipe(floorp_prefs_sync::PREFS_COLLECTION_NAME)
            .unwrap();
        assert!(delegate.association_resets.load(Ordering::Relaxed) == 2);

        // Hold the same operation guard used by `sync` and prove that a
        // concurrent disconnect cannot reset prefs until the run releases it.
        let operation = manager.operation.lock();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let manager_for_disconnect = Arc::clone(&manager);
        let disconnect = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            manager_for_disconnect.disconnect();
            done_tx.send(()).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        assert!(delegate.association_resets.load(Ordering::Relaxed) == 2);
        drop(operation);
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        disconnect.join().unwrap();
        assert!(delegate.association_resets.load(Ordering::Relaxed) == 3);

        // The legacy method remains non-throwing, while the checked API makes
        // a foreign delegate's persistence rejection visible to Swift.
        delegate
            .reject_association_reset
            .store(true, Ordering::Relaxed);
        let state_before_failed_disconnect = store.sync_state();
        let error = manager.disconnect_checked().unwrap_err();
        let SyncManagerError::AnyhowError(source) = &error else {
            panic!("unexpected checked disconnect error: {error}");
        };
        assert!(source.chain().any(|cause| {
            matches!(
                cause.downcast_ref::<floorp_prefs_sync::FloorpPrefsSyncError>(),
                Some(floorp_prefs_sync::FloorpPrefsSyncError::DelegateRejected)
            )
        }));
        assert!(error.to_string().contains("Failed to reset prefs"));
        assert!(delegate.association_resets.load(Ordering::Relaxed) == 4);
        assert!(store.sync_state() == state_before_failed_disconnect);
    }
}

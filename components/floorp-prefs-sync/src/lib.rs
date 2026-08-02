/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#![allow(unknown_lints)]
#![warn(rust_2018_idioms)]

mod engine;

pub use engine::{
    get_registered_sync_engine, FloorpPrefsRemoteNotes, FloorpPrefsSyncDelegate,
    FloorpPrefsSyncFinish, FloorpPrefsSyncPlan, FloorpPrefsSyncPrepareInput, FloorpPrefsSyncState,
    FloorpPrefsSyncStore, CONTROL_PREF_NAME, NOTES_PREF_NAME, PREFS_COLLECTION_NAME,
    PREFS_RECORD_ID,
};

uniffi::setup_scaffolding!("floorp_prefs_sync");

pub type Result<T> = std::result::Result<T, FloorpPrefsSyncError>;

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum FloorpPrefsSyncError {
    #[error("The persisted Floorp preferences Sync state is invalid")]
    InvalidPersistedState,
    #[error("The incoming Floorp preferences record is invalid")]
    InvalidIncomingRecord,
    #[error("More than one target Floorp preferences record was received")]
    DuplicateTargetRecord,
    #[error("The remote Floorp Notes preference has an unsupported value type")]
    InvalidRemoteNotesValue,
    #[error("The Floorp preferences Sync payload exceeds the safe record limit")]
    PayloadTooLarge,
    #[error("The Floorp Notes sync delegate returned an invalid preparation")]
    InvalidPreparation,
    #[error("The Floorp preferences record upload was not confirmed")]
    UploadNotConfirmed,
    #[error("The Floorp preferences engine is in an unexpected transaction state")]
    UnexpectedSyncState,
    #[error("The embedding application rejected the Floorp preferences sync transaction")]
    DelegateRejected,
}

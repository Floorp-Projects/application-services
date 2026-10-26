/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

mod model;
mod store;
pub use store::{get_registered_sync_engine, FloorpWorkspacesStore};
pub const COLLECTION_NAME: &str = "floorp-workspaces";
pub const ENGINE_VERSION: usize = 1;
uniffi::setup_scaffolding!("floorp_workspaces_sync");
pub type Result<T> = std::result::Result<T, FloorpWorkspacesError>;

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum FloorpWorkspacesError {
    #[error("Workspace cache could not be read or durably saved")]
    Storage,
    #[error("Workspace cache schema is invalid or unsupported")]
    InvalidCache,
    #[error("Invalid or unsupported Desktop workspace record")]
    InvalidRecord,
    #[error("Workspace run no longer belongs to the current account or collection")]
    StaleRun,
    #[error("Workspace reception is not active")]
    Inactive,
    #[error("A read-only workspace engine attempted an upload")]
    UnexpectedUpload,
}

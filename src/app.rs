//! What the app keeps about repositories. Called by `sync`.

use octocrab::{Octocrab, models::RepositoryId, secrecy::SecretString};
use tracing::info;

use crate::sync::Result;

/// Read from GitHub at the start of each sync.
#[expect(dead_code, reason = "the `sync` stub reads only the description")]
pub struct Context {
    pub repository: RepositoryId,
    pub full_name: String,
    pub default_branch: String,
    /// Acts as the installation.
    pub github: Octocrab,
    /// An installation access token, for git over HTTPS.
    pub token: SecretString,
}

/// Must be idempotent. Never runs twice at once for one repository.
pub async fn sync(context: &Context) -> Result {
    info!(
        repository = context.repository.0,
        full_name = context.full_name,
        default_branch = context.default_branch,
        "synced"
    );
    Ok(())
}

pub async fn forget(repository: RepositoryId) -> Result {
    info!(repository = repository.0, "forgot");
    Ok(())
}

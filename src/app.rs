//! What the app keeps about repositories. Called by `sync`.

use octocrab::{Octocrab, models::RepositoryId, secrecy::SecretString};
use tracing::info;

use crate::{App, Result, checkout};

/// Read from GitHub at the start of each sync.
#[expect(dead_code, reason = "later steps read the rest")]
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
pub async fn sync(app: &App, context: &Context) -> Result {
    let url = format!("https://github.com/{}.git", context.full_name);
    let token = context.token.clone();
    checkout::update(&app.config.checkouts, context.repository, url, token).await?;
    info!(repository = context.repository.0, "synced");
    Ok(())
}

pub async fn forget(app: &App, repository: RepositoryId) -> Result {
    checkout::remove(&app.config.checkouts, repository).await?;
    info!(repository = repository.0, "forgot");
    Ok(())
}

//! What the app keeps about repositories. Called by `sync`.

use octocrab::{models::RepositoryId, secrecy::SecretString};
use tracing::info;

use crate::{App, Result, checkout::Checkout};

/// Read from GitHub at the start of each sync.
pub struct Context {
    pub repository: RepositoryId,
    pub full_name: String,
    /// An installation access token, for git over HTTPS.
    pub token: SecretString,
}

/// Must be idempotent. Never runs twice at once for one repository.
pub async fn sync(app: &App, context: &Context) -> Result {
    let url = format!("https://github.com/{}.git", context.full_name);
    Checkout::new(&app.config.checkouts, context.repository)
        .update(url, context.token.clone())
        .await?;
    info!(repository = context.repository.0, "synced");
    Ok(())
}

pub async fn forget(app: &App, repository: RepositoryId) -> Result {
    Checkout::new(&app.config.checkouts, repository)
        .remove()
        .await?;
    info!(repository = repository.0, "forgot");
    Ok(())
}

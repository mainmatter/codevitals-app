//! What the app keeps about repositories. Called by `sync`.

use gix::refs::FullName;
use octocrab::{Octocrab, models::RepositoryId, secrecy::SecretString};
use tracing::info;

use crate::{App, Result, accounts, checkout::Checkout, commits};

/// Read from GitHub at the start of each sync.
pub struct Context {
    pub repository: RepositoryId,
    pub full_name: String,
    /// The one tracked branch.
    pub default_branch: String,
    /// Acting as the installation that sees the repository.
    pub github: Octocrab,
    /// An installation access token, for git over HTTPS.
    pub token: SecretString,
}

/// Must be idempotent. Never runs twice at once for one repository.
pub async fn sync(app: &App, context: &Context) -> Result {
    let checkout = Checkout::new(&app.config.checkouts, context.repository);
    let url = format!("https://github.com/{}.git", context.full_name);
    let refs = checkout.update(url, context.token.clone()).await?;

    // An empty repository has no branch yet.
    let branch = FullName::try_from(format!("refs/heads/{}", context.default_branch))?;
    let tips = Vec::from_iter(refs.get(&branch).copied());
    let commits = commits::record(&app.db, &checkout, tips).await?;
    accounts::record(&context.github, &app.db, &context.full_name, &commits).await?;
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

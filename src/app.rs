//! What the app keeps about repositories. Called by `sync`.

use std::iter;

use gix::refs::FullName;
use octocrab::{Octocrab, models::RepositoryId, secrecy::SecretString};
use tracing::info;

use crate::{App, Result, accounts, checkout::Checkout, commits, mainline};

/// Read from GitHub at the start of each sync.
pub struct Context {
    pub repository: RepositoryId,
    pub full_name: String,
    /// Tracked, as are the branches configured for the repository.
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
    sqlx::query("INSERT INTO repo VALUES ($1, $2) ON CONFLICT (id) DO UPDATE SET full_name = $2")
        .bind(context.repository.0 as i64)
        .bind(&context.full_name)
        .execute(&app.db)
        .await?;

    // A tracked branch may not exist yet, or any more.
    let configured = (app.config.tracked_branches.iter())
        .filter(|(repository, _)| *repository == context.repository)
        .map(|(_, branch)| branch);
    let mut tips = Vec::new();
    for branch in iter::once(&context.default_branch).chain(configured) {
        let name = FullName::try_from(format!("refs/heads/{branch}"))?;
        if let Some(&tip) = refs.get(&name) {
            tips.push((branch.clone(), tip));
        }
    }

    let (db, repository) = (app.db.clone(), context.repository);
    let commits = checkout.read(move |repo| {
        let commits = commits::record(&db, repo, tips.iter().map(|(_, tip)| *tip).collect())?;
        mainline::record(&db, repo, repository, &tips)?;
        Ok(commits)
    });
    let commits = commits.await?;
    accounts::record(&context.github, &app.db, &context.full_name, &commits).await?;
    info!(repository = context.repository.0, "synced");
    Ok(())
}

/// Its rows go with it.
pub async fn forget(app: &App, repository: RepositoryId) -> Result {
    sqlx::query("DELETE FROM repo WHERE id = $1")
        .bind(repository.0 as i64)
        .execute(&app.db)
        .await?;
    Checkout::new(&app.config.checkouts, repository)
        .remove()
        .await?;
    info!(repository = repository.0, "forgot");
    Ok(())
}

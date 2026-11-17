//! What the app keeps about repositories. Called by `sync`.

use std::iter;

use gix::ObjectId;
use octocrab::{Octocrab, models::RepositoryId, secrecy::SecretString};
use sqlx::PgPool;
use tracing::info;

use crate::{
    App, Result, accounts, commits, facts, mainline,
    mirror::{self, Mirror, Refs},
    pull_heads, pulls,
};

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
    let (db, repository) = (&app.db, context.repository);
    let mirror = Mirror::new(&app.config.mirrors, repository);
    let url = format!("https://github.com/{}.git", context.full_name);
    let refs = mirror.update(url, context.token.clone()).await?;
    sqlx::query("INSERT INTO repo VALUES ($1, $2) ON CONFLICT (id) DO UPDATE SET full_name = $2")
        .bind(repository.0 as i64)
        .bind(&context.full_name)
        .execute(db)
        .await?;

    pulls::record(&context.github, db, repository, &context.full_name).await?;
    let branches = tracked_branches(app, context, &refs)?;
    let commits = record(db, &mirror, repository, &refs, &branches).await?;
    accounts::record(&context.github, db, &context.full_name, &commits).await?;
    info!(repository = repository.0, "synced");
    Ok(())
}

/// Names and tips. A configured branch may not exist yet, or any more.
fn tracked_branches(app: &App, context: &Context, refs: &Refs) -> Result<Vec<(String, ObjectId)>> {
    let configured = (app.config.tracked_branches.iter())
        .filter(|(repository, _)| *repository == context.repository)
        .map(|(_, branch)| branch);
    let mut branches = Vec::new();
    for name in iter::once(&context.default_branch).chain(configured) {
        if let Some(tip) = mirror::branch(refs, name)? {
            branches.push((name.clone(), tip));
        }
    }
    Ok(branches)
}

/// Everything derived from the mirror alone. Returns the commits its branches and pull requests
/// reach.
pub async fn record(
    db: &PgPool,
    mirror: &Mirror,
    repository: RepositoryId,
    refs: &Refs,
    branches: &[(String, ObjectId)],
) -> Result<Vec<ObjectId>> {
    let tips = branches
        .iter()
        .map(|(_, tip)| *tip)
        .chain(mirror::pull_heads(refs));
    let commits = commits::record(db, mirror, tips.collect()).await?;
    mainline::record(db, mirror, repository, branches).await?;
    pull_heads::record(db, mirror, repository, refs).await?;
    facts::record(db, mirror, repository).await?;
    Ok(commits)
}

/// Its rows go with it.
pub async fn forget(app: &App, repository: RepositoryId) -> Result {
    sqlx::query("DELETE FROM repo WHERE id = $1")
        .bind(repository.0 as i64)
        .execute(&app.db)
        .await?;
    Mirror::new(&app.config.mirrors, repository)
        .remove()
        .await?;
    info!(repository = repository.0, "forgot");
    Ok(())
}

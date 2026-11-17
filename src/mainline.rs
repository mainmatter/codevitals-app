//! Each tracked branch's first-parent history, as `mainline` rows, and its changes.
//!
//! Rows of branches no longer tracked are deleted. Then, for each tracked branch whose rows do not
//! end at its tip:
//!
//! 1. [`first_parents`]: its line from the root to the tip.
//! 2. [`changes::record`] each commit on it against its first parent.
//! 3. [`replace`] its rows with the line.
//!
//! A tip determines its line, so rows ending at the tip are already the line. Rows are replaced
//! whole, so a force-push is no special case and readers see the old line or the new one.

use gix::ObjectId;
use sqlx::PgPool;

use crate::{
    Result,
    changes::{self, CommitPair},
    column, limits,
    mirror::Mirror,
};

pub async fn record(
    db: &PgPool,
    mirror: &Mirror,
    repo: i64,
    branches: &[(String, ObjectId)],
) -> Result<usize> {
    let names: Vec<&str> = branches.iter().map(|(name, _)| name.as_str()).collect();
    sqlx::query("DELETE FROM mainline WHERE repo = $1 AND branch <> ALL($2)")
        .bind(repo)
        .bind(&names)
        .execute(db)
        .await?;

    let mut replaced = 0;
    for (branch, tip) in branches {
        let last: Option<Vec<u8>> = sqlx::query_scalar(
            "SELECT commit FROM mainline WHERE repo = $1 AND branch = $2 ORDER BY seq DESC LIMIT 1",
        )
        .bind(repo)
        .bind(branch)
        .fetch_optional(db)
        .await?;
        if last.as_deref() == Some(tip.as_slice()) {
            continue;
        }
        let tip = *tip;
        let line = mirror.run(move |git| first_parents(git, tip)).await?;
        changes::record(db, mirror, &line).await?;
        replace(db, repo, branch, &line).await?;
        replaced += 1;
    }
    Ok(replaced)
}

/// From the root to `tip`, each commit paired with its first parent.
fn first_parents(git: &gix::Repository, tip: ObjectId) -> Result<Vec<CommitPair>> {
    let walk = git.rev_walk([tip]).first_parent_only().all()?;
    let pairs = walk.map(|commit| {
        let commit = commit?;
        let base = commit.parent_ids.first().copied();
        Ok(CommitPair {
            base,
            head: commit.id,
        })
    });
    let mut line = limits::collect(pairs, limits::COMMITS, "commits on a first-parent line")?;
    line.reverse();
    Ok(line)
}

/// One transaction.
async fn replace(db: &PgPool, repo: i64, branch: &str, line: &[CommitPair]) -> Result {
    let mut transaction = db.begin().await?;
    sqlx::query("DELETE FROM mainline WHERE repo = $1 AND branch = $2")
        .bind(repo)
        .bind(branch)
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "INSERT INTO mainline
         SELECT $1, $2, (seq - 1)::int, commit FROM unnest($3::bytea[]) WITH ORDINALITY AS c (commit, seq)",
    )
    .bind(repo)
    .bind(branch)
    .bind(column(line, |pair| pair.head.as_slice()))
    .execute(&mut *transaction)
    .await?;
    Ok(transaction.commit().await?)
}

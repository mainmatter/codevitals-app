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
use octocrab::models::RepositoryId;
use sqlx::PgPool;

use crate::{
    Result,
    changes::{self, CommitPair},
    column, limits,
    mirror::Mirror,
};

/// The commits of `branches` must have rows.
pub async fn record(
    db: &PgPool,
    mirror: &Mirror,
    repository: RepositoryId,
    branches: &[(String, ObjectId)],
) -> Result {
    let repository = repository.0 as i64;
    let names: Vec<&str> = branches.iter().map(|(name, _)| name.as_str()).collect();
    sqlx::query("DELETE FROM mainline WHERE repo = $1 AND branch <> ALL($2)")
        .bind(repository)
        .bind(&names)
        .execute(db)
        .await?;

    for (branch, tip) in branches {
        let last: Option<Vec<u8>> = sqlx::query_scalar(
            "SELECT commit FROM mainline WHERE repo = $1 AND branch = $2 ORDER BY seq DESC LIMIT 1",
        )
        .bind(repository)
        .bind(branch)
        .fetch_optional(db)
        .await?;
        if last.as_deref() == Some(tip.as_slice()) {
            continue;
        }
        let tip = *tip;
        let line = mirror.run(move |repo| first_parents(repo, tip)).await?;
        changes::record(db, mirror, &line).await?;
        replace(db, repository, branch, &line).await?;
    }
    Ok(())
}

/// From the root to `tip`, each commit paired with its first parent.
fn first_parents(repo: &gix::Repository, tip: ObjectId) -> Result<Vec<CommitPair>> {
    let walk = repo.rev_walk([tip]).first_parent_only().all()?;
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
async fn replace(db: &PgPool, repository: i64, branch: &str, line: &[CommitPair]) -> Result {
    let mut transaction = db.begin().await?;
    sqlx::query("DELETE FROM mainline WHERE repo = $1 AND branch = $2")
        .bind(repository)
        .bind(branch)
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "INSERT INTO mainline
         SELECT $1, $2, (seq - 1)::int, commit FROM unnest($3::bytea[]) WITH ORDINALITY AS c (commit, seq)",
    )
    .bind(repository)
    .bind(branch)
    .bind(column(line, |pair| pair.head.as_slice()))
    .execute(&mut *transaction)
    .await?;
    Ok(transaction.commit().await?)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, os::unix::fs::PermissionsExt};

    use super::*;
    use crate::fixtures::{
        self, EMPTY_TREE, assert_changes_match_git, commit, git, repo, rows, scratch, submodule,
        write,
    };

    /// `main` and `side` go through ordinary pushes, replaced commits and a fast-forward that
    /// reroutes `main`'s first-parent line. Then `side` is no longer tracked.
    #[sqlx::test]
    async fn every_seq_holds_what_git_lists(db: PgPool) {
        repo(&db).await;
        let mirror = Mirror(scratch("mainline"));
        let work = &mirror.0;
        let both = ["main", "side"];

        // Ordinary pushes: every kind of entry, a mode change, a directory replaced by a file, an
        // empty commit and a merge.
        git(work, &["init", "--initial-branch=main"]);
        git(work, &["commit", "--allow-empty", "-m", "root"]);
        let root = git(work, &["rev-parse", "HEAD"]);
        write(work, "a.txt", "1\n2\n3\n");
        write(work, "dir/b.txt", "b\n");
        write(work, "run.sh", "#!/bin/sh\n");
        fs::set_permissions(work.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        write(work, "binary", "\0\x01\x02\n");
        std::os::unix::fs::symlink("a.txt", work.join("link")).unwrap();
        submodule(work, &root);
        commit(work, "everything");
        git(work, &["branch", "side"]);
        write(work, "a.txt", "1\ntwo\n3\n4\n");
        fs::remove_dir_all(work.join("dir")).unwrap();
        write(work, "dir", "now a file\n");
        fs::set_permissions(work.join("run.sh"), fs::Permissions::from_mode(0o644)).unwrap();
        write(work, "binary", "\0\x03\n");
        let everything = git(work, &["rev-parse", "HEAD"]);
        submodule(work, &everything);
        commit(work, "rewrite");
        git(work, &["commit", "--allow-empty", "-m", "nothing"]);
        git(work, &["switch", "side"]);
        write(work, "side.txt", "side\n");
        commit(work, "side");
        git(work, &["switch", "main"]);
        git(work, &["merge", "--no-ff", "-m", "merge side", "side"]);
        sync_and_check(&db, &mirror, &both).await;

        // Replaced commits, the tip among them.
        git(work, &["reset", "--hard", "HEAD~2"]);
        write(work, "a.txt", "replaced\n");
        commit(work, "replaced 1");
        write(work, "c.txt", "c\n");
        commit(work, "replaced 2");
        sync_and_check(&db, &mirror, &both).await;

        // `side` merges `main`, then `main` fast-forwards to it: `main`'s first parents now run
        // through `side`.
        git(work, &["switch", "side"]);
        write(work, "side.txt", "side 2\n");
        commit(work, "side 2");
        git(work, &["merge", "--no-ff", "-m", "merge main", "main"]);
        git(work, &["switch", "main"]);
        git(work, &["merge", "--ff-only", "side"]);
        sync_and_check(&db, &mirror, &both).await;

        sync_and_check(&db, &mirror, &["main"]).await;
        let side = rows(&db, "SELECT branch FROM mainline WHERE branch = 'side'");
        assert!(side.await.is_empty());
        fs::remove_dir_all(work).unwrap();
    }

    async fn sync_and_check(db: &PgPool, mirror: &Mirror, names: &[&str]) {
        let work = &mirror.0;
        fixtures::record(db, mirror, names).await;

        for &branch in names {
            // Each commit on the line, after its first parent.
            let line = git(
                work,
                &[
                    "rev-list",
                    "--first-parent",
                    "--reverse",
                    "--parents",
                    branch,
                ],
            );
            let pairs = Vec::from_iter(line.lines().map(|line| {
                let mut ids = line.split(' ');
                let commit = ids.next().unwrap().to_owned();
                (ids.next().unwrap_or(EMPTY_TREE).to_owned(), commit)
            }));

            let mainline = format!(
                "SELECT seq || ' ' || encode(commit, 'hex') FROM mainline WHERE branch = '{branch}'"
            );
            let listed = pairs
                .iter()
                .enumerate()
                .map(|(seq, (_, commit))| format!("{seq} {commit}"));
            assert_eq!(rows(db, &mainline).await, sorted(listed));

            let tree = format!(
                "SELECT concat_ws(' ', encode(commit, 'hex'),
                    CASE mode WHEN 'file' THEN '100644' WHEN 'executable' THEN '100755'
                        WHEN 'symlink' THEN '120000' ELSE '160000' END,
                    encode(oid, 'hex'), path)
                FROM tree JOIN mainline m USING (repo, branch)
                WHERE branch = '{branch}' AND seqs @> m.seq"
            );
            let listed = pairs.iter().flat_map(|(_, commit)| {
                let format = format!("--format={commit} %(objectmode) %(objectname) %(path)");
                let listed = git(work, &["ls-tree", "-r", &format, commit]);
                listed.lines().map(String::from).collect::<Vec<_>>()
            });
            assert_eq!(rows(db, &tree).await, sorted(listed));
            assert_changes_match_git(db, work, &pairs).await;
        }
    }

    fn sorted(lines: impl Iterator<Item = String>) -> Vec<String> {
        Vec::from_iter(BTreeSet::from_iter(lines))
    }
}

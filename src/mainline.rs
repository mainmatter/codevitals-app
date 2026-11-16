//! Each tracked branch's first-parent history, as `mainline` rows, and its changes.
//!
//! Rows of branches no longer tracked are deleted. Then, for each tracked branch whose rows do
//! not end at its tip:
//!
//! 1. [`first_parents`]: its line from root to tip.
//! 2. [`changes::record`] each commit on the line against its first parent.
//! 3. [`replace`] its rows with the line, in one transaction.
//!
//! Rows are replaced whole, never patched, so a force-push is no special case and readers see the
//! old line or the new one. Changes are keyed by content, so a replaced line diffs only commits
//! it has not seen.

use gix::ObjectId;
use octocrab::models::RepositoryId;
use sqlx::PgPool;

use crate::{
    Result,
    changes::{self, Pair},
    limits, wait,
};

/// `tips` holds each tracked branch the repository has; their commits must have rows.
pub fn record(
    db: &PgPool,
    repo: &gix::Repository,
    repository: RepositoryId,
    tips: &[(String, ObjectId)],
) -> Result {
    let repository = repository.0 as i64;
    let branches: Vec<&str> = tips.iter().map(|(branch, _)| branch.as_str()).collect();
    let untracked = sqlx::query("DELETE FROM mainline WHERE repo = $1 AND branch <> ALL($2)")
        .bind(repository)
        .bind(&branches);
    wait(untracked.execute(db))?;

    for (branch, tip) in tips {
        let last = sqlx::query_scalar(
            "SELECT commit FROM mainline WHERE repo = $1 AND branch = $2 ORDER BY seq DESC LIMIT 1",
        )
        .bind(repository)
        .bind(branch);
        let last: Option<Vec<u8>> = wait(last.fetch_optional(db))?;
        if last.as_deref() == Some(tip.as_slice()) {
            continue;
        }
        let line = first_parents(repo, *tip)?;
        changes::record(db, repo, &line)?;
        wait(replace(db, repository, branch, &line))?;
    }
    Ok(())
}

/// From the root to `tip`, each commit paired with its first parent.
fn first_parents(repo: &gix::Repository, tip: ObjectId) -> Result<Vec<Pair>> {
    let walk = repo.rev_walk([tip]).first_parent_only().all()?;
    let mut line = Vec::new();
    for info in walk.take(limits::COMMITS + 1) {
        let info = info?;
        let base = info.parent_ids.first().copied();
        line.push(Pair {
            base,
            head: info.id,
        });
    }
    if line.len() > limits::COMMITS {
        let max = limits::COMMITS;
        return Err(format!("more than {max} commits on a first-parent line").into());
    }
    line.reverse();
    Ok(line)
}

async fn replace(db: &PgPool, repository: i64, branch: &str, line: &[Pair]) -> Result {
    let commits: Vec<&[u8]> = line.iter().map(|pair| pair.head.as_slice()).collect();
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
    .bind(&commits)
    .execute(&mut *transaction)
    .await?;
    Ok(transaction.commit().await?)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, os::unix::fs::PermissionsExt, path::Path};

    use super::*;
    use crate::{
        checkout::{
            Checkout,
            tests::{git, scratch},
        },
        commits,
    };

    const REPOSITORY: RepositoryId = RepositoryId(1);

    /// `main` and `side` go through ordinary pushes, an amended tip, replaced commits, a
    /// fast-forward that reroutes `main`'s first-parent line, and unrelated history. Then `side`
    /// is no longer tracked.
    #[sqlx::test]
    async fn every_seq_holds_what_git_lists(db: PgPool) {
        sqlx::query("INSERT INTO repo VALUES (1, 'o/r')")
            .execute(&db)
            .await
            .unwrap();
        let checkout = Checkout(scratch("mainline"));
        let work = &checkout.0;
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
        sync_and_check(&db, &checkout, &both).await;

        // An amended tip.
        write(work, "a.txt", "amended\n");
        git(work, &["commit", "--amend", "-a", "-m", "amended merge"]);
        sync_and_check(&db, &checkout, &both).await;

        // Replaced commits.
        git(work, &["reset", "--hard", "HEAD~2"]);
        write(work, "a.txt", "replaced\n");
        commit(work, "replaced 1");
        write(work, "c.txt", "c\n");
        commit(work, "replaced 2");
        sync_and_check(&db, &checkout, &both).await;

        // `side` merges `main`, then `main` fast-forwards to it: `main`'s first parents now run
        // through `side`.
        git(work, &["switch", "side"]);
        write(work, "side.txt", "side 2\n");
        commit(work, "side 2");
        git(work, &["merge", "--no-ff", "-m", "merge main", "main"]);
        git(work, &["switch", "main"]);
        git(work, &["merge", "--ff-only", "side"]);
        sync_and_check(&db, &checkout, &both).await;

        // Unrelated history.
        git(work, &["switch", "--orphan", "unrelated"]);
        write(work, "new.txt", "new\n");
        commit(work, "unrelated");
        git(work, &["branch", "--force", "main", "unrelated"]);
        sync_and_check(&db, &checkout, &both).await;

        sync_and_check(&db, &checkout, &["main"]).await;
        let side = rows(&db, "SELECT branch FROM mainline WHERE branch = $1", "side");
        assert!(side.await.is_empty());
        fs::remove_dir_all(work).unwrap();
    }

    async fn sync_and_check(db: &PgPool, checkout: &Checkout, branches: &[&str]) {
        let work = &checkout.0;
        let tips: Vec<(String, ObjectId)> = (branches.iter())
            .map(|&branch| {
                (
                    branch.into(),
                    git(work, &["rev-parse", branch]).parse().unwrap(),
                )
            })
            .collect();
        let pool = db.clone();
        let read = checkout.read(move |repo| {
            commits::record(&pool, repo, tips.iter().map(|(_, tip)| *tip).collect())?;
            record(&pool, repo, REPOSITORY, &tips)
        });
        read.await.unwrap();

        for &branch in branches {
            let line = git(work, &["rev-list", "--first-parent", "--reverse", branch]);
            let line: Vec<&str> = line.lines().collect();

            let mainline = rows(
                db,
                "SELECT seq || ' ' || encode(commit, 'hex') FROM mainline WHERE branch = $1",
                branch,
            );
            let listed = line
                .iter()
                .enumerate()
                .map(|(seq, commit)| format!("{seq} {commit}"));
            assert_eq!(mainline.await, listed.collect(), "{branch}");

            let tree = "SELECT concat_ws(' ', encode(commit, 'hex'),
                    CASE mode WHEN 'file' THEN '100644' WHEN 'executable' THEN '100755'
                        WHEN 'symlink' THEN '120000' ELSE '160000' END,
                    encode(oid, 'hex'), path)
                FROM tree JOIN mainline m USING (repo, branch) WHERE branch = $1 AND seqs @> m.seq";
            let listed = line.iter().flat_map(|commit| {
                let format = format!("--format={commit} %(objectmode) %(objectname) %(path)");
                let listed = git(work, &["ls-tree", "-r", &format, commit]);
                listed.lines().map(String::from).collect::<Vec<_>>()
            });
            assert_eq!(rows(db, tree, branch).await, listed.collect(), "{branch}");

            // Without submodules, whose `Subproject commit` lines git counts.
            let counted = "SELECT concat_ws(E'\\t', encode(head, 'hex'),
                    coalesce(lines_added::text, '-'), coalesce(lines_deleted::text, '-'), path)
                FROM mainline JOIN commit ON sha = commit
                JOIN change ON head = commit AND base IS NOT DISTINCT FROM parents[1]
                WHERE branch = $1 AND 'submodule' NOT IN (coalesce(old_mode, ''), coalesce(new_mode, ''))";
            let log = git(
                work,
                &["log", "--first-parent", "--numstat", "--format=%H", branch],
            );
            let (mut commit, mut numstat) = ("", BTreeSet::new());
            for line in log
                .lines()
                .filter(|line| !line.is_empty() && !line.ends_with("\tsub"))
            {
                if line.contains('\t') {
                    numstat.insert(format!("{commit}\t{line}"));
                } else {
                    commit = line;
                }
            }
            assert_eq!(rows(db, counted, branch).await, numstat, "{branch}");
        }
    }

    fn write(work: &Path, path: &str, content: &str) {
        let path = work.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn submodule(work: &Path, commit: &str) {
        let info = format!("160000,{commit},sub");
        git(work, &["update-index", "--add", "--cacheinfo", &info]);
    }

    /// Stages every file, but not the deletion of `sub`, which has no directory.
    fn commit(work: &Path, message: &str) {
        git(work, &["add", "--all", "--", ".", ":!sub"]);
        git(work, &["commit", "-m", message]);
    }

    async fn rows(db: &PgPool, sql: &str, branch: &str) -> BTreeSet<String> {
        let rows = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(branch)
            .fetch_all(db);
        rows.await.unwrap().into_iter().collect()
    }
}

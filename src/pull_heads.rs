//! Each head a pull request is seen with, as a `pull_head` row, with the commits after its
//! merge-base and the diff from its merge-base to it.
//!
//! 1. [`missing`]: each pull request's head without a row, and its base branch's tip. A head whose
//!    base branch is gone is left out.
//! 2. [`read`] each one's merge-base and the commits after it. A head without a merge-base, whose
//!    history is unrelated, is left out.
//! 3. [`changes::record`] its diff, then [`insert`] its rows, together: a head with a row is
//!    complete.
//!
//! Diffing from the merge-base rather than the base branch's tip keeps movement on the base branch
//! out of the pull request.

use std::{cell::Cell, io};

use gix::{
    ObjectId,
    traverse::commit::{
        Simple,
        simple::{CommitTimeOrder, Sorting},
    },
};
use octocrab::models::RepositoryId;
use sqlx::PgPool;

use crate::{
    Result,
    changes::{self, CommitPair},
    column, limits,
    mirror::{self, Mirror, Refs},
};

/// The commits of the heads must have rows.
pub async fn record(db: &PgPool, mirror: &Mirror, repository: RepositoryId, refs: &Refs) -> Result {
    let repository = repository.0 as i64;
    let missing = missing(db, repository, refs).await?;
    mirror
        .read_each(&missing, read, |head| async move {
            let Some(head) = head else { return Ok(()) };
            let pair = CommitPair {
                base: Some(head.merge_base),
                head: head.head,
            };
            changes::record(db, mirror, &[pair]).await?;
            insert(db, repository, head).await
        })
        .await
}

// 1. Missing

#[derive(Clone)]
struct Missing {
    number: i32,
    head: ObjectId,
    base_tip: ObjectId,
}

async fn missing(db: &PgPool, repository: i64, refs: &Refs) -> Result<Vec<Missing>> {
    let missing: Vec<(i32, String, Vec<u8>)> = sqlx::query_as(
        "SELECT p.number, p.base_ref, p.head FROM pull p
         WHERE p.repo = $1 AND NOT EXISTS (
             SELECT FROM pull_head h WHERE (h.repo, h.number, h.head) = (p.repo, p.number, p.head)
         )
         LIMIT $2",
    )
    .bind(repository)
    .bind(limits::PULLS as i64 + 1)
    .fetch_all(db)
    .await?;
    limits::check(
        missing.len(),
        limits::PULLS,
        "pull request heads without rows",
    )?;
    let mut based = Vec::new();
    for (number, base_ref, head) in missing {
        if let Some(base_tip) = mirror::branch(refs, &base_ref)? {
            let head = ObjectId::from_bytes_or_panic(&head);
            based.push(Missing {
                number,
                head,
                base_tip,
            });
        }
    }
    Ok(based)
}

// 2. Read

struct Head {
    number: i32,
    head: ObjectId,
    merge_base: ObjectId,
    /// Oldest first.
    added: Vec<ObjectId>,
}

/// The commits added are those after the merge-base, as `git rev-list --reverse merge_base..head`
/// lists them. gix finds both, reading through one [`Budget`], which bounds them.
fn read(repo: &gix::Repository, missing: Missing) -> Result<Option<Head>> {
    let budget = Budget(&repo.objects, Cell::new(0));
    let mut graph = gix::revwalk::Graph::new(&budget, None);
    let merge_base = repo.merge_base_with_graph(missing.head, missing.base_tip, &mut graph)?;
    let Some(merge_base) = merge_base.map(|merge_base| merge_base.detach()) else {
        return Ok(None);
    };
    let walk = Simple::new([missing.head], &budget)
        .sorting(Sorting::ByCommitTime(CommitTimeOrder::NewestFirst))?
        .hide([merge_base])?
        .map(|info| Ok(info?.id));
    let mut added = walk.collect::<Result<Vec<_>>>()?;
    added.reverse();
    let (number, head) = (missing.number, missing.head);
    Ok(Some(Head {
        number,
        head,
        merge_base,
        added,
    }))
}

/// The mirror's objects, failing past [`limits::PULL_HEAD_WALK`] reads.
struct Budget<'r>(&'r gix::OdbHandle, Cell<usize>);

impl gix::objs::Find for Budget<'_> {
    fn try_find<'a>(
        &self,
        id: &gix::oid,
        buffer: &'a mut Vec<u8>,
    ) -> gix::error::Result<Option<gix::objs::Data<'a>>> {
        self.1.set(self.1.get() + 1);
        let within = limits::check(self.1.get(), limits::PULL_HEAD_WALK, "objects read");
        within.map_err(|error| gix::error::Error::from_error(io::Error::other(error)))?;
        self.0.try_find(id, buffer)
    }
}

// 3. Insert

/// One statement, so a head and its commits go in together.
async fn insert(db: &PgPool, repository: i64, head: Head) -> Result {
    sqlx::query(
        "WITH head AS (
             INSERT INTO pull_head VALUES ($1, $2, $3, $4, now())
             ON CONFLICT DO NOTHING
             RETURNING repo
         )
         INSERT INTO pull_commit
         SELECT $1, $2, $3, c.commit, (c.position - 1)::int
         FROM head, unnest($5::bytea[]) WITH ORDINALITY AS c (commit, position)",
    )
    .bind(repository)
    .bind(head.number)
    .bind(head.head.as_slice())
    .bind(head.merge_base.as_slice())
    .bind(column(&head.added, |commit| commit.as_slice()))
    .execute(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::*;
    use crate::fixtures::{
        self, assert_changes_match_git, assert_every_blob_has_facts, commit, git, repo, rows,
        scratch, write,
    };

    /// Pull request 1 is opened from `topic`, updated with a merge of `main` and a commit,
    /// force-pushed and merged. Pull request 2's base branch is gone; pull request 3 is unrelated
    /// to its base; pull request 4 is first seen with a head `main` already has.
    #[sqlx::test]
    async fn every_head_seen_has_its_commits_diff_and_facts(db: PgPool) {
        repo(&db).await;
        let mirror = Mirror(scratch("pull-heads"));
        let work = &mirror.0;
        // Each pull request's head and its merge-base when first seen.
        let mut seen = Vec::new();

        git(work, &["init", "--initial-branch=main"]);
        write(work, "a.rs", "fn a() {}\n");
        commit(work, "root");
        git(work, &["switch", "-c", "topic"]);
        write(work, "b.rs", "fn b() {}\n");
        commit(work, "add b");
        git(work, &["switch", "--orphan", "unrelated"]);
        write(work, "u.py", "def u():\n    pass\n");
        commit(work, "unrelated");
        pull(&db, work, 2, "gone", "topic").await;
        pull(&db, work, 3, "main", "unrelated").await;
        open(&db, work, &mut seen, 1).await;
        check(&db, &mirror, &seen).await;

        git(work, &["switch", "main"]);
        write(work, "a.rs", "fn a() { 1; }\n");
        commit(work, "main moves");
        git(work, &["switch", "topic"]);
        git(work, &["merge", "--no-ff", "-m", "merge main", "main"]);
        write(work, "c.c", "int c(void) { return 0; }\n");
        commit(work, "add c");
        open(&db, work, &mut seen, 1).await;
        check(&db, &mirror, &seen).await;

        write(work, "c.c", "int c(void) { return 1; }\n");
        git(
            work,
            &["commit", "--all", "--amend", "-m", "add c, amended"],
        );
        open(&db, work, &mut seen, 1).await;
        check(&db, &mirror, &seen).await;

        // Merging moves `main` past the head: pull request 1's rows stay as first seen.
        git(work, &["switch", "main"]);
        git(work, &["merge", "--no-ff", "-m", "merge topic", "topic"]);
        open(&db, work, &mut seen, 4).await;
        check(&db, &mirror, &seen).await;
        fs::remove_dir_all(work).unwrap();
    }

    /// Points pull request `number`, and its ref on GitHub, at `branch`.
    async fn pull(db: &PgPool, work: &Path, number: i32, base_ref: &str, branch: &str) {
        let head = git(work, &["rev-parse", branch]);
        git(
            work,
            &["update-ref", &format!("refs/pull/{number}/head"), &head],
        );
        sqlx::query(
            "INSERT INTO pull VALUES (1, $1, 'open', false, $2, decode($3, 'hex'), now(), now(),
                 NULL, NULL, NULL, 1, 'ada', 'User')
             ON CONFLICT (repo, number) DO UPDATE SET head = EXCLUDED.head",
        )
        .bind(number)
        .bind(base_ref)
        .bind(head)
        .execute(db)
        .await
        .unwrap();
    }

    /// Points pull request `number` at `topic`, based on `main`.
    async fn open(db: &PgPool, work: &Path, seen: &mut Vec<(i32, String, String)>, number: i32) {
        pull(db, work, number, "main", "topic").await;
        let merge_base = git(work, &["merge-base", "main", "topic"]);
        seen.push((number, merge_base, git(work, &["rev-parse", "topic"])));
    }

    async fn check(db: &PgPool, mirror: &Mirror, seen: &[(i32, String, String)]) {
        fixtures::record(db, mirror, &["main"]).await;
        let (mut heads, mut commits, mut pairs) = (Vec::new(), Vec::new(), Vec::new());
        for (number, merge_base, head) in seen {
            heads.push(format!("{number} {head} {merge_base}"));
            let range = format!("{merge_base}..{head}");
            let added = git(
                &mirror.0,
                &["rev-list", "--topo-order", "--reverse", &range],
            );
            let added = added.lines().enumerate();
            commits.extend(added.map(|(i, commit)| format!("{number} {head} {i} {commit}")));
            pairs.push((merge_base.clone(), head.clone()));
        }
        heads.sort();
        commits.sort();
        let shown = "SELECT concat_ws(' ', number, encode(head, 'hex'), encode(merge_base, 'hex'))
            FROM pull_head";
        assert_eq!(rows(db, shown).await, heads);
        let shown = "SELECT concat_ws(' ', number, encode(head, 'hex'), position,
            encode(commit, 'hex')) FROM pull_commit";
        assert_eq!(rows(db, shown).await, commits);
        assert_changes_match_git(db, &mirror.0, &pairs).await;
        assert_every_blob_has_facts(db).await;
    }
}

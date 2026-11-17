//! Each head a pull request is seen with, as a `pull_head` row, with the commits after its
//! merge-base and the diff from its merge-base to it.
//!
//! 1. [`missing`]: each pull request's head without a row, and its base branch's tip. A head the
//!    repository does not reach yet, listed by GitHub after the fetch, waits for a later sync; one
//!    whose base branch is gone is left out.
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
use sqlx::PgPool;

use crate::{
    Result,
    changes::{self, CommitPair},
    column, limits,
    mirror::{self, Mirror, Refs},
};

pub async fn record(db: &PgPool, mirror: &Mirror, repo: i64, refs: &Refs) -> Result<usize> {
    let missing = missing(db, repo, refs).await?;
    mirror
        .read_each(missing, read, |head| async move {
            let Some(head) = head else { return Ok(()) };
            let pair = CommitPair {
                base: Some(head.merge_base),
                head: head.head,
            };
            changes::record(db, mirror, &[pair]).await?;
            insert(db, repo, head).await
        })
        .await
}

// 1. Missing

#[derive(Debug)]
struct Missing {
    number: i32,
    head: ObjectId,
    base_tip: ObjectId,
}

async fn missing(db: &PgPool, repo: i64, refs: &Refs) -> Result<Vec<Missing>> {
    let missing: Vec<(i32, String, Vec<u8>)> = sqlx::query_as(
        "SELECT p.number, p.base_ref, p.head FROM pull p
         WHERE p.repo = $1
             AND EXISTS (SELECT FROM repo_commit r WHERE (r.repo, r.commit) = (p.repo, p.head))
             AND NOT EXISTS (
                 SELECT FROM pull_head h
                 WHERE (h.repo, h.number, h.head) = (p.repo, p.number, p.head)
             )
         LIMIT $2",
    )
    .bind(repo)
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
fn read(git: &gix::Repository, missing: Missing) -> Result<Option<Head>> {
    let budget = Budget(&git.objects, Cell::new(0));
    let mut graph = gix::revwalk::Graph::new(&budget, None);
    let merge_base = git.merge_base_with_graph(missing.head, missing.base_tip, &mut graph)?;
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

/// The mirror's objects, failing past [`limits::COMMITS`] reads.
struct Budget<'r>(&'r gix::OdbHandle, Cell<usize>);

impl gix::objs::Find for Budget<'_> {
    fn try_find<'a>(
        &self,
        id: &gix::oid,
        buffer: &'a mut Vec<u8>,
    ) -> gix::error::Result<Option<gix::objs::Data<'a>>> {
        self.1.set(self.1.get() + 1);
        let within = limits::check(self.1.get(), limits::COMMITS, "objects read");
        within.map_err(|error| gix::error::Error::from_error(io::Error::other(error)))?;
        self.0.try_find(id, buffer)
    }
}

// 3. Insert

/// One statement, so a head and its commits go in together.
async fn insert(db: &PgPool, repo: i64, head: Head) -> Result {
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
    .bind(repo)
    .bind(head.number)
    .bind(head.head.as_slice())
    .bind(head.merge_base.as_slice())
    .bind(column(&head.added, |commit| commit.as_slice()))
    .execute(db)
    .await?;
    Ok(())
}

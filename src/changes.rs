//! Records what differs between pairs of commits: one `change` row per file.
//!
//! For each of the pairs without rows ([`missing`]), [`diff`] its trees and insert a row per
//! changed file, with its [`lines`], in one transaction.
//!
//! A pair's rows depend on its content alone. No attributes or configuration apply, so they are
//! right for every repository and branch that reaches the pair. As git does without attributes,
//! a blob is binary if its first 8000 bytes hold a NUL.

use std::collections::VecDeque;

use gix::{
    ObjectId,
    bstr::{BStr, BString, ByteSlice, ByteVec},
    diff::{
        blob::{Algorithm, Diff, InternedInput},
        tree::{Error, State, Visit, visit},
    },
    object::tree::{EntryKind, EntryMode},
    objs::TreeRefIter,
};
use sqlx::PgPool;

use crate::{Result, limits, wait};

/// `base` is diffed to `head`; a root commit has no base.
#[derive(Clone, Copy)]
pub struct Pair {
    pub base: Option<ObjectId>,
    pub head: ObjectId,
}

/// The commits of `pairs` must have rows. Rows are determined by their pair, so any that another
/// sync inserts meanwhile are the same.
pub fn record(db: &PgPool, repo: &gix::Repository, pairs: &[Pair]) -> Result {
    for i in wait(missing(db, pairs))? {
        let pair = pairs[i];
        let mut transaction = wait(db.begin())?;
        for (path, old, new) in diff(repo, pair)? {
            let (added, deleted) = lines(repo, old, new)?.unzip();
            let oid = |side: &Side| side.as_ref().map(|(oid, _)| oid.as_slice().to_vec());
            let query = sqlx::query(
                "INSERT INTO change VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                 ON CONFLICT DO NOTHING",
            )
            .bind(pair.base.as_ref().map(ObjectId::as_slice))
            .bind(pair.head.as_slice())
            .bind(path.to_str()?)
            .bind(oid(&old))
            .bind(old.map(|(_, mode)| mode))
            .bind(oid(&new))
            .bind(new.map(|(_, mode)| mode))
            .bind(added)
            .bind(deleted);
            wait(query.execute(&mut *transaction))?;
        }
        wait(transaction.commit())?;
    }
    Ok(())
}

// 1. Missing

/// Indices into `pairs`. A pair whose trees are equal has no rows, so it is always missing.
async fn missing(db: &PgPool, pairs: &[Pair]) -> Result<Vec<usize>> {
    let bases: Vec<_> = pairs
        .iter()
        .map(|pair| pair.base.as_ref().map(ObjectId::as_slice))
        .collect();
    let heads: Vec<_> = pairs.iter().map(|pair| pair.head.as_slice()).collect();
    let missing: Vec<i64> = sqlx::query_scalar(
        "SELECT i - 1 FROM unnest($1::bytea[], $2::bytea[]) WITH ORDINALITY AS p (base, head, i)
         WHERE NOT EXISTS (
             SELECT FROM change c WHERE c.head = p.head AND c.base IS NOT DISTINCT FROM p.base
         )",
    )
    .bind(&bases)
    .bind(&heads)
    .fetch_all(db)
    .await?;
    Ok(missing.into_iter().map(|i| i as usize).collect())
}

// 2. Diff

/// A path's object and mode, named as in the schema, unless it is absent or a tree.
type Side = Option<(ObjectId, &'static str)>;

/// The changed files: path, old side, new side.
fn diff(repo: &gix::Repository, pair: Pair) -> Result<Vec<(BString, Side, Side)>> {
    let base = match pair.base {
        Some(base) => repo.find_commit(base)?.tree()?,
        None => repo.empty_tree(),
    };
    let head = repo.find_commit(pair.head)?.tree()?;
    fn iter<'a>(tree: &'a gix::Tree) -> TreeRefIter<'a> {
        TreeRefIter::from_bytes(&tree.data, tree.id.kind())
    }
    let mut changed = Changed::default();
    let diffed = gix::diff::tree(
        iter(&base),
        iter(&head),
        State::default(),
        &repo.objects,
        &mut changed,
    );
    if let Err(Error::Cancelled) = diffed {
        let max = limits::CHANGED_PATH_BYTES;
        return Err(format!("more than {max} bytes of changed paths between two commits").into());
    }
    diffed?;

    Ok(changed.files)
}

/// The changed files, with paths kept as gix's `Recorder` keeps them. Stops once the paths of
/// changed entries, trees included, sum past [`limits::CHANGED_PATH_BYTES`]. That bounds the
/// paths kept, the subtrees queued to compare, each visited first, and the work done.
#[derive(Default)]
struct Changed {
    path: BString,
    queued: VecDeque<BString>,
    path_bytes: usize,
    files: Vec<(BString, Side, Side)>,
}

impl Visit for Changed {
    fn pop_front_tracked_path_and_set_current(&mut self) {
        self.path = self.queued.pop_front().expect("queued with its subtree");
    }
    fn push_back_tracked_path_component(&mut self, component: &BStr) {
        self.push_path_component(component);
        self.queued.push_back(self.path.clone());
    }
    fn push_path_component(&mut self, component: &BStr) {
        if !self.path.is_empty() && !component.is_empty() {
            self.path.push(b'/');
        }
        self.path.push_str(component);
    }
    fn pop_path_component(&mut self) {
        let parent = self.path.rfind_byte(b'/').unwrap_or(0);
        self.path.truncate(parent);
    }
    fn visit(&mut self, change: visit::Change) -> visit::Action {
        self.path_bytes += self.path.len();
        if self.path_bytes > limits::CHANGED_PATH_BYTES {
            return visit::Action::Break(());
        }
        let (old, new) = match change {
            visit::Change::Addition {
                entry_mode, oid, ..
            } => (None, side(entry_mode, oid)),
            visit::Change::Deletion {
                entry_mode, oid, ..
            } => (side(entry_mode, oid), None),
            visit::Change::Modification {
                previous_entry_mode,
                previous_oid,
                entry_mode,
                oid,
            } => (
                side(previous_entry_mode, previous_oid),
                side(entry_mode, oid),
            ),
        };
        if old.is_some() || new.is_some() {
            self.files.push((self.path.clone(), old, new));
        }
        visit::Action::Continue(())
    }
}

fn side(mode: EntryMode, oid: ObjectId) -> Side {
    let mode = match mode.kind() {
        EntryKind::Tree => return None,
        EntryKind::Blob => "file",
        EntryKind::BlobExecutable => "executable",
        EntryKind::Link => "symlink",
        EntryKind::Commit => "submodule",
    };
    Some((oid, mode))
}

/// Added and deleted, as `git diff --numstat` counts them with Myers' algorithm, git's default;
/// none for binary content and submodules. A blob is at most [`limits::OBJECT_BYTES`].
fn lines(repo: &gix::Repository, old: Side, new: Side) -> Result<Option<(i32, i32)>> {
    if [old, new]
        .iter()
        .any(|side| matches!(side, Some((_, "submodule"))))
    {
        return Ok(None);
    }
    let read = |side: Side| -> Result<Vec<u8>> {
        Ok(match side {
            Some((oid, _)) => repo.find_blob(oid)?.detach().data,
            None => Vec::new(),
        })
    };
    let (old, new) = (read(old)?, read(new)?);
    let is_binary = |data: &[u8]| data[..data.len().min(8000)].contains(&0);
    if is_binary(&old) || is_binary(&new) {
        return Ok(None);
    }
    let diff = Diff::compute(Algorithm::Myers, &InternedInput::new(&old[..], &new[..]));
    let (added, deleted) = (diff.count_additions(), diff.count_removals());
    Ok(Some((added.try_into()?, deleted.try_into()?)))
}

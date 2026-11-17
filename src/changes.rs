//! Records what differs between pairs of commits: a `change` row per changed file.
//!
//! 1. [`missing`]: the pairs without rows.
//! 2. [`diff_pair`] each: the changed files, and how many lines changed in each.
//! 3. [`insert`] its rows.
//!
//! A pair's rows depend on its content alone, so they hold for every repository and branch that
//! has the pair.

use std::collections::VecDeque;

use gix::{
    ObjectId,
    bstr::{BStr, BString, ByteSlice, ByteVec},
    diff::{
        blob::{Algorithm, Diff, InternedInput},
        tree::{State, Visit, visit},
    },
    object::tree::{EntryKind, EntryMode},
    objs::TreeRefIter,
};
use sqlx::PgPool;

use crate::{Result, column, limits, mirror::Mirror};

/// `base` is diffed to `head`; a root commit has no base.
#[derive(Clone, Copy)]
pub struct CommitPair {
    pub base: Option<ObjectId>,
    pub head: ObjectId,
}

/// The commits of `pairs` must have rows.
pub async fn record(db: &PgPool, mirror: &Mirror, pairs: &[CommitPair]) -> Result {
    let missing = missing(db, pairs).await?;
    mirror
        .read_each(&missing, diff_pair, |diffed| insert(db, diffed))
        .await
}

// 1. Missing

/// A pair whose trees are equal has no rows, so it is always missing.
async fn missing(db: &PgPool, pairs: &[CommitPair]) -> Result<Vec<CommitPair>> {
    let missing: Vec<(Option<Vec<u8>>, Vec<u8>)> = sqlx::query_as(
        "SELECT p.base, p.head FROM unnest($1::bytea[], $2::bytea[]) AS p (base, head)
         WHERE NOT EXISTS (
             SELECT FROM change c WHERE c.head = p.head AND c.base IS NOT DISTINCT FROM p.base
         )",
    )
    .bind(column(pairs, |p| p.base.as_ref().map(ObjectId::as_slice)))
    .bind(column(pairs, |pair| pair.head.as_slice()))
    .fetch_all(db)
    .await?;
    let pair = |(base, head): (Option<Vec<u8>>, Vec<u8>)| CommitPair {
        base: base.as_deref().map(ObjectId::from_bytes_or_panic),
        head: ObjectId::from_bytes_or_panic(&head),
    };
    Ok(missing.into_iter().map(pair).collect())
}

// 2. Diff

struct FileChange {
    path: String,
    old: Option<Entry>,
    new: Option<Entry>,
    lines_added: Option<i32>,
    lines_deleted: Option<i32>,
}

/// A file's object and mode, named as in the schema.
#[derive(Clone, Copy)]
struct Entry {
    oid: ObjectId,
    mode: &'static str,
}

fn diff_pair(repo: &gix::Repository, pair: CommitPair) -> Result<(CommitPair, Vec<FileChange>)> {
    let base = match pair.base {
        Some(base) => repo.find_commit(base)?.tree()?,
        None => repo.empty_tree(),
    };
    let head = repo.find_commit(pair.head)?.tree()?;
    fn entries<'a>(tree: &'a gix::Tree) -> TreeRefIter<'a> {
        TreeRefIter::from_bytes(&tree.data, tree.id.kind())
    }
    let mut changed = ChangedFiles::default();
    let diffed = gix::diff::tree(
        entries(&base),
        entries(&head),
        State::default(),
        &repo.objects,
        &mut changed,
    );
    // `ChangedFiles` cancels the diff past the bound.
    limits::check(
        changed.path_bytes,
        limits::CHANGED_PATH_BYTES,
        "changed path bytes",
    )?;
    diffed?;

    let files = changed.files.into_iter().map(|(path, old, new)| {
        let (lines_added, lines_deleted) = count_lines(repo, old, new)?.unzip();
        let path = path.to_str()?.to_owned();
        Ok(FileChange {
            path,
            old,
            new,
            lines_added,
            lines_deleted,
        })
    });
    Ok((pair, files.collect::<Result<_>>()?))
}

/// The changed files, with paths kept as gix's `Recorder` keeps them. Stops once the paths of
/// changed entries, trees included, sum past [`limits::CHANGED_PATH_BYTES`], which bounds the
/// paths kept, the subtrees queued and the work done.
#[derive(Default)]
struct ChangedFiles {
    path: BString,
    queued: VecDeque<BString>,
    path_bytes: usize,
    files: Vec<(BString, Option<Entry>, Option<Entry>)>,
}

impl Visit for ChangedFiles {
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
            } => (None, entry(entry_mode, oid)),
            visit::Change::Deletion {
                entry_mode, oid, ..
            } => (entry(entry_mode, oid), None),
            visit::Change::Modification {
                previous_entry_mode,
                previous_oid,
                entry_mode,
                oid,
            } => (
                entry(previous_entry_mode, previous_oid),
                entry(entry_mode, oid),
            ),
        };
        if old.is_some() || new.is_some() {
            self.files.push((self.path.clone(), old, new));
        }
        visit::Action::Continue(())
    }
}

/// Trees are not files.
fn entry(mode: EntryMode, oid: ObjectId) -> Option<Entry> {
    let mode = match mode.kind() {
        EntryKind::Tree => return None,
        EntryKind::Blob => "file",
        EntryKind::BlobExecutable => "executable",
        EntryKind::Link => "symlink",
        EntryKind::Commit => "submodule",
    };
    Some(Entry { oid, mode })
}

/// Added and deleted, as `git diff --numstat` counts them; none for binary files and submodules.
fn count_lines(
    repo: &gix::Repository,
    old: Option<Entry>,
    new: Option<Entry>,
) -> Result<Option<(i32, i32)>> {
    if [old, new]
        .iter()
        .flatten()
        .any(|entry| entry.mode == "submodule")
    {
        return Ok(None);
    }
    let content = |entry: Option<Entry>| -> Result<Vec<u8>> {
        Ok(match entry {
            Some(entry) => repo.find_blob(entry.oid)?.detach().data,
            None => Vec::new(),
        })
    };
    let (old, new) = (content(old)?, content(new)?);
    if is_binary(&old) || is_binary(&new) {
        return Ok(None);
    }
    // Myers, git's default.
    let diff = Diff::compute(Algorithm::Myers, &InternedInput::new(&old[..], &new[..]));
    Ok(Some((
        diff.count_additions().try_into()?,
        diff.count_removals().try_into()?,
    )))
}

/// As git decides without attributes: a NUL in the first 8000 bytes.
pub fn is_binary(content: &[u8]) -> bool {
    content[..content.len().min(8000)].contains(&0)
}

// 3. Insert

async fn insert(db: &PgPool, (pair, files): (CommitPair, Vec<FileChange>)) -> Result {
    let files = &files;
    let oid = |entry: Option<Entry>| entry.map(|entry| entry.oid.as_slice().to_vec());
    let mode = |entry: Option<Entry>| entry.map(|entry| entry.mode);
    sqlx::query(
        "INSERT INTO change SELECT $1, $2, f.* FROM unnest(
             $3::text[], $4::bytea[], $5::text[], $6::bytea[], $7::text[], $8::int[], $9::int[]
         ) AS f
         ON CONFLICT DO NOTHING",
    )
    .bind(pair.base.as_ref().map(ObjectId::as_slice))
    .bind(pair.head.as_slice())
    .bind(column(files, |file| file.path.as_str()))
    .bind(column(files, |file| oid(file.old)))
    .bind(column(files, |file| mode(file.old)))
    .bind(column(files, |file| oid(file.new)))
    .bind(column(files, |file| mode(file.new)))
    .bind(column(files, |file| file.lines_added))
    .bind(column(files, |file| file.lines_deleted))
    .execute(db)
    .await?;
    Ok(())
}

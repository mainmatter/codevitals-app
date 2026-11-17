//! Records what differs between pairs of commits: a `change` row per changed file.
//!
//! 1. [`missing`]: the pairs without rows.
//! 2. [`diff`] each: the changed files, and how many lines changed in each.
//! 3. [`insert`] its rows.
//!
//! A pair's rows depend on its content alone, so they hold for every repository and branch that
//! has the pair.

use std::ops::ControlFlow;

use gix::{
    ObjectId,
    diff::blob::{Algorithm, Diff, InternedInput},
    object::tree::{EntryKind, EntryMode, diff::Change},
};
use sqlx::PgPool;

use crate::{Result, column, limits, mirror::Mirror, text};

/// `base` is diffed to `head`; a root commit has no base.
#[derive(Clone, Copy, Debug)]
pub struct CommitPair {
    pub base: Option<ObjectId>,
    pub head: ObjectId,
}

pub async fn record(db: &PgPool, mirror: &Mirror, pairs: &[CommitPair]) -> Result<usize> {
    let missing = missing(db, pairs).await?;
    mirror
        .read_each(missing, diff, |diffed| insert(db, diffed))
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

/// A file's object and mode, named as in the schema; none where the path is absent.
type Side = Option<(ObjectId, &'static str)>;

struct File {
    path: String,
    old: Side,
    new: Side,
    /// Added and deleted.
    lines: Option<(i32, i32)>,
}

/// Stops once the paths of changed entries, trees included, sum past
/// [`limits::CHANGED_PATH_BYTES`], which bounds the paths kept and the work done.
fn diff(git: &gix::Repository, pair: CommitPair) -> Result<(CommitPair, Vec<File>)> {
    let base = match pair.base {
        Some(base) => git.find_commit(base)?.tree()?,
        None => git.empty_tree(),
    };
    let head = git.find_commit(pair.head)?.tree()?;
    let (mut path_bytes, mut changed) = (0, Vec::new());
    let diffed = (base.changes()?)
        .options(|options| {
            options.track_path().track_rewrites(None);
        })
        .for_each_to_obtain_tree(&head, |change| {
            path_bytes += change.location().len();
            if path_bytes > limits::CHANGED_PATH_BYTES {
                return Ok(ControlFlow::Break(()));
            }
            let (old, new) = match change {
                Change::Addition { entry_mode, id, .. } => (None, side(entry_mode, id)),
                Change::Deletion { entry_mode, id, .. } => (side(entry_mode, id), None),
                Change::Modification {
                    previous_entry_mode: old_mode,
                    previous_id: old_id,
                    entry_mode,
                    id,
                    ..
                } => (side(old_mode, old_id), side(entry_mode, id)),
                Change::Rewrite { .. } => unreachable!("rewrites are not tracked"),
            };
            if old.is_some() || new.is_some() {
                changed.push((change.location().to_owned(), old, new));
            }
            Ok(ControlFlow::Continue(()))
        });
    limits::check(path_bytes, limits::CHANGED_PATH_BYTES, "changed path bytes")?;
    diffed?;
    let files = changed.into_iter().map(|(path, old, new)| {
        let lines = count_lines(git, old, new)?;
        Ok(File {
            path: text(&path),
            old,
            new,
            lines,
        })
    });
    Ok((pair, files.collect::<Result<_>>()?))
}

/// Trees are not files.
fn side(mode: EntryMode, id: gix::Id) -> Side {
    let mode = match mode.kind() {
        EntryKind::Tree => return None,
        EntryKind::Blob => "file",
        EntryKind::BlobExecutable => "executable",
        EntryKind::Link => "symlink",
        EntryKind::Commit => "submodule",
    };
    Some((id.detach(), mode))
}

/// As `git diff --numstat` counts them; none for binary files, submodules and a side over
/// [`limits::DIFF_BYTES`].
fn count_lines(git: &gix::Repository, old: Side, new: Side) -> Result<Option<(i32, i32)>> {
    let content = |side: Side| -> Result<Option<Vec<u8>>> {
        Ok(match side {
            Some((_, "submodule")) => None,
            Some((oid, _)) if git.find_header(oid)?.size() > limits::DIFF_BYTES => None,
            Some((oid, _)) => Some(git.find_blob(oid)?.detach().data).filter(|c| !is_binary(c)),
            None => Some(Vec::new()),
        })
    };
    let (Some(old), Some(new)) = (content(old)?, content(new)?) else {
        return Ok(None);
    };
    // Myers, git's default.
    let diff = Diff::compute(Algorithm::Myers, &InternedInput::new(&old[..], &new[..]));
    let (added, deleted) = (diff.count_additions(), diff.count_removals());
    Ok(Some((added.try_into()?, deleted.try_into()?)))
}

/// As git decides without attributes: a NUL in the first 8000 bytes.
pub fn is_binary(content: &[u8]) -> bool {
    content[..content.len().min(8000)].contains(&0)
}

// 3. Insert

async fn insert(db: &PgPool, (pair, files): (CommitPair, Vec<File>)) -> Result {
    let files = &files;
    let oid = |side: Side| side.map(|(oid, _)| oid.as_slice().to_vec());
    let mode = |side: Side| side.map(|(_, mode)| mode);
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
    .bind(column(files, |file| file.lines.map(|(added, _)| added)))
    .bind(column(files, |file| file.lines.map(|(_, deleted)| deleted)))
    .execute(db)
    .await?;
    Ok(())
}

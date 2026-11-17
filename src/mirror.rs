//! A bare mirror of each repository: the source of all code data.
//!
//! 1. [`Mirror::update`] [`fetch`]es GitHub's branches and pull request heads.
//! 2. [`prune`] deletes those GitHub no longer has, and keeps every pull request head as
//!    `refs/pull-heads/<oid>`, so a head replaced by a force-push stays readable.
//! 3. [`Mirror::run`] and [`Mirror::read_each`] read the mirror on rayon's thread pool, keeping
//!    CPU-bound work off the async runtime.
//!
//! The token reaches git only through the credentials callback, never a URL or the config.

use std::{
    collections::HashMap,
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use gix::{
    ObjectId,
    bstr::ByteSlice,
    credentials::{helper::Action, protocol::Outcome},
    progress::Discard,
    refs::{
        FullName, Target,
        transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog},
    },
    remote::Direction,
    sec::identity::Account,
};
use octocrab::{
    models::RepositoryId,
    secrecy::{ExposeSecret, SecretString},
};
use rayon::prelude::*;
use tokio::{sync::oneshot, time::timeout};
use tracing::Span;

use crate::{Result, limits};

const REFSPECS: [&str; 2] = [
    "+refs/heads/*:refs/heads/*",
    "+refs/pull/*/head:refs/pull/*/head",
];
const PULL_REQUEST_HEADS: &str = "refs/pull/";
const KEPT_HEADS: &str = "refs/pull-heads/";

/// Ref names and the commits they point to.
pub type Refs = HashMap<FullName, ObjectId>;

/// Creates `dir`, readable by this user only.
pub fn prepare(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

#[derive(Clone)]
pub struct Mirror(pub PathBuf);

impl Mirror {
    pub fn new(dir: &Path, repository: RepositoryId) -> Self {
        Self(dir.join(format!("{}.git", repository.0)))
    }

    /// Returns GitHub's refs. Past `FETCH_TIMEOUT` the fetch is asked to stop, which it notices
    /// only while receiving the pack. Network I/O, so a blocking thread rather than rayon.
    pub async fn update(&self, url: String, token: SecretString) -> Result<Refs> {
        let interrupt = Arc::new(AtomicBool::new(false));
        let mut fetching = tokio::task::spawn_blocking({
            let (mirror, interrupt) = (self.clone(), interrupt.clone());
            move || {
                let repo = mirror.open_or_init()?;
                let github = fetch(&repo, &url, token, &interrupt)?;
                prune(&repo, &github)?;
                Ok(github)
            }
        });
        if let Ok(fetched) = timeout(limits::FETCH_TIMEOUT, &mut fetching).await {
            return fetched?;
        }
        interrupt.store(true, Ordering::Relaxed);
        fetching.await?
    }

    pub async fn remove(&self) -> Result {
        let path = self.0.clone();
        Ok(tokio::task::spawn_blocking(move || remove_dir(&path)).await??)
    }

    /// On rayon's thread pool, in the caller's tracing span.
    pub async fn run<T: Send + 'static>(
        &self,
        job: impl FnOnce(&gix::Repository) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (mirror, span) = (self.clone(), Span::current());
        let (send, receive) = oneshot::channel();
        rayon::spawn(move || {
            let _ = send.send(span.in_scope(|| job(&mirror.open()?)));
        });
        receive.await?
    }

    /// The shape of every step: `read` each item, a batch at a time in parallel, and hand what it
    /// read to `insert`, in order.
    pub async fn read_each<I, T, F>(
        &self,
        items: &[I],
        read: fn(&gix::Repository, I) -> Result<T>,
        mut insert: impl FnMut(T) -> F,
    ) -> Result
    where
        I: Clone + Send + 'static,
        T: Send + 'static,
        F: Future<Output = Result>,
    {
        for batch in items.chunks(limits::BATCH) {
            let batch = batch.to_vec();
            let read = self.run(move |repo| {
                let (repo, span) = (repo.clone().into_sync(), Span::current());
                (batch.into_par_iter())
                    .map_init(
                        || repo.to_thread_local(),
                        |repo, item| span.in_scope(|| read(repo, item)),
                    )
                    .collect::<Result<Vec<_>>>()
            });
            for item in read.await? {
                insert(item).await?;
            }
        }
        Ok(())
    }

    /// Nothing from the host's git config or environment applies.
    fn open(&self) -> Result<gix::Repository> {
        let alloc_limit = format!("gitoxide.objects.allocLimit={}", limits::OBJECT_BYTES);
        let options = gix::open::Options::isolated().config_overrides([alloc_limit]);
        Ok(gix::open_opts(&self.0, options)?)
    }

    /// Initialised beside the mirror and moved into place, so it is never half a repository.
    fn open_or_init(&self) -> Result<gix::Repository> {
        if !self.0.exists() {
            let new = self.0.with_added_extension("new");
            remove_dir(&new)?;
            gix::init_bare(&new)?;
            fs::rename(&new, &self.0)?;
        }
        self.open()
    }
}

// 1. Fetch

/// Fails before receiving anything if GitHub's refs would not fit beside those already here:
/// receiving writes them, and pruning must read them all.
fn fetch(
    repo: &gix::Repository,
    url: &str,
    token: SecretString,
    interrupt: &AtomicBool,
) -> Result<Refs> {
    let credentials = move |action| {
        Ok(match action {
            Action::Get(context) => Some(Outcome {
                identity: Account {
                    username: "x-access-token".into(),
                    password: token.expose_secret().into(),
                    oauth_refresh_token: None,
                },
                next: context.into(),
            }),
            Action::Store(_) | Action::Erase(_) => None,
        })
    };
    // Never saved, so the URL stays out of the config.
    let remote = repo
        .remote_at(url)?
        .with_refspecs(REFSPECS, Direction::Fetch)?;
    let prepared = (remote.connect(Direction::Fetch)?)
        .with_credentials(credentials)
        .prepare_fetch(Discard, Default::default())?;
    let refs = prepared.ref_map().mappings.len() + read_refs(repo)?.len();
    limits::check(refs, limits::MIRROR_REFS, REFS)?;
    let fetched = prepared.receive(Discard, interrupt)?;

    let github = fetched.ref_map.mappings.into_iter().map(|mapping| {
        let name = mapping.remote.as_name().ok_or("an unnamed ref")?;
        let oid = mapping.remote.as_id().ok_or("an unborn ref")?;
        Ok((name.to_owned().try_into()?, oid.to_owned()))
    });
    github.collect()
}

// 2. Prune

/// gix fetches without pruning. Wanted are GitHub's refs, every kept head, and a kept head for each
/// pull request head; the refs that differ are written in one transaction.
fn prune(repo: &gix::Repository, github: &Refs) -> Result {
    let refs = read_refs(repo)?;
    let starts_with = |name: &FullName, prefix: &str| name.as_bstr().starts_with_str(prefix);
    let mut wanted = github.clone();
    for (name, &oid) in &refs {
        if starts_with(name, KEPT_HEADS) {
            wanted.insert(name.clone(), oid);
        }
        if starts_with(name, PULL_REQUEST_HEADS) {
            let kept = FullName::try_from(format!("{KEPT_HEADS}{oid}")).expect("a valid ref name");
            wanted.insert(kept, oid);
        }
    }
    limits::check(wanted.len(), limits::MIRROR_REFS, REFS)?;

    let deleted = (refs.keys().filter(|name| !wanted.contains_key(*name))).map(|name| RefEdit {
        change: Change::Delete {
            expected: PreviousValue::Any,
            log: RefLog::AndReference,
        },
        name: name.clone(),
        deref: false,
    });
    let created =
        (wanted.iter().filter(|(name, _)| !refs.contains_key(*name))).map(|(name, &oid)| RefEdit {
            change: Change::Update {
                log: LogChange::default(),
                expected: PreviousValue::Any,
                new: Target::Object(oid),
            },
            name: name.clone(),
            deref: false,
        });
    repo.edit_references(deleted.chain(created).collect::<Vec<_>>())?;
    Ok(())
}

/// Checked before every write too, so a mirror never holds more refs than it can read.
const REFS: &str = "refs in a mirror";

fn read_refs(repo: &gix::Repository) -> Result<Refs> {
    let platform = repo.references()?;
    let refs = platform.all()?.map(|reference| {
        let reference = reference?.detach();
        // Only direct refs are written here.
        Ok((reference.name, reference.target.into_id()))
    });
    Ok(limits::collect(refs, limits::MIRROR_REFS, REFS)?
        .into_iter()
        .collect())
}

fn remove_dir(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        removed => removed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{git, scratch};

    /// GitHub holds `main`, `gone`, and pull requests 1 and 2 with the same head. Then `main`
    /// moves, pull request 1 is force-pushed and `gone` deleted.
    #[tokio::test]
    async fn a_replaced_pull_request_head_stays_readable() {
        let root = scratch("mirror");
        let (github, work) = (root.join("github.git"), root.join("work"));
        git(&root, &["init", "--bare", "github.git"]);
        git(&root, &["init", "work"]);
        let amend = |message| {
            git(
                &work,
                &["commit", "--amend", "--allow-empty", "-m", message],
            );
            git(&work, &["rev-parse", "HEAD"])
        };
        let push = |refspec| git(&work, &["push", "--force", "../github.git", refspec]);
        git(&work, &["commit", "--allow-empty", "-m", "root"]);
        let a = amend("a");
        push("HEAD:refs/heads/main");
        push("HEAD:refs/heads/gone");
        push("HEAD:refs/pull/1/head");
        push("HEAD:refs/pull/2/head");

        let mirror = Mirror::new(&root, RepositoryId(1));
        let url = github.to_str().unwrap().to_owned();
        mirror.update(url.clone(), "".into()).await.unwrap();
        let b = amend("b");
        push("HEAD:refs/heads/main");
        push("HEAD:refs/pull/1/head");
        push(":refs/heads/gone");
        mirror.update(url.clone(), "".into()).await.unwrap();

        let refs = git(
            &mirror.0,
            &["for-each-ref", "--format=%(refname) %(objectname)"],
        );
        let mut expected = vec![
            format!("refs/heads/main {b}"),
            format!("refs/pull-heads/{a} {a}"),
            format!("refs/pull-heads/{b} {b}"),
            format!("refs/pull/1/head {b}"),
            format!("refs/pull/2/head {a}"),
        ];
        expected.sort();
        assert_eq!(Vec::from_iter(refs.lines()), expected);
        let config = fs::read_to_string(mirror.0.join("config")).unwrap();
        assert!(!config.contains(&url), "{config}");
        remove_dir(&root).unwrap();
    }
}

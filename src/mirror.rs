//! A bare mirror of each repository: the source of all code data.
//!
//! 1. [`Mirror::update`] [`fetch`]es GitHub's branches and pull request heads.
//! 2. [`prune`] deletes those GitHub no longer has. Their objects stay: nothing collects garbage,
//!    so a head replaced by a force-push stays readable.
//! 3. [`Mirror::run`] and [`Mirror::read_each`] read the mirror on blocking threads, off the async
//!    runtime.
//!
//! The token reaches git only through the credentials callback, never a URL or the config.

use std::{
    collections::HashMap,
    fmt::Debug,
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use gix::{
    ObjectId,
    credentials::{helper::Action, protocol::Outcome},
    progress::Discard,
    refs::{
        FullName,
        transaction::{Change, PreviousValue, RefEdit, RefLog},
    },
    remote::Direction,
    sec::identity::Account,
};
use octocrab::{
    models::RepositoryId,
    secrecy::{ExposeSecret, SecretString},
};
use tokio::{sync::mpsc, time::timeout};
use tracing::Span;

use crate::{Result, limits};

const REFSPECS: [&str; 2] = [
    "+refs/heads/*:refs/heads/*",
    "+refs/pull/*/head:refs/pull/*/head",
];

/// Ref names and the commits they point to.
pub type Refs = HashMap<FullName, ObjectId>;

/// The tip of branch `name`, if GitHub has it.
pub fn branch(refs: &Refs, name: &str) -> Result<Option<ObjectId>> {
    Ok(refs
        .get(&FullName::try_from(format!("refs/heads/{name}"))?)
        .copied())
}

/// Every pull request's head.
pub fn pull_heads(refs: &Refs) -> impl Iterator<Item = ObjectId> {
    let pull_heads = refs
        .iter()
        .filter(|(name, _)| name.as_bstr().starts_with(b"refs/pull/"));
    pull_heads.map(|(_, head)| *head)
}

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
    /// only while receiving the pack.
    pub async fn update(&self, url: String, token: SecretString) -> Result<Refs> {
        let interrupt = Arc::new(AtomicBool::new(false));
        let mut fetching = tokio::task::spawn_blocking({
            let (mirror, interrupt) = (self.clone(), interrupt.clone());
            move || {
                let git = mirror.open_or_init()?;
                let github = fetch(&git, &url, token, &interrupt)?;
                prune(&git, &github)?;
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

    /// On a blocking thread, in the caller's tracing span.
    pub async fn run<T: Send + 'static>(
        &self,
        job: impl FnOnce(&gix::Repository) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (mirror, span) = (self.clone(), Span::current());
        tokio::task::spawn_blocking(move || span.in_scope(|| job(&mirror.open()?))).await?
    }

    /// The shape of every step: [`limits::READERS`] threads `read` the items, and hand what they
    /// read to `insert` as it comes. Each thread has its own repository: threads sharing one may
    /// each find an object missing while another loads the index holding it. An error names its
    /// item, and is returned once every thread has stopped. Returns how many items there were.
    pub async fn read_each<I, T, F>(
        &self,
        items: Vec<I>,
        read: fn(&gix::Repository, I) -> Result<T>,
        mut insert: impl FnMut(T) -> F,
    ) -> Result<usize>
    where
        I: Debug + Send + 'static,
        T: Send + 'static,
        F: Future<Output = Result>,
    {
        let count = items.len();
        let readers = count.min(limits::READERS);
        let items = Arc::new(Mutex::new(items.into_iter()));
        let (send, mut receive) = mpsc::channel(limits::READERS);
        let span = Span::current();
        let readers = Vec::from_iter((0..readers).map(|_| {
            let (mirror, items, send, span) =
                (self.clone(), items.clone(), send.clone(), span.clone());
            tokio::task::spawn_blocking(move || {
                let _span = span.enter();
                // Stops when the items run out, at its first error, or once nothing receives.
                let read_all = || -> Result {
                    let git = mirror.open()?;
                    loop {
                        let Some(item) = items.lock().unwrap().next() else {
                            return Ok(());
                        };
                        let what = format!("{item:?}");
                        let read = read(&git, item).map_err(|error| format!("{what}: {error}"))?;
                        if send.blocking_send(Ok(read)).is_err() {
                            return Ok(());
                        }
                    }
                };
                if let Err(error) = read_all() {
                    let _ = send.blocking_send(Err(error));
                }
            })
        }));
        drop(send);

        let inserted: Result = async {
            while let Some(read) = receive.recv().await {
                insert(read?).await?;
            }
            Ok(())
        }
        .await;
        drop(receive);
        for reader in readers {
            reader.await?;
        }
        inserted.map(|()| count)
    }

    /// Nothing from the host's git config or environment applies.
    fn open(&self) -> Result<gix::Repository> {
        let alloc_limit = format!("gitoxide.objects.allocLimit={}", limits::OBJECT_BYTES);
        let options = gix::open::Options::isolated().config_overrides([alloc_limit]);
        Ok(gix::open_opts(&self.0, options)?)
    }

    fn open_or_init(&self) -> Result<gix::Repository> {
        if !self.0.exists() {
            gix::init_bare(&self.0)?;
        }
        self.open()
    }
}

// 1. Fetch

/// Fails before receiving anything if GitHub's refs would not fit beside those already here:
/// receiving writes them, and pruning must read them all.
fn fetch(
    git: &gix::Repository,
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
    let remote = git
        .remote_at(url)?
        .with_refspecs(REFSPECS, Direction::Fetch)?;
    let prepared = (remote.connect(Direction::Fetch)?)
        .with_credentials(credentials)
        .prepare_fetch(Discard, Default::default())?;
    let refs = prepared.ref_map().mappings.len() + read_refs(git)?.len();
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

/// gix fetches without pruning.
fn prune(git: &gix::Repository, github: &Refs) -> Result {
    let gone = read_refs(git)?
        .into_keys()
        .filter(|name| !github.contains_key(name));
    let deleted = gone.map(|name| RefEdit {
        change: Change::Delete {
            expected: PreviousValue::Any,
            log: RefLog::AndReference,
        },
        name,
        deref: false,
    });
    git.edit_references(deleted.collect::<Vec<_>>())?;
    Ok(())
}

/// Checked before every write too, so a mirror never holds more refs than it can read.
const REFS: &str = "refs in a mirror";

fn read_refs(git: &gix::Repository) -> Result<Refs> {
    let platform = git.references()?;
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

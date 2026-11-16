//! A bare checkout of each repository, the source of all code data.
//!
//! [`Checkout::update`] fetches GitHub's branches and pull request heads, then makes the checkout's refs:
//!
//! - `refs/heads/*` and `refs/pull/*/head`: exactly GitHub's.
//! - `refs/pull-heads/<oid>`: every pull request head the checkout has held, never deleted, so a
//!   head replaced by a force-push stays readable.
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
use tokio::time::timeout;

use crate::{Result, limits};

/// Into a namespace nothing reads, so that [`set_refs`] alone writes the checkout's refs. It
/// deletes these, as every ref not wanted.
const REFSPECS: [&str; 2] = [
    "+refs/heads/*:refs/fetch/heads/*",
    "+refs/pull/*/head:refs/fetch/pull/*/head",
];
const PULLS: &str = "refs/pull/";
const PULL_HEADS: &str = "refs/pull-heads/";

/// Ref names and their commits.
pub type Refs = HashMap<FullName, ObjectId>;

/// Creates `dir`, readable by this user only.
pub fn prepare(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

/// A repository's bare checkout, at this path.
#[derive(Clone)]
pub struct Checkout(pub PathBuf);

impl Checkout {
    pub fn new(dir: &Path, repository: RepositoryId) -> Self {
        Self(dir.join(format!("{}.git", repository.0)))
    }

    /// Returns GitHub's refs, once the work has stopped, so the caller keeps the repository until
    /// then. Past `FETCH_TIMEOUT`, asks the fetch to stop; it checks only while receiving the pack.
    pub async fn update(&self, url: String, token: SecretString) -> Result<Refs> {
        let interrupt = Arc::new(AtomicBool::new(false));
        let mut worker = tokio::task::spawn_blocking({
            let (checkout, interrupt) = (self.clone(), interrupt.clone());
            move || {
                let repo = checkout.open_or_init()?;
                let github = fetch(&repo, &url, token, &interrupt)?;
                set_refs(&repo, github.clone())?;
                Ok(github)
            }
        });
        if let Ok(done) = timeout(limits::FETCH_TIMEOUT, &mut worker).await {
            return done?;
        }
        interrupt.store(true, Ordering::Relaxed);
        worker.await?
    }

    pub async fn remove(&self) -> Result {
        let path = self.0.clone();
        Ok(tokio::task::spawn_blocking(move || remove_dir(&path)).await??)
    }

    /// Runs `f` on the repository, off the async runtime. `f` reaches the database with
    /// [`crate::wait`].
    pub async fn read<T: Send + 'static>(
        &self,
        f: impl FnOnce(&gix::Repository) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let checkout = self.clone();
        tokio::task::spawn_blocking(move || f(&checkout.open()?)).await?
    }

    /// Nothing from the host's git config or environment applies.
    fn open(&self) -> Result<gix::Repository> {
        let alloc_limit = format!("gitoxide.objects.allocLimit={}", limits::OBJECT_BYTES);
        let options = gix::open::Options::isolated().config_overrides([alloc_limit]);
        Ok(gix::open_opts(&self.0, options)?)
    }

    /// Initialised beside the checkout and moved into place, so that it is never half a
    /// repository.
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

/// Writes GitHub's branches and pull request heads into the checkout, and returns them. Fails
/// before receiving anything if they would not fit beside the refs already there.
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
    fits(prepared.ref_map().mappings.len() + refs(repo)?.len())?;
    let fetched = prepared.receive(Discard, interrupt)?;

    let refs = fetched.ref_map.mappings.into_iter().map(|mapping| {
        let name = mapping.remote.as_name().ok_or("an unnamed ref")?;
        let oid = mapping.remote.as_id().ok_or("an unborn ref")?;
        Ok((name.to_owned().try_into()?, oid.to_owned()))
    });
    refs.collect()
}

// 2. Set refs

/// Makes the refs `wanted`, plus a kept head for each pull request head in it or the checkout, in
/// one transaction.
fn set_refs(repo: &gix::Repository, mut wanted: Refs) -> Result {
    let current = refs(repo)?;
    let is_pull_head = |name: &FullName| {
        [PULLS, PULL_HEADS]
            .iter()
            .any(|prefix| name.as_bstr().starts_with_str(prefix))
    };
    let kept: Refs = (current.iter().chain(&wanted))
        .filter(|(name, _)| is_pull_head(name))
        .map(|(_, &oid)| {
            let name = format!("{PULL_HEADS}{oid}").try_into();
            (name.expect("a valid ref name"), oid)
        })
        .collect();
    wanted.extend(kept);
    fits(wanted.len())?;

    let names = (current.keys())
        .filter(|name| !wanted.contains_key(*name))
        .chain(wanted.keys());
    let edits = names
        .filter(|name| current.get(*name) != wanted.get(*name))
        .map(|name| RefEdit {
            change: match wanted.get(name) {
                Some(&oid) => Change::Update {
                    log: LogChange::default(),
                    expected: PreviousValue::Any,
                    new: Target::Object(oid),
                },
                None => Change::Delete {
                    expected: PreviousValue::Any,
                    log: RefLog::AndReference,
                },
            },
            name: name.clone(),
            deref: false,
        });
    repo.edit_references(edits)?;
    Ok(())
}

// Plumbing

fn refs(repo: &gix::Repository) -> Result<Refs> {
    let references = repo.references()?;
    let mut refs = Refs::new();
    for reference in references.all()?.take(limits::CHECKOUT_REFS + 1) {
        let reference = reference?.detach();
        // The checkout holds only the direct refs written here.
        refs.insert(reference.name, reference.target.into_id());
    }
    fits(refs.len())?;
    Ok(refs)
}

/// Every write is checked first, so a checkout never holds more refs than it can read.
fn fits(refs: usize) -> Result {
    if refs > limits::CHECKOUT_REFS {
        return Err(format!("more than {} refs in the checkout", limits::CHECKOUT_REFS).into());
    }
    Ok(())
}

fn remove_dir(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        removed => removed,
    }
}

#[cfg(test)]
pub mod tests {
    use std::process::Command;

    use super::*;

    /// GitHub holds `main`, `gone` and pull request 1. Then pull request 1 is force-pushed and
    /// `gone` deleted.
    #[tokio::test]
    async fn a_replaced_pull_request_head_stays_readable() {
        let root = scratch("checkout");
        let (github, work) = (root.join("github.git"), root.join("work"));
        git(&root, &["init", "--bare", "github.git"]);
        git(&root, &["init", "work"]);
        let commit = |message| {
            git(
                &work,
                &["commit", "--amend", "--allow-empty", "-m", message],
            );
            git(&work, &["rev-parse", "HEAD"])
        };
        let push = |refspec| git(&work, &["push", "--force", "../github.git", refspec]);
        git(&work, &["commit", "--allow-empty", "-m", "root"]);
        let a = commit("a");
        push("HEAD:refs/heads/main");
        push("HEAD:refs/heads/gone");
        push("HEAD:refs/pull/1/head");

        let checkout = Checkout::new(&root, RepositoryId(1));
        let url = github.to_str().unwrap().to_owned();
        checkout.update(url.clone(), "".into()).await.unwrap();
        let b = commit("b");
        push("HEAD:refs/pull/1/head");
        push(":refs/heads/gone");
        checkout.update(url.clone(), "".into()).await.unwrap();

        let refs = git(
            &checkout.0,
            &["for-each-ref", "--format=%(refname) %(objectname)"],
        );
        let mut expected = vec![
            format!("refs/heads/main {a}"),
            format!("refs/pull-heads/{a} {a}"),
            format!("refs/pull-heads/{b} {b}"),
            format!("refs/pull/1/head {b}"),
        ];
        expected.sort();
        assert_eq!(Vec::from_iter(refs.lines()), expected);
        let config = fs::read_to_string(checkout.0.join("config")).unwrap();
        assert!(!config.contains(&url), "{config}");
        remove_dir(&root).unwrap();
    }

    /// A new, empty directory for test `name`.
    pub fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cv-{name}-{}", std::process::id()));
        remove_dir(&dir).unwrap();
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_DATE", "2020-01-01T12:00:00+05:30")
            .env("GIT_COMMITTER_DATE", "2020-01-01T12:00:00+05:30")
            .args(["-c", "user.name=A", "-c", "user.email=a@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
}

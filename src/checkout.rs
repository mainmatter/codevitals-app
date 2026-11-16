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

const REFSPECS: [&str; 2] = [
    "+refs/heads/*:refs/heads/*",
    "+refs/pull/*/head:refs/pull/*/head",
];
const PULLS: &str = "refs/pull/";
const PULL_HEADS: &str = "refs/pull-heads/";

/// Ref names and their commits.
type Refs = HashMap<FullName, ObjectId>;

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

    /// Returns only once the work has stopped, so the caller keeps the repository until then.
    /// Past `FETCH_TIMEOUT`, asks the fetch to stop; it checks only while receiving the pack.
    pub async fn update(&self, url: String, token: SecretString) -> Result {
        let interrupt = Arc::new(AtomicBool::new(false));
        let mut worker = tokio::task::spawn_blocking({
            let (checkout, interrupt) = (self.clone(), interrupt.clone());
            move || {
                let repo = checkout.open_or_init()?;
                // Keeps the pull request heads the fetch is about to overwrite.
                set_refs(&repo, refs(&repo)?)?;
                let github = fetch(&repo, &url, token, &interrupt)?;
                set_refs(&repo, github)
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

    /// Nothing from the host's git config or environment applies.
    pub fn open(&self) -> Result<gix::Repository> {
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
    use std::{
        collections::BTreeMap,
        future::IntoFuture,
        io::Write,
        process::{Command, Stdio},
    };

    use axum::{
        Router,
        body::Bytes,
        extract::State,
        http::{HeaderMap, StatusCode, Uri, header},
        response::{IntoResponse, Response},
    };
    use base64::{Engine, prelude::BASE64_STANDARD};
    use tokio::net::TcpListener;

    use super::*;

    const TOKEN: &str = "secret-token";
    const REPOSITORY: RepositoryId = RepositoryId(1);

    /// GitHub holds `main`, `gone` and pull request 1. Then pull request 1 is force-pushed and
    /// `gone` deleted.
    #[tokio::test]
    async fn a_replaced_pull_request_head_stays_readable() {
        let root = scratch("checkout");
        let (github, work) = (root.join("o/r.git"), root.join("work"));
        fs::create_dir_all(&github).unwrap();
        fs::create_dir(&work).unwrap();
        git(&github, &["init", "--bare"]);
        git(&work, &["init"]);
        git(&work, &["commit", "--allow-empty", "-m", "a"]);
        let a = git(&work, &["rev-parse", "HEAD"]);
        let push = |refspec| git(&work, &["push", "--force", "../o/r.git", refspec]);
        push("HEAD:refs/heads/main");
        push("HEAD:refs/heads/gone");
        push("HEAD:refs/pull/1/head");

        let url = format!("{}/o/r.git", serve(root.clone()).await);
        let checkout = Checkout::new(&root, REPOSITORY);
        checkout.update(url.clone(), TOKEN.into()).await.unwrap();

        git(&work, &["commit", "--amend", "--allow-empty", "-m", "b"]);
        let b = git(&work, &["rev-parse", "HEAD"]);
        push("HEAD:refs/pull/1/head");
        push(":refs/heads/gone");
        checkout.update(url.clone(), TOKEN.into()).await.unwrap();

        let refs: BTreeMap<_, _> = git(
            &checkout.0,
            &["for-each-ref", "--format=%(refname) %(objectname)"],
        )
        .lines()
        .map(|line| line.split_once(' ').unwrap())
        .map(|(name, oid)| (name.to_owned(), oid.to_owned()))
        .collect();
        let expected = BTreeMap::from([
            ("refs/heads/main".to_owned(), a.clone()),
            ("refs/pull/1/head".to_owned(), b.clone()),
            (format!("refs/pull-heads/{a}"), a.clone()),
            (format!("refs/pull-heads/{b}"), b.clone()),
        ]);
        assert_eq!(refs, expected);
        assert_eq!(git(&checkout.0, &["cat-file", "-t", &a]), "commit");

        let config = fs::read_to_string(checkout.0.join("config")).unwrap();
        assert!(
            !config.contains(TOKEN) && !config.contains(&url),
            "{config}"
        );
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

    /// Git's smart HTTP protocol, version 2, over the repositories in `root`, for [`TOKEN`] only.
    async fn serve(root: PathBuf) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new().fallback(upload_pack).with_state(root);
        tokio::spawn(axum::serve(listener, router).into_future());
        format!("http://{address}")
    }

    async fn upload_pack(
        State(root): State<PathBuf>,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        let basic = format!(
            "Basic {}",
            BASE64_STANDARD.encode(format!("x-access-token:{TOKEN}"))
        );
        if headers
            .get(header::AUTHORIZATION)
            .is_none_or(|value| value != basic.as_str())
        {
            return (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Basic")],
            )
                .into_response();
        }
        let path = uri.path();
        let (repository, advertise) = match path.strip_suffix("/info/refs") {
            Some(repository) => (repository, true),
            None => (path.strip_suffix("/git-upload-pack").unwrap(), false),
        };
        let mut git = Command::new("git");
        git.args(["upload-pack", "--stateless-rpc"]);
        if advertise {
            git.arg("--http-backend-info-refs");
        }
        let protocol = headers.get("git-protocol").unwrap().to_str().unwrap();
        let mut child = (git.arg(root.join(&repository[1..])))
            .env("GIT_PROTOCOL", protocol)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&body).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let kind = if advertise { "advertisement" } else { "result" };
        let content_type = format!("application/x-git-upload-pack-{kind}");
        ([(header::CONTENT_TYPE, content_type)], output.stdout).into_response()
    }
}

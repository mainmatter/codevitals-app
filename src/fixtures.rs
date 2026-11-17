//! What tests share: scratch repositories made with git, a fake GitHub, and rows as text.

use std::{
    collections::BTreeSet,
    convert::Infallible,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
};

use axum::http::{Request, Response, StatusCode, Uri};
use octocrab::{
    AuthState, OctoBody, Octocrab, OctocrabBuilder, models::RepositoryId,
    service::middleware::base_uri::BaseUriLayer,
};
use serde_json::Value;
use sqlx::PgPool;
use tower::service_fn;

use crate::{
    App, app,
    mirror::{self, Mirror, Refs},
};

/// A new, empty directory for test `name`.
pub fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cv-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs git in `dir`, as `A <a@example.com>` a second after the last run, and returns its output.
pub fn git(dir: &Path, args: &[&str]) -> String {
    static SECONDS: AtomicI64 = AtomicI64::new(1_577_860_200);
    let date = format!("{} +0530", SECONDS.fetch_add(1, Ordering::Relaxed));
    let output = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .args(["-c", "user.name=A", "-c", "user.email=a@example.com"])
        .args(args)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

pub fn write(work: &Path, path: &str, content: &str) {
    let path = work.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Stages a submodule at `sub`.
pub fn submodule(work: &Path, commit: &str) {
    let info = format!("160000,{commit},sub");
    git(work, &["update-index", "--add", "--cacheinfo", &info]);
}

/// Stages every file, but not the deletion of `sub`, which has no directory.
pub fn commit(work: &Path, message: &str) {
    git(work, &["add", "--all", "--", ".", ":!sub"]);
    git(work, &["commit", "-m", message]);
}

/// Repository 1, `o/r`.
pub async fn repo(db: &PgPool) {
    let insert = sqlx::query("INSERT INTO repo VALUES (1, 'o/r')").execute(db);
    insert.await.unwrap();
}

/// A query's one text column, sorted, duplicates kept.
pub async fn rows(db: &PgPool, sql: &str) -> Vec<String> {
    let rows = sqlx::query_scalar(sqlx::AssertSqlSafe(sql)).fetch_all(db);
    let mut rows: Vec<String> = rows.await.unwrap();
    rows.sort();
    rows
}

/// [`app::record`] for repository 1 from the work repository `mirror`, tracking `branches`. Twice:
/// the second run must change nothing.
pub async fn record(db: &PgPool, mirror: &Mirror, branches: &[&str]) {
    let refs = git(
        &mirror.0,
        &["for-each-ref", "--format=%(refname) %(objectname)"],
    );
    let refs = Refs::from_iter(refs.lines().map(|line| {
        let (name, oid) = line.split_once(' ').unwrap();
        (name.try_into().unwrap(), oid.parse().unwrap())
    }));
    let tip = |name: &&str| {
        (
            name.to_string(),
            mirror::branch(&refs, name).unwrap().unwrap(),
        )
    };
    let branches = Vec::from_iter(branches.iter().map(tip));
    let every_row = [
        "commit",
        "commit_person",
        "change",
        "mainline",
        "pull_head",
        "pull_commit",
        "blob_fact",
        "fn_fact",
    ]
    .map(|table| format!("SELECT '{table} ' || t::text FROM {table} t"));
    let every_row = every_row.join(" UNION ALL ");
    let mut first_run = None;
    for _ in 0..2 {
        app::record(db, mirror, RepositoryId(1), &refs, &branches)
            .await
            .unwrap();
        let run = rows(db, &every_row).await;
        assert_eq!(*first_run.get_or_insert(run.clone()), run);
    }
}

/// git's empty tree, the base of a root commit.
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Each pair's `change` rows are what `git diff --numstat` lists, submodules aside: git counts
/// their `Subproject commit` lines.
pub async fn assert_changes_match_git(db: &PgPool, work: &Path, pairs: &[(String, String)]) {
    let mut expected = BTreeSet::new();
    for (base, head) in pairs {
        let numstat = git(work, &["diff", "--numstat", base, head]);
        let files = numstat.lines().filter(|line| !line.ends_with("\tsub"));
        expected.extend(files.map(|line| format!("{base} {head}\t{line}")));
    }
    let pairs: Vec<String> = pairs
        .iter()
        .map(|(b, h)| format!("('{b}', '{h}')"))
        .collect();
    let base = format!("coalesce(encode(base, 'hex'), '{EMPTY_TREE}')");
    let changes = format!(
        "SELECT DISTINCT concat_ws(E'\\t', {base} || ' ' || encode(head, 'hex'),
            coalesce(lines_added::text, '-'), coalesce(lines_deleted::text, '-'), path)
        FROM change
        WHERE ({base}, encode(head, 'hex')) IN ({})
        AND 'submodule' NOT IN (coalesce(old_mode, ''), coalesce(new_mode, ''))",
        pairs.join(", ")
    );
    assert_eq!(rows(db, &changes).await, Vec::from_iter(expected));
}

/// Every side of every change, submodules aside, has facts.
pub async fn assert_every_blob_has_facts(db: &PgPool) {
    let without_facts = "SELECT format('%s %s', ch.path, encode(s.oid, 'hex')) FROM change ch
        CROSS JOIN LATERAL (VALUES (ch.old_oid, ch.old_mode), (ch.new_oid, ch.new_mode)) s (oid, mode)
        WHERE s.mode <> 'submodule' AND NOT EXISTS (SELECT FROM blob_fact f WHERE f.blob = s.oid
            AND f.language IS NOT DISTINCT FROM language(ch.path, s.mode))";
    assert_eq!(rows(db, without_facts).await, Vec::<String>::new());
}

/// GitHub at `http://github.test`, answering every request with `answer`.
pub fn github<A>(
    answer: impl Fn(Request<OctoBody>) -> A + Clone + Send + Sync + 'static,
) -> Octocrab
where
    A: Future<Output = (StatusCode, Value)> + Send + 'static,
{
    let service = service_fn(move |request| {
        let answered = answer(request);
        async move {
            let (status, body) = answered.await;
            let response = Response::builder().status(status);
            Ok::<_, Infallible>(response.body(body.to_string()).unwrap())
        }
    });
    OctocrabBuilder::new_empty()
        .with_service(service)
        .with_layer(&BaseUriLayer::new(Uri::from_static("http://github.test")))
        .with_auth(AuthState::None)
        .build()
        .unwrap()
}

/// Acting as `github`, with a database it never reaches.
pub fn app(github: Octocrab) -> Arc<App> {
    let config = crate::Config {
        listen: ([127, 0, 0, 1], 0).into(),
        database: "postgres://unused".parse().unwrap(),
        mirrors: "/unused".into(),
        tracked_branches: Vec::new(),
        app_id: 1,
        client_id: "unused".into(),
        private_key: "unused".into(),
        client_secret: "unused".into(),
        webhook_secret: "secret".into(),
    };
    App::new(config, github.clone(), github)
}

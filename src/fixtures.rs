//! What tests share: scratch repositories made with git, a fake GitHub, and rows as text.

use std::{
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
    AuthState, OctoBody, Octocrab, OctocrabBuilder, service::middleware::base_uri::BaseUriLayer,
};
use serde_json::Value;
use sqlx::PgPool;
use tower::service_fn;

use crate::App;

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
    let db = PgPool::connect_lazy_with(config().database);
    App::new(config(), github.clone(), github, db)
}

/// Nothing it names exists.
pub fn config() -> crate::Config {
    crate::Config {
        listen: ([127, 0, 0, 1], 0).into(),
        database: "postgres://unused".parse().unwrap(),
        mirrors: "/unused".into(),
        tracked_branches: Vec::new(),
        app_id: 1,
        client_id: "unused".into(),
        private_key: "unused".into(),
        client_secret: "unused".into(),
        webhook_secret: "secret".into(),
    }
}

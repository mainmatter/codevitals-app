// Register the app on GitHub with:
// - Callback URL `https://<host>/callback`, listed first.
// - Setup URL `https://<host>/setup`, with "Redirect on update" checked.
// - "Request user authorization (OAuth) during installation" unchecked.
// - Webhook URL `https://<host>/webhook`, with a random secret.
// - Read access to contents, metadata and pull requests.
// - Subscribed to "Push", "Pull request" and "Repository": these decide what triggers a sync.

#![forbid(unsafe_code)]

mod accounts;
mod app;
mod changes;
mod commits;
mod facts;
#[cfg(test)]
mod fixtures;
mod limits;
mod mainline;
mod mirror;
mod pull_heads;
mod pulls;
mod sign_in;
mod sync;

use std::{
    collections::HashMap,
    error::Error,
    future::IntoFuture,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::{Request, StatusCode, header::ACCEPT},
    routing::{get, post},
};
use clap::Parser;
use octocrab::{
    Octocrab, Page,
    models::RepositoryId,
    secrecy::{ExposeSecret, SecretString},
    service::middleware::retry::RetryConfig,
};
use serde::de::DeserializeOwned;
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use tokio::{
    net::TcpListener,
    signal::unix::{SignalKind, signal},
    sync::Semaphore,
};
use tower::{ServiceBuilder, limit::ConcurrencyLimitLayer};
use tower_http::{
    timeout::TimeoutLayer,
    trace::{DefaultOnResponse, TraceLayer},
};
use tracing::{Level, info, info_span};
use tracing_subscriber::{EnvFilter, fmt::format::FmtSpan};

pub type Result<T = ()> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Parser)]
#[command(version)]
struct Config {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:3000")]
    listen: SocketAddr,
    /// Postgres, e.g. `postgres:///cv?host=/run/postgresql`.
    #[arg(long = "database-url", value_name = "URL")]
    database: PgConnectOptions,
    /// Directory for a bare git mirror of each repository. Made readable by this user only.
    #[arg(long = "mirror-dir", value_name = "DIR")]
    mirrors: PathBuf,
    /// A branch to track besides the repository's default branch. Repeatable.
    #[arg(long = "tracked-branch", value_name = "REPOSITORY_ID:BRANCH", value_parser = tracked_branch)]
    tracked_branches: Vec<(RepositoryId, String)>,

    /// The app's ID.
    #[arg(long)]
    app_id: u64,
    /// The app's client ID.
    #[arg(long)]
    client_id: SecretString,
    /// File holding the app's private key, PEM-encoded.
    #[arg(long = "private-key-file", value_name = "FILE", value_parser = read_secret)]
    private_key: SecretString,
    /// File holding the app's client secret.
    #[arg(long = "client-secret-file", value_name = "FILE", value_parser = read_secret)]
    client_secret: SecretString,
    /// File holding the app's webhook secret.
    #[arg(long = "webhook-secret-file", value_name = "FILE", value_parser = read_secret)]
    webhook_secret: SecretString,
}

struct App {
    config: Config,
    github: Octocrab,
    /// Signs people in, on github.com rather than api.github.com.
    oauth: Octocrab,
    db: PgPool,
    /// Repositories with a sync waiting or running; `true` asks for one more.
    pending: Mutex<HashMap<RepositoryId, bool>>,
    permits: Semaphore,
}

impl App {
    fn new(config: Config, github: Octocrab, oauth: Octocrab, db: PgPool) -> Arc<App> {
        Arc::new(App {
            config,
            github,
            oauth,
            db,
            pending: Mutex::default(),
            permits: Semaphore::new(limits::CONCURRENT_SYNCS),
        })
    }
}

#[tokio::main]
async fn main() -> Result {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        // A span's duration, when it closes: requests, syncs, and at `debug` each blob's facts.
        .with_span_events(FmtSpan::CLOSE)
        .init();
    let config = Config::parse();
    mirror::prepare(&config.mirrors)?;

    // octocrab waits forever by default, and one stalled request would stall all syncing. Server
    // and transport errors are retried at once; a rate limit fails the sync rather than holding its
    // turn while it waits, and the next trigger tries again.
    let client = || {
        let timeout = Some(limits::GITHUB_REQUEST);
        Octocrab::builder()
            .set_connect_timeout(timeout)
            .set_read_timeout(timeout)
            .add_retry_config(RetryConfig::Simple(limits::GITHUB_RETRIES))
    };
    let key = octocrab::EncodingKey::from_rsa_pem(config.private_key.expose_secret().as_bytes())?;
    let github = client().app(config.app_id.into(), key).build()?;
    let oauth = client()
        .base_uri("https://github.com")?
        .add_header(ACCEPT, "application/json".into())
        .build()?;

    // A sync holds at most one connection at a time.
    let db = PgPoolOptions::new()
        .max_connections(limits::CONCURRENT_SYNCS as u32)
        .connect_lazy_with(config.database.clone());
    let app = App::new(config, github, oauth, db);
    sqlx::migrate!().run(&app.db).await?;

    let listener = TcpListener::bind(app.config.listen).await?;
    info!(address = %app.config.listen, "listening");
    tokio::spawn(sync::resync(app.clone()));
    // Running syncs are dropped on shutdown; the next run completes their work.
    let serve = axum::serve(listener, router(app)).with_graceful_shutdown(shutdown());
    Ok(serve.into_future().await?)
}

fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/setup", get(sign_in::setup))
        .route("/callback", get(sign_in::callback))
        // Anyone can reach it, and a body is read whole before its signature is checked. axum
        // accepts 2 MB by default. The timeout covers waiting for a turn.
        .route(
            "/webhook",
            post(sync::webhook).layer(
                ServiceBuilder::new()
                    .layer(TimeoutLayer::with_status_code(
                        StatusCode::REQUEST_TIMEOUT,
                        limits::WEBHOOK_TIME,
                    ))
                    .layer(ConcurrencyLimitLayer::new(limits::WEBHOOKS_AT_ONCE))
                    .layer(DefaultBodyLimit::max(limits::WEBHOOK_BODY)),
            ),
        )
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|request: &Request<_>| {
                    let header = |name| request.headers().get(name)?.to_str().ok();
                    // The path, not the query: on `/callback` that holds the sign-in code.
                    info_span!(
                        "request",
                        method = %request.method(),
                        path = request.uri().path(),
                        delivery = header("x-github-delivery"),
                        event = header("x-github-event"),
                    )
                })
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .with_state(app)
}

/// Waits for Ctrl-C or SIGTERM.
async fn shutdown() {
    let mut terminate = signal(SignalKind::terminate()).expect("a SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
    info!("shutting down");
}

// Plumbing

/// Reads a secret from a file, without the trailing newline most editors and `echo` leave.
fn read_secret(path: &str) -> std::io::Result<SecretString> {
    Ok(std::fs::read_to_string(path)?.trim_end().into())
}

fn tracked_branch(value: &str) -> Result<(RepositoryId, String)> {
    let (repository, branch) = value.split_once(':').ok_or("not REPOSITORY_ID:BRANCH")?;
    Ok((RepositoryId(repository.parse()?), branch.to_owned()))
}

/// Bytes git wrote, as Postgres `text`, losslessly: each byte of invalid UTF-8, and NUL, which
/// Postgres refuses, is the code point U+10FF00 plus the byte, in a private-use plane git-written
/// text does not use.
fn text(bytes: &[u8]) -> String {
    let byte = |byte: u8| char::from_u32(0x10_FF00 + u32::from(byte)).expect("a code point");
    let mut text = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        text.extend(
            chunk
                .valid()
                .chars()
                .map(|c| if c == '\0' { byte(0) } else { c }),
        );
        text.extend(chunk.invalid().iter().map(|&invalid| byte(invalid)));
    }
    text
}

/// One field of each row, to bind as an array.
fn column<'r, R, T>(rows: &'r [R], field: impl Fn(&'r R) -> T) -> Vec<T> {
    rows.iter().map(field).collect()
}

/// Every item of GitHub's list at `path`. Fails past `limit` items.
async fn list<T: DeserializeOwned>(github: &Octocrab, path: &str, limit: usize) -> Result<Vec<T>> {
    let mut items = Vec::new();
    for number in 1.. {
        let mut page = page(github, path, number).await?;
        let last = page.len() < limits::PER_PAGE;
        items.append(&mut page);
        limits::check(items.len(), limit, path)?;
        if last {
            break;
        }
    }
    Ok(items)
}

/// Page `number` of GitHub's list at `path`, from 1. Only the last page is short.
async fn page<T: DeserializeOwned>(github: &Octocrab, path: &str, number: usize) -> Result<Vec<T>> {
    let query = [("per_page", limits::PER_PAGE), ("page", number)];
    let page: Page<T> = github.get(path, Some(&query)).await?;
    Ok(page.items)
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use clap::CommandFactory;
    use tower::ServiceExt;

    use super::*;

    /// clap checks its definitions in debug builds only.
    #[test]
    fn config_is_valid() {
        Config::command().debug_assert();
    }

    /// Signed deliveries are read for the repositories they name, whatever else they hold; one
    /// that cannot be read is refused rather than ending the process.
    #[tokio::test]
    async fn webhook_accepts_only_signed_readable_deliveries() {
        // These name no repository, so GitHub is never asked.
        let github = fixtures::github(|_| async { (StatusCode::NOT_FOUND, Default::default()) });
        let router = router(fixtures::app(github));
        let ping = r#"{"zen": "Design for failure.", "repositories": null}"#;
        for (signature, body, status) in [
            (sign(ping), ping, StatusCode::ACCEPTED),
            (String::new(), ping, StatusCode::UNAUTHORIZED),
            (sign("{}"), ping, StatusCode::UNAUTHORIZED),
            (
                sign(r#"{"repository": 1}"#),
                r#"{"repository": 1}"#,
                StatusCode::BAD_REQUEST,
            ),
        ] {
            let request = Request::post("/webhook")
                .header("x-hub-signature-256", signature)
                .body(Body::from(body))
                .unwrap();
            let response = router.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), status, "{body}");
        }
    }

    /// As GitHub signs with the secret `secret`: HMAC-SHA256.
    fn sign(body: &str) -> String {
        use sha2::{Digest, Sha256};
        let key = [&b"secret"[..], &[0; 58]].concat();
        let pad = |byte: u8| Vec::from_iter(key.iter().map(|k| k ^ byte));
        let inner = Sha256::new()
            .chain_update(pad(0x36))
            .chain_update(body)
            .finalize();
        let outer = Sha256::new()
            .chain_update(pad(0x5c))
            .chain_update(inner)
            .finalize();
        let hex: String = outer.iter().map(|byte| format!("{byte:02x}")).collect();
        format!("sha256={hex}")
    }
}

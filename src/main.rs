// Register the app on GitHub with:
// - Callback URL `https://<host>/callback`, listed first.
// - Setup URL `https://<host>/setup`, with "Redirect on update" checked.
// - "Request user authorization (OAuth) during installation" unchecked.
// - Webhook URL `https://<host>/webhook`, with a random secret.
// - Read access to contents, metadata and pull requests.
// - Subscribed to "Push", "Pull request" and "Repository": these decide what triggers a sync.

#![forbid(unsafe_code)]

mod app;
mod checkout;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "a later step of `sync` calls it")
)]
mod commits;
mod limits;
#[cfg(test)]
mod schema;
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
    http::{Request, header::ACCEPT},
    routing::{get, post},
};
use clap::Parser;
use octocrab::{
    Octocrab, Page,
    models::RepositoryId,
    secrecy::{ExposeSecret, SecretString},
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
use tower_http::trace::{DefaultOnResponse, TraceLayer};
use tracing::{Level, info, info_span};
use tracing_subscriber::EnvFilter;

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
    /// Directory for a bare git checkout of each repository. Made readable by this user only.
    #[arg(long = "checkout-dir", value_name = "DIR")]
    checkouts: PathBuf,

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
    fn new(config: Config, github: Octocrab, oauth: Octocrab) -> Arc<App> {
        // A sync holds at most one connection at a time.
        let db = PgPoolOptions::new()
            .max_connections(limits::CONCURRENT_SYNCS as u32)
            .connect_lazy_with(config.database.clone());
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
        .init();
    let config = Config::parse();
    checkout::prepare(&config.checkouts)?;

    // octocrab waits forever by default, and one stalled request would stall all syncing.
    let client = || {
        let timeout = Some(limits::GITHUB_REQUEST);
        Octocrab::builder()
            .set_connect_timeout(timeout)
            .set_read_timeout(timeout)
    };
    let key = octocrab::EncodingKey::from_rsa_pem(config.private_key.expose_secret().as_bytes())?;
    let github = client().app(config.app_id.into(), key).build()?;
    let oauth = client()
        .base_uri("https://github.com")?
        .add_header(ACCEPT, "application/json".into())
        .build()?;

    let app = App::new(config, github, oauth);
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
        // axum accepts 2 MB by default.
        .route(
            "/webhook",
            post(sync::webhook).layer(DefaultBodyLimit::max(limits::WEBHOOK_BODY)),
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

/// Every item of GitHub's list at `path`. Fails past `limit` items.
async fn list<T: DeserializeOwned>(github: &Octocrab, path: &str, limit: usize) -> Result<Vec<T>> {
    // GitHub's maximum.
    const PER_PAGE: usize = 100;
    let mut page: Page<T> = github.get(path, Some(&[("per_page", PER_PAGE)])).await?;
    let mut items = Vec::new();
    for _ in 0..limit.div_ceil(PER_PAGE) {
        items.append(&mut page.items);
        match github.get_page(&page.next).await? {
            Some(next) => page = next,
            None => return Ok(items),
        }
    }
    Err(format!("more than {limit} items at {path}").into())
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

    #[test]
    fn config_is_valid() {
        Config::command().debug_assert();
    }

    #[tokio::test]
    async fn webhook_accepts_only_signed_deliveries() {
        // A `ping` signed with the secret `secret`, from octocrab's documentation.
        const PING: &str = r#"{"zen":"Design for failure.","hook_id":423885699,"hook":{"type":"App","id":423885699,"name":"web","active":true,"events":["issues"],"config":{"content_type":"json","insecure_ssl":"0","url":"https://smee.io/R"},"updated_at":"2023-07-13T09:30:45Z","created_at":"2023-07-13T09:30:45Z","app_id":360617,"deliveries_url":"https://api.github.com/app/hook/deliveries"}}"#;
        const SIGNED: &str =
            "sha256=f101961424ae12c9b9d55cd4e562f4040c27ff1b9a8b7d2c4ba5cfc15e62d54f";

        let router = router(sync::tests::app().0);

        for (signature, body, status) in [
            (SIGNED, PING.into(), StatusCode::ACCEPTED),
            ("", PING.into(), StatusCode::UNAUTHORIZED),
            (
                SIGNED,
                PING.replace("failure", "success"),
                StatusCode::UNAUTHORIZED,
            ),
        ] {
            let request = Request::post("/webhook")
                .header("x-github-event", "ping")
                .header("x-hub-signature-256", signature)
                .body(Body::from(body))
                .unwrap();
            assert_eq!(
                router.clone().oneshot(request).await.unwrap().status(),
                status
            );
        }
    }
}

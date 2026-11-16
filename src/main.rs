// Register the app on GitHub with:
// - Callback URL `https://<host>/callback`, listed first.
// - Setup URL `https://<host>/setup`, with "Redirect on update" checked.
// - "Request user authorization (OAuth) during installation" unchecked.
// - Webhook URL `https://<host>/webhook`, with a random secret.

#![forbid(unsafe_code)]

mod app;
mod sign_in;
mod sync;

use std::{error::Error, future::IntoFuture, net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::{Request, header::ACCEPT},
    routing::{get, post},
};
use clap::Parser;
use octocrab::{
    Octocrab,
    secrecy::{ExposeSecret, SecretString},
};
use tokio::{
    net::TcpListener,
    signal::unix::{SignalKind, signal},
    sync::mpsc::{self, Sender},
};
use tokio_util::sync::CancellationToken;
use tower_http::trace::{DefaultOnResponse, TraceLayer};
use tracing::{Level, info, info_span};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version)]
struct Config {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:3000")]
    listen: SocketAddr,

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

/// Reads a secret from a file, without the trailing newline most editors and `echo` leave.
fn read_secret(path: &str) -> std::io::Result<SecretString> {
    Ok(std::fs::read_to_string(path)?.trim_end().into())
}

struct App {
    config: Config,
    github: Octocrab,
    /// Signs people in, on github.com rather than api.github.com.
    oauth: Octocrab,
    jobs: Sender<sync::Job>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let config = Config::parse();

    // octocrab waits forever by default, and one stalled request would stall all syncing.
    let client = || {
        let timeout = Some(Duration::from_secs(30));

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

    let (jobs, queue) = mpsc::channel(1024);
    let stop = CancellationToken::new();
    let resync = stop.run_until_cancelled(sync::resync(github.clone(), jobs.clone()));
    let work = sync::work(github.clone(), queue, stop.clone());

    let listener = TcpListener::bind(config.listen).await?;
    info!(address = %config.listen, "listening");
    let app = App {
        config,
        github,
        oauth,
        jobs,
    };
    let serve = axum::serve(listener, router(app)).with_graceful_shutdown(shutdown(stop.clone()));

    // Polled together here rather than spawned, so that a panic in any of them unwinds out of
    // `main` and ends the process. On shutdown, the server finishes the requests in flight and
    // the worker its job in progress.
    let (served, (), _) = tokio::join!(serve.into_future(), work, resync);
    Ok(served?)
}

fn router(app: App) -> Router {
    Router::new()
        .route("/setup", get(sign_in::setup))
        .route("/callback", get(sign_in::callback))
        // GitHub sends up to 25 MB; axum accepts 2 MB by default.
        .route(
            "/webhook",
            post(sync::webhook).layer(DefaultBodyLimit::max(25 << 20)),
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
        .with_state(Arc::new(app))
}

/// Waits for Ctrl-C or SIGTERM, then tells the worker and resync to stop.
async fn shutdown(stop: CancellationToken) {
    let mut terminate = signal(SignalKind::terminate()).expect("a SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
    info!("shutting down");
    stop.cancel();
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

        let config = Config {
            listen: ([127, 0, 0, 1], 0).into(),
            app_id: 1,
            client_id: "unused".into(),
            private_key: "unused".into(),
            client_secret: "unused".into(),
            webhook_secret: "secret".into(),
        };
        let router = router(App {
            config,
            github: Octocrab::default(),
            oauth: Octocrab::default(),
            jobs: mpsc::channel(1).0,
        });

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

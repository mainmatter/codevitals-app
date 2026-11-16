//! When and for which repositories `app::sync` and `app::forget` run.
//!
//! 1. A [`webhook`] delivery, startup and an hourly [`resync`] name repositories that may have
//!    changed. Nothing else is taken from a delivery, so repeated or reordered deliveries are
//!    harmless and lost ones are made up by the resync, except a lost removal: the resync only
//!    sees what exists.
//! 2. [`schedule`] runs at most one sync per repository at a time. Any number of triggers during
//!    one cause exactly one more.
//! 3. [`run`] asks GitHub which installation of the app sees the repository, and syncs it as that
//!    installation, or forgets it if none does.

use std::{sync::Arc, time::Duration};

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use octocrab::{
    Octocrab, Page,
    models::{
        Installation, InstallationId, Repository, RepositoryId,
        webhook_events::{WebhookEvent, WebhookEventPayload, verification::verify_signature},
    },
    secrecy::ExposeSecret,
};
use serde::de::DeserializeOwned;
use tokio::time::timeout;
use tracing::{Instrument, info_span, warn};

use crate::{App, app, limits};

// 1. Triggers

/// `POST /webhook`
pub async fn webhook(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let header = |name| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
    };
    let secret = app.config.webhook_secret.expose_secret();
    if let Err(error) = verify_signature(header("x-hub-signature-256"), secret, &body) {
        warn!(%error, "webhook rejected");
        return StatusCode::UNAUTHORIZED;
    }

    // Parsed in full, so that malformed deliveries are rejected.
    let event = match WebhookEvent::try_from_header_and_body(header("x-github-event"), &body) {
        Ok(event) => event,
        Err(error) => {
            warn!(%error, "webhook unreadable");
            return StatusCode::BAD_REQUEST;
        }
    };
    let named = match event.specific {
        WebhookEventPayload::Installation(payload) => payload.repositories.unwrap_or_default(),
        WebhookEventPayload::InstallationRepositories(payload) => {
            [payload.repositories_added, payload.repositories_removed].concat()
        }
        _ => Vec::new(),
    };
    let repositories = (event.repository.map(|repository| repository.id).into_iter())
        .chain(named.into_iter().map(|repository| repository.id));
    for repository in repositories {
        if let Err(error) = schedule(&app, repository) {
            warn!(%error, "webhook turned away");
            return StatusCode::SERVICE_UNAVAILABLE;
        }
    }
    StatusCode::ACCEPTED
}

/// GitHub never redelivers a failed webhook; this catches up.
const RESYNC_EVERY: Duration = Duration::from_secs(60 * 60);

pub async fn resync(app: Arc<App>) {
    let mut interval = tokio::time::interval(RESYNC_EVERY);
    loop {
        interval.tick().await;
        if let Err(error) = schedule_all(&app).await {
            warn!(%error, "resync failed");
        }
    }
}

async fn schedule_all(app: &Arc<App>) -> Result {
    let apps = app.github.apps();
    let page = apps.installations().per_page(PER_PAGE as u8).send().await?;
    for installation in all_pages(&app.github, page, limits::INSTALLATIONS).await? {
        // One at a time, so that a suspended installation does not hold up the others.
        if let Err(error) = schedule_installation(app, installation.id).await {
            warn!(installation = installation.id.0, %error, "resync failed");
        }
    }
    Ok(())
}

async fn schedule_installation(app: &Arc<App>, installation: InstallationId) -> Result {
    let github = app.github.installation(installation)?;
    let page = github.get(
        "/installation/repositories",
        Some(&[("per_page", PER_PAGE)]),
    );
    let repositories: Vec<Repository> =
        all_pages(&github, page.await?, limits::REPOSITORIES).await?;
    for repository in repositories {
        schedule(app, repository.id)?;
    }
    Ok(())
}

// 2. Scheduling

/// A trigger while a sync of `repository` waits or runs makes it run once more afterwards.
fn schedule(app: &Arc<App>, repository: RepositoryId) -> Result {
    let mut pending = app.pending.lock().unwrap();
    if pending.len() >= limits::REPOSITORIES && !pending.contains_key(&repository) {
        return Err("too many repositories".into());
    }
    if pending.insert(repository, true).is_none() {
        tokio::spawn(run(app.clone(), repository));
    }
    Ok(())
}

async fn run(app: Arc<App>, repository: RepositoryId) {
    loop {
        // Per run, and handed out in order, so that busy repositories take turns with the rest.
        let _permit = app.permits.acquire().await.unwrap();
        {
            let mut pending = app.pending.lock().unwrap();
            if !std::mem::take(pending.get_mut(&repository).unwrap()) {
                pending.remove(&repository);
                return;
            }
        }
        let synced = async {
            match context(&app.github, repository).await? {
                Some(context) => app::sync(&context).await,
                None => app::forget(repository).await,
            }
        };
        let span = info_span!("sync", repository = repository.0);
        let synced = timeout(limits::SYNC_TIMEOUT, synced).instrument(span).await;
        if let Err(error) = synced.unwrap_or_else(|elapsed| Err(elapsed.into())) {
            warn!(repository = repository.0, %error, "sync failed");
        }
    }
}

// 3. Sync or forget

/// `None` if GitHub answers 404: no installation of the app sees the repository.
///
/// `/repositories/{id}` is an undocumented alias of `/repos/{owner}/{repo}`, stable for years.
async fn context(github: &Octocrab, repository: RepositoryId) -> Result<Option<app::Context>> {
    let found = async {
        let path = format!("/repositories/{}/installation", repository.0);
        let installation: Installation = github.get(path, None::<&()>).await?;
        let (github, token) = github.installation_and_token(installation.id).await?;
        let found = github.repos_by_id(repository).get().await?;
        Ok((github, token, found))
    };
    let (github, token, found) = match found.await {
        Err(octocrab::Error::GitHub { source, .. }) if source.status_code == 404 => {
            return Ok(None);
        }
        found => found?,
    };
    Ok(Some(app::Context {
        repository,
        full_name: found.full_name.ok_or("no full name")?,
        default_branch: found.default_branch.ok_or("no default branch")?,
        github,
        token,
    }))
}

// Plumbing

pub type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// GitHub's maximum.
const PER_PAGE: usize = 100;

/// Fails past `limit` items.
async fn all_pages<T: DeserializeOwned>(
    github: &Octocrab,
    mut page: Page<T>,
    limit: usize,
) -> Result<Vec<T>> {
    let mut items = Vec::new();
    for _ in 0..limit.div_ceil(PER_PAGE) {
        items.append(&mut page.items);
        match github.get_page(&page.next).await? {
            Some(next) => page = next,
            None => return Ok(items),
        }
    }
    Err(format!("more than {limit} items").into())
}

#[cfg(test)]
pub mod tests {
    use std::{
        convert::Infallible,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use axum::http::{Request, Response, Uri};
    use octocrab::{
        AuthState, EncodingKey, OctoBody, OctocrabBuilder, auth::AppAuth,
        service::middleware::base_uri::BaseUriLayer,
    };
    use serde_json::json;
    use tokio::sync::Semaphore;
    use tower::service_fn;

    use super::*;

    const REPOSITORY: RepositoryId = RepositoryId(42);

    #[tokio::test]
    async fn pushes_during_a_sync_cause_one_more() {
        let (app, fake) = app();
        for _ in 0..10 {
            schedule(&app, REPOSITORY).unwrap();
            settle().await;
        }
        fake.release.add_permits(10);
        settle().await;
        assert_eq!(fake.reads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn an_uninstalled_repository_is_forgotten() {
        let (app, _) = app();
        let context = context(&app.github, RepositoryId(43)).await;
        assert!(context.unwrap().is_none());
    }

    /// GitHub where installation 7 sees only [`REPOSITORY`]. Each read of it is counted, then
    /// held until `release`.
    pub struct Fake {
        reads: AtomicUsize,
        release: Semaphore,
    }

    pub fn app() -> (Arc<App>, Arc<Fake>) {
        let fake = Arc::new(Fake {
            reads: AtomicUsize::new(0),
            release: Semaphore::new(0),
        });
        let faked = fake.clone();
        let github = service_fn(move |request: Request<OctoBody>| {
            let fake = faked.clone();
            async move {
                let path = request.uri().path();
                let (status, body) = if path == "/repositories/42/installation" {
                    (StatusCode::OK, installation())
                } else if path == "/app/installations/7/access_tokens" {
                    let token = json!({
                        "token": "t",
                        "expires_at": "2099-01-01T00:00:00Z",
                        "permissions": {},
                    });
                    (StatusCode::CREATED, token)
                } else if path == "/repositories/42" {
                    fake.reads.fetch_add(1, Ordering::SeqCst);
                    fake.release.acquire().await.unwrap().forget();
                    let repository = json!({
                        "id": 42,
                        "name": "r",
                        "full_name": "o/r",
                        "default_branch": "main",
                        "url": "https://api.github.com/repos/o/r",
                    });
                    (StatusCode::OK, repository)
                } else {
                    (StatusCode::NOT_FOUND, json!({ "message": "Not Found" }))
                };
                let response = Response::builder().status(status);
                Ok::<_, Infallible>(response.body(body.to_string()).unwrap())
            }
        });

        let key = EncodingKey::from_rsa_pem(include_bytes!("testdata/fake-app-key.pem")).unwrap();
        let auth = AuthState::App(AppAuth {
            app_id: 1.into(),
            key,
        });
        let github = OctocrabBuilder::new_empty()
            .with_service(github)
            .with_layer(&BaseUriLayer::new(Uri::from_static("http://github.test")))
            .with_auth(auth)
            .build()
            .unwrap();
        let config = crate::Config {
            listen: ([127, 0, 0, 1], 0).into(),
            app_id: 1,
            client_id: "unused".into(),
            private_key: "unused".into(),
            client_secret: "unused".into(),
            webhook_secret: "secret".into(),
        };
        let app = App::new(config, github.clone(), github);
        (app, fake)
    }

    fn installation() -> serde_json::Value {
        let url = "https://github.test";
        json!({
            "id": 7,
            "account": {
                "login": "o", "id": 1, "node_id": "n", "gravatar_id": "", "type": "User",
                "site_admin": false, "avatar_url": url, "url": url, "html_url": url,
                "followers_url": url, "following_url": url, "gists_url": url, "starred_url": url,
                "subscriptions_url": url, "organizations_url": url, "repos_url": url,
                "events_url": url, "received_events_url": url,
            },
            "permissions": {},
            "events": [],
        })
    }

    /// Runs every other task until it waits. Tests run on one thread.
    async fn settle() {
        for _ in 0..1_000 {
            tokio::task::yield_now().await;
        }
    }
}

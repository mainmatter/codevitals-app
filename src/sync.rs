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
    Octocrab,
    models::{
        Installation, InstallationId, Repository, RepositoryId,
        webhook_events::{WebhookEvent, WebhookEventPayload, verification::verify_signature},
    },
    secrecy::ExposeSecret,
};
use tracing::{Instrument, info_span, warn};

use crate::{App, Result, app, limits, list};

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
    let installations: Vec<Installation> =
        list(&app.github, "/app/installations", limits::INSTALLATIONS).await?;
    for installation in installations {
        // One at a time, so that a suspended installation does not hold up the others.
        if let Err(error) = schedule_installation(app, installation.id).await {
            warn!(installation = installation.id.0, %error, "resync failed");
        }
    }
    Ok(())
}

async fn schedule_installation(app: &Arc<App>, installation: InstallationId) -> Result {
    let github = app.github.installation(installation)?;
    let repositories: Vec<Repository> =
        list(&github, "/installation/repositories", limits::REPOSITORIES).await?;
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
                Some(context) => app::sync(&app, &context).await,
                None => app::forget(&app, repository).await,
            }
        };
        // Never cancelled: blocking work would outlive it, and the next run could overlap it.
        let span = info_span!("sync", repository = repository.0);
        if let Err(error) = synced.instrument(span).await {
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
    if found.size.ok_or("no size")? > limits::REPOSITORY_KB {
        return Err(format!("more than {} KB on GitHub", limits::REPOSITORY_KB).into());
    }
    Ok(Some(app::Context {
        repository,
        full_name: found.full_name.ok_or("no full name")?,
        default_branch: found.default_branch.ok_or("no default branch")?,
        github,
        token,
    }))
}

#[cfg(test)]
pub mod tests {
    use std::{
        convert::Infallible,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use axum::http::{Request, Response, Uri};
    use octocrab::{
        AuthState, OctoBody, OctocrabBuilder, service::middleware::base_uri::BaseUriLayer,
    };
    use serde_json::{Value, json};
    use tokio::sync::Semaphore;
    use tower::service_fn;

    use super::*;

    /// GitHub counts each request and holds it until released, then fails it: each sync ends at
    /// its first request.
    #[tokio::test]
    async fn pushes_during_a_sync_cause_one_more() {
        let (requests, release) = (Arc::new(AtomicUsize::new(0)), Arc::new(Semaphore::new(0)));
        let (counted, held) = (requests.clone(), release.clone());
        let app = app(github(move |_| {
            let (counted, held) = (counted.clone(), held.clone());
            async move {
                counted.fetch_add(1, Ordering::SeqCst);
                held.acquire().await.unwrap().forget();
                (StatusCode::BAD_REQUEST, json!({ "message": "held" }))
            }
        }));
        for _ in 0..10 {
            schedule(&app, RepositoryId(42)).unwrap();
            settle().await;
        }
        release.add_permits(10);
        settle().await;
        assert_eq!(requests.load(Ordering::SeqCst), 2);
    }

    /// Acting as `github`, with a database it never reaches.
    pub fn app(github: Octocrab) -> Arc<App> {
        let config = crate::Config {
            listen: ([127, 0, 0, 1], 0).into(),
            database: "postgres://unused".parse().unwrap(),
            checkouts: "/unused".into(),
            tracked_branches: Vec::new(),
            app_id: 1,
            client_id: "unused".into(),
            private_key: "unused".into(),
            client_secret: "unused".into(),
            webhook_secret: "secret".into(),
        };
        App::new(config, github.clone(), github)
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

    /// Runs every other task until it waits. Tests run on one thread.
    async fn settle() {
        for _ in 0..1_000 {
            tokio::task::yield_now().await;
        }
    }
}

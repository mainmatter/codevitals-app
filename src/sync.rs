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
        Installation, Repository, RepositoryId, webhook_events::verification::verify_signature,
    },
    secrecy::ExposeSecret,
};
use serde::Deserialize;
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

    let delivery: Delivery = match serde_json::from_slice(&body) {
        Ok(delivery) => delivery,
        Err(error) => {
            warn!(%error, "webhook unreadable");
            return StatusCode::BAD_REQUEST;
        }
    };
    let named = [
        delivery.repository.into_iter().collect(),
        delivery.repositories.unwrap_or_default(),
        delivery.repositories_added.unwrap_or_default(),
        delivery.repositories_removed.unwrap_or_default(),
    ];
    for Named { id: repository } in named.into_iter().flatten() {
        if let Err(error) = schedule(&app, repository) {
            warn!(%error, "webhook turned away");
            return StatusCode::SERVICE_UNAVAILABLE;
        }
    }
    StatusCode::ACCEPTED
}

/// The repositories any event names; GitHub's models of the rest change faster than octocrab's.
#[derive(Deserialize)]
struct Delivery {
    repository: Option<Named>,
    /// Of `installation` events.
    repositories: Option<Vec<Named>>,
    /// Of `installation_repositories` events.
    repositories_added: Option<Vec<Named>>,
    repositories_removed: Option<Vec<Named>>,
}

#[derive(Deserialize)]
struct Named {
    id: RepositoryId,
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
        let scheduled = async {
            let github = app.github.installation(installation.id)?;
            let repositories: Vec<Repository> =
                list(&github, "/installation/repositories", limits::REPOSITORIES).await?;
            repositories.iter().try_for_each(|r| schedule(app, r.id))
        };
        // One at a time, so that a suspended installation does not hold up the others.
        if let Err(error) = scheduled.await {
            warn!(installation = installation.id.0, %error, "resync failed");
        }
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
    let full_name = found.full_name.ok_or("no full name")?;
    Ok(Some(app::Context {
        repository,
        url: format!("https://github.com/{full_name}.git"),
        full_name,
        default_branch: found.default_branch.ok_or("no default branch")?,
        github,
        token,
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;
    use tokio::sync::Semaphore;

    use super::*;
    use crate::fixtures::{app, github};

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

    /// Runs every other task until it waits. Tests run on one thread.
    async fn settle() {
        for _ in 0..1_000 {
            tokio::task::yield_now().await;
        }
    }
}

//! Handing webhook events and repository syncs to the app.

use std::{
    collections::{HashMap, hash_map::Entry},
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use octocrab::{
    Octocrab,
    models::{
        InstallationId, Repository,
        webhook_events::{
            EventInstallation, WebhookEvent, WebhookEventPayload,
            payload::{InstallationRepositoriesWebhookEventAction, InstallationWebhookEventAction},
            verification::verify_signature,
        },
    },
    secrecy::ExposeSecret,
};
use tokio::{
    sync::mpsc::{Receiver, Sender},
    time::{MissedTickBehavior, timeout},
};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, Span, info, info_span, warn};

use crate::{App, app};

pub type Result = std::result::Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// How often every installation is synced.
const RESYNC_EVERY: Duration = Duration::from_secs(60 * 60);
/// How long each call to the app's `on_event` or `sync` may take before it is cancelled.
const APP_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Work for the app within one installation: an event to hand to it, then perhaps a sync of
/// every repository.
pub struct Job {
    installation: InstallationId,
    event: Option<Box<WebhookEvent>>,
    sync: bool,
    /// Where the job came from, so that what it logs can be traced back there.
    span: Span,
}

/// `POST /webhook`: GitHub reports an event. Queues it for the app.
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

    let event = match WebhookEvent::try_from_header_and_body(header("x-github-event"), &body) {
        Ok(event) => event,
        Err(error) => {
            warn!(%error, "webhook unreadable");
            return StatusCode::BAD_REQUEST;
        }
    };
    // Only `ping` comes from outside any installation.
    let Some(installation) = event.installation.as_ref().map(EventInstallation::id) else {
        return StatusCode::ACCEPTED;
    };

    // New installations and newly added repositories start out with a sync.
    let sync = match &event.specific {
        WebhookEventPayload::Installation(payload) => matches!(
            payload.action,
            InstallationWebhookEventAction::Created | InstallationWebhookEventAction::Unsuspend
        ),
        WebhookEventPayload::InstallationRepositories(payload) => {
            payload.action == InstallationRepositoriesWebhookEventAction::Added
        }
        _ => false,
    };

    let job = Job {
        installation,
        event: Some(Box::new(event)),
        sync,
        span: Span::current(),
    };
    match app.jobs.try_send(job) {
        Ok(()) => StatusCode::ACCEPTED,
        Err(error) => {
            warn!(%error, "webhook turned away");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

/// Queues a sync of every installation now, and again every [`RESYNC_EVERY`]. GitHub never retries
/// a failed webhook delivery, so this is what catches up on downtime and turned-away events.
pub async fn resync(github: Octocrab, jobs: Sender<Job>) {
    let mut interval = tokio::time::interval(RESYNC_EVERY);
    // After a resync held up by a full queue, wait a whole period rather than catching up.
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        if let Err(error) = queue_installations(&github, &jobs).await {
            warn!(%error, "resync failed");
        }
    }
}

async fn queue_installations(github: &Octocrab, jobs: &Sender<Job>) -> Result {
    let page = github.apps().installations().per_page(100).send().await?;
    let installations = github.all_pages(page).await?;
    info!(installations = installations.len(), "resyncing");

    let span = info_span!("resync");
    for installation in installations {
        let job = Job {
            installation: installation.id,
            event: None,
            sync: true,
            span: span.clone(),
        };
        jobs.send(job).await?;
    }
    Ok(())
}

/// Runs jobs one at a time until `stop`, finishing the job in progress.
pub async fn work(github: Octocrab, mut jobs: Receiver<Job>, stop: CancellationToken) {
    let mut installations = HashMap::new();
    while let Some(Some(job)) = stop.run_until_cancelled(jobs.recv()).await {
        let span = info_span!(parent: &job.span, "job", installation = job.installation.0);
        let started = Instant::now();
        match run(&github, &mut installations, job)
            .instrument(span.clone())
            .await
        {
            Ok(()) => span.in_scope(|| info!(elapsed = ?started.elapsed(), "done")),
            Err(error) => span.in_scope(|| warn!(%error, "failed")),
        }
    }
}

/// Keeps one client per installation in `installations`, so that its token is reused.
async fn run(
    github: &Octocrab,
    installations: &mut HashMap<InstallationId, Octocrab>,
    job: Job,
) -> Result {
    let github = match installations.entry(job.installation) {
        Entry::Occupied(entry) => entry.into_mut(),
        Entry::Vacant(entry) => entry.insert(github.installation(job.installation)?),
    };

    if let Some(event) = job.event {
        timeout(APP_TIMEOUT, app::on_event(github, *event)).await??;
    }
    if job.sync {
        sync(github).await?;
    }
    Ok(())
}

/// Syncs every repository of the installation `github` acts as.
async fn sync(github: &Octocrab) -> Result {
    let page = github.get("/installation/repositories", Some(&[("per_page", 100)]));
    for repository in github.all_pages::<Repository>(page.await?).await? {
        let span = info_span!("repository", name = repository.full_name);
        let synced = timeout(APP_TIMEOUT, app::sync(github, &repository)).instrument(span.clone());
        match synced.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => span.in_scope(|| warn!(%error, "sync failed")),
            Err(elapsed) => span.in_scope(|| warn!(error = %elapsed, "sync failed")),
        }
    }
    Ok(())
}

use octocrab::{
    Octocrab,
    models::{
        Repository,
        webhook_events::{WebhookEvent, WebhookEventPayload},
    },
};
use tracing::info;

use crate::sync::Result;

/// Acts on an event GitHub reported, with `github` acting as the installation it happened in.
/// Deliveries can be lost, so anything that must happen eventually belongs in [`sync`] too.
pub async fn on_event(_github: &Octocrab, event: WebhookEvent) -> Result {
    match event.specific {
        WebhookEventPayload::Push(push) => info!(commit = push.after, "pushed"),
        WebhookEventPayload::PullRequest(pull) => info!(number = pull.number, "pull request"),
        _ => info!(kind = ?event.kind, "ignored"),
    }
    Ok(())
}

/// Brings what the app keeps about `repository` up to date. Runs for every repository on
/// startup, periodically, and when the app is installed on it, so it must be idempotent.
pub async fn sync(_github: &Octocrab, _repository: &Repository) -> Result {
    info!("synced");
    Ok(())
}

//! What an operator sees of the app.
//!
//! 1. [`init`] writes logs to stderr and, given an OTLP URL, sends traces there. [`link_trace`]
//!    puts a root span's trace id in its logs.
//! 2. Metrics, recorded where things happen, and served to Prometheus by [`metrics`].

use std::{
    sync::{Arc, LazyLock},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{extract::State, http::StatusCode};
use opentelemetry::trace::{TraceContextExt, TraceId, TracerProvider as _};
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::{Resource, trace::SdkTracerProvider};
use prometheus::{
    Gauge, GaugeVec, HistogramVec, IntCounterVec, IntGauge, IntGaugeVec, TextEncoder,
    exponential_buckets, register_gauge, register_gauge_vec, register_histogram_vec,
    register_int_counter_vec, register_int_gauge, register_int_gauge_vec,
};
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::{
    EnvFilter, Layer, fmt::format::FmtSpan, layer::SubscriberExt, util::SubscriberInitExt,
};

use crate::{App, Result, limits};

// 1. Logs and traces

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum LogFormat {
    Text,
    /// A JSON object per line, with the fields of every enclosing span.
    Json,
}

/// Logs follow `RUST_LOG`; traces leave out the span of each blob. Returns what to flush on
/// shutdown.
pub fn init(format: LogFormat, otlp: Option<&str>) -> Result<Option<SdkTracerProvider>> {
    let logs = tracing_subscriber::fmt::layer().with_span_events(FmtSpan::CLOSE);
    let logs = match format {
        LogFormat::Text => logs.boxed(),
        LogFormat::Json => logs.json().flatten_event(true).with_span_list(true).boxed(),
    };
    let logs =
        logs.with_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()));

    let mut provider = None;
    if let Some(url) = otlp {
        let exporter = SpanExporter::builder()
            .with_http()
            .with_endpoint(url)
            .build()?;
        let resource = Resource::builder().with_service_name("cv").build();
        let built = SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(resource);
        provider = Some(built.build());
    }
    let traces = provider.as_ref().map(|provider| {
        tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("cv"))
            .with_filter(EnvFilter::new("info,octocrab=debug"))
    });

    tracing_subscriber::registry()
        .with(logs)
        .with(traces)
        .try_init()?;
    Ok(provider)
}

/// `span` declares an empty `trace_id`.
pub fn link_trace(span: &Span) {
    let trace = span.context().span().span_context().trace_id();
    if trace != TraceId::INVALID {
        span.record("trace_id", trace.to_string());
    }
}

// 2. Metrics

macro_rules! metric {
    ($name:ident: $type:ty = $register:expr) => {
        pub static $name: LazyLock<$type> =
            LazyLock::new(|| $register.expect("a metric registered once"));
    };
}

metric!(SYNC_SECONDS: HistogramVec = register_histogram_vec!(
    "cv_sync_duration_seconds", "Syncs and forgets that ended.", &["kind", "outcome"],
    exponential_buckets(1.0, 4.0, 8).unwrap()
));
metric!(STEP_SECONDS: HistogramVec = register_histogram_vec!(
    "cv_sync_step_duration_seconds", "Steps of syncs that ended.", &["step", "outcome"],
    exponential_buckets(0.01, 4.0, 10).unwrap()
));
metric!(REPO_LAST_SUCCESS: GaugeVec = register_gauge_vec!(
    "cv_repo_last_sync_success_timestamp_seconds", "When a repository last synced.",
    &["repository"]
));
metric!(REPO_FAILURES: IntGaugeVec = register_int_gauge_vec!(
    "cv_repo_sync_failures_consecutive", "Failed syncs of a repository since its last success.",
    &["repository"]
));
metric!(RESYNC_LAST_SUCCESS: Gauge = register_gauge!(
    "cv_resync_last_success_timestamp_seconds",
    "When a resync last reached every installation, or the process started."
));
metric!(WEBHOOKS: IntCounterVec = register_int_counter_vec!(
    "cv_webhook_deliveries_total",
    "Webhook deliveries; the event is `unverified` until the signature is.",
    &["event", "outcome"]
));
metric!(GITHUB_ERRORS: IntCounterVec = register_int_counter_vec!(
    "cv_github_errors_total", "GitHub's answers that failed a sync, after retries.", &["status"]
));
metric!(SYNCS_RUNNING: IntGauge = register_int_gauge!(
    "cv_syncs_running", "Syncs holding a turn."
));
metric!(SYNCS_PENDING: IntGauge = register_int_gauge!(
    "cv_syncs_pending", "Repositories with a sync waiting or running."
));

/// For timestamp gauges.
pub fn now() -> f64 {
    let since_epoch = SystemTime::now().duration_since(UNIX_EPOCH);
    since_epoch.unwrap_or_default().as_secs_f64()
}

/// `GET /metrics`
pub async fn metrics(
    State(app): State<Arc<App>>,
) -> std::result::Result<String, (StatusCode, String)> {
    SYNCS_RUNNING.set((limits::CONCURRENT_SYNCS - app.permits.available_permits()) as i64);
    SYNCS_PENDING.set(app.pending.lock().unwrap().len() as i64);
    let encoded = TextEncoder::new().encode_to_string(&prometheus::gather());
    encoded.map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))
}

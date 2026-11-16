//! Every bound. Exceeding one is an error.

use std::time::Duration;

/// GitHub sends up to 25 MB.
pub const WEBHOOK_BODY: usize = 25 << 20;
pub const GITHUB_REQUEST: Duration = Duration::from_secs(30);

pub const INSTALLATIONS: usize = 1_000;
/// Repositories served, across all installations.
pub const REPOSITORIES: usize = 10_000;

pub const CONCURRENT_SYNCS: usize = 4;
pub const SYNC_TIMEOUT: Duration = Duration::from_secs(15 * 60);

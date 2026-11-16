//! Every bound. Exceeding one is an error.

use std::time::Duration;

/// GitHub sends up to 25 MB.
pub const WEBHOOK_BODY: usize = 25 << 20;
pub const GITHUB_REQUEST: Duration = Duration::from_secs(30);

pub const INSTALLATIONS: usize = 1_000;
/// Repositories served, across all installations.
pub const REPOSITORIES: usize = 10_000;

pub const CONCURRENT_SYNCS: usize = 4;
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// As GitHub reports it, before fetching: there is no limit on the pack itself.
pub const REPOSITORY_KB: u32 = 10 << 20;
/// Refs in a checkout.
pub const CHECKOUT_REFS: usize = 500_000;
/// The largest single allocation while reading a fetched pack: an object, or a delta's result.
pub const OBJECT_BYTES: usize = 512 << 20;

/// Reachable from one set of tips. Linux has about 1.4 million.
pub const COMMITS: usize = 2_000_000;

/// Commits looked up in one GitHub GraphQL query.
pub const EMAILS_PER_QUERY: usize = 100;
/// An author and co-authors, as GitHub lists them: its maximum page.
pub const AUTHORS_PER_COMMIT: usize = 100;

//! Every bound. Exceeding one is an error, except for a blob's facts: its status says so instead.

use std::time::Duration;

use crate::Result;

/// An error if there are more than `max` of `what`.
pub fn check(count: usize, max: usize, what: &str) -> Result {
    if count > max {
        return Err(format!("more than {max} {what}").into());
    }
    Ok(())
}

/// Reads one past `max`, to tell whether there are more.
pub fn collect<T>(
    items: impl Iterator<Item = Result<T>>,
    max: usize,
    what: &str,
) -> Result<Vec<T>> {
    let items = items.take(max + 1).collect::<Result<Vec<_>>>()?;
    check(items.len(), max, what)?;
    Ok(items)
}

/// GitHub sends up to 25 MB.
pub const WEBHOOK_BODY: usize = 25 << 20;
/// Deliveries read at once: their memory is this many times [`WEBHOOK_BODY`].
pub const WEBHOOKS_AT_ONCE: usize = 8;
/// GitHub's own: it gives up on a delivery after 10 seconds.
pub const WEBHOOK_TIME: Duration = Duration::from_secs(10);
/// Sign-ins asking GitHub at once.
pub const SIGN_INS_AT_ONCE: usize = 8;
/// Of a whole sign-in, waiting for a turn included: a person is watching a browser.
pub const SIGN_IN_TIME: Duration = Duration::from_secs(30);
pub const GITHUB_REQUEST: Duration = Duration::from_secs(30);
/// Of one request, after a server or transport error, or a `429`.
pub const GITHUB_RETRIES: usize = 3;
/// Items in one page of a GitHub list: its maximum.
pub const PER_PAGE: usize = 100;

pub const INSTALLATIONS: usize = 1_000;
/// Repositories served, across all installations.
pub const REPOSITORIES: usize = 10_000;

pub const CONCURRENT_SYNCS: usize = 4;
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// As GitHub reports it, before fetching: there is no limit on the pack itself.
pub const REPOSITORY_KB: u32 = 10 << 20;
/// Refs in a mirror.
pub const MIRROR_REFS: usize = 500_000;
/// The largest single allocation while reading a fetched pack: an object, or a delta's result.
pub const OBJECT_BYTES: usize = 512 << 20;

/// Threads reading a step's items, each holding one, beside as many read and waiting to be
/// written: a step's memory is twice this many times one item's bound.
pub const READERS: usize = 16;

/// Reachable from one set of tips. Linux has about 1.4 million.
pub const COMMITS: usize = 2_000_000;

/// Without an account, on one repository's commits.
pub const EMAILS: usize = 1_000_000;
/// Commits looked up in one GitHub GraphQL query.
pub const EMAILS_PER_QUERY: usize = 100;
/// An author and co-authors, as GitHub lists them: its maximum page.
pub const AUTHORS_PER_COMMIT: usize = 100;

/// A larger side of a changed file has its lines not counted: both sides are held to count them.
pub const DIFF_BYTES: u64 = 8 << 20;
/// The paths of entries that differ between two commits, trees included, summed.
pub const CHANGED_PATH_BYTES: usize = 64 << 20;

/// Blobs without facts, on one repository's tracked branches.
pub const BLOBS: usize = 10_000_000;
/// A larger blob is `too_large`.
pub const FACT_BYTES: u64 = 1 << 20;
/// A deeper syntax tree is `too_deep`.
pub const SYNTAX_DEPTH: usize = 1_000;

/// In one repository. The largest have a few hundred thousand.
pub const PULLS: usize = 1_000_000;

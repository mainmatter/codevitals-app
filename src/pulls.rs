//! Every pull request of a repository, whatever its state, as `pull` rows.
//!
//! GitHub lists pull requests by update time, newest first. An update moves a pull request to the
//! front and shifts the rest back, so a list read page by page may repeat one but skips none.
//! A sync [`upsert`]s each pull request it reads, from the front down to `repo.pulls_since`, the
//! newest update the last sync saw.
//!
//! A first sync reads to the end instead, a page at a time from `repo.pulls_page`, and keeps the
//! newest update it saw first as `pulls_since`: cut short, it resumes, and the sync after it
//! reads what changed meanwhile. Once synced, a run reads one page.

use gix::ObjectId;
use octocrab::{Octocrab, models::RepositoryId};
use serde::Deserialize;
use sqlx::{
    PgPool,
    types::chrono::{DateTime, Utc},
};

use crate::{Pages, Result, limits};

type Time = DateTime<Utc>;

pub async fn record(
    github: &Octocrab,
    db: &PgPool,
    repository: RepositoryId,
    full_name: &str,
) -> Result {
    let repository = repository.0 as i64;
    let path = format!("/repos/{full_name}/pulls?state=all&sort=updated&direction=desc");
    let (since, page): (Option<Time>, Option<i32>) =
        sqlx::query_as("SELECT pulls_since, pulls_page FROM repo WHERE id = $1")
            .bind(repository)
            .fetch_one(db)
            .await?;
    let stop = since.filter(|_| page.is_none());
    let (mut number, mut newest) = (page.unwrap_or(1), since.filter(|_| page.is_some()));
    let mut pages = Pages::new(github, &path, limits::PULLS, number as usize);
    'read: while let Some(pulls) = pages.next::<Pull>().await? {
        for pull in &pulls {
            newest.get_or_insert(pull.updated_at);
            if stop.is_some_and(|stop| pull.updated_at < stop) {
                break 'read;
            }
            upsert(db, repository, pull).await?;
        }
        number += 1;
        if stop.is_none() {
            save(db, repository, newest, Some(number)).await?;
        }
    }
    save(db, repository, newest, None).await
}

/// As GitHub lists it. A deleted account is GitHub's `ghost`. Until merged, `merge_commit_sha` is
/// a trial merge GitHub redoes without moving `updated_at`, so it is kept only once merged.
#[derive(Deserialize)]
struct Pull {
    number: i32,
    state: String,
    draft: bool,
    base: Branch,
    head: Branch,
    created_at: Time,
    updated_at: Time,
    merged_at: Option<Time>,
    closed_at: Option<Time>,
    merge_commit_sha: Option<String>,
    user: Account,
}

#[derive(Deserialize)]
struct Branch {
    #[serde(rename = "ref")]
    name: String,
    sha: String,
}

#[derive(Deserialize)]
struct Account {
    id: i64,
    login: String,
    #[serde(rename = "type")]
    kind: String,
}

async fn save(db: &PgPool, repository: i64, since: Option<Time>, page: Option<i32>) -> Result {
    sqlx::query("UPDATE repo SET pulls_since = $2, pulls_page = $3 WHERE id = $1")
        .bind(repository)
        .bind(since)
        .bind(page)
        .execute(db)
        .await?;
    Ok(())
}

/// Never replaces a row with an older description: a pull request listed twice is written once.
async fn upsert(db: &PgPool, repository: i64, pull: &Pull) -> Result {
    let sha = |hex: &str| -> Result<Vec<u8>> {
        Ok(ObjectId::from_hex(hex.as_bytes())?.as_slice().to_vec())
    };
    sqlx::query(
        "INSERT INTO pull VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
         ON CONFLICT (repo, number) DO UPDATE SET
             (repo, number, state, draft, base_ref, head, created, updated, merged, closed,
              merge_commit, author_id, author_login, author_type) = ROW(EXCLUDED.*)
         WHERE pull.updated < EXCLUDED.updated",
    )
    .bind(repository)
    .bind(pull.number)
    .bind(&pull.state)
    .bind(pull.draft)
    .bind(&pull.base.name)
    .bind(sha(&pull.head.sha)?)
    .bind(pull.created_at)
    .bind(pull.updated_at)
    .bind(pull.merged_at)
    .bind(pull.closed_at)
    .bind(
        pull.merged_at
            .as_ref()
            .and(pull.merge_commit_sha.as_deref())
            .map(sha)
            .transpose()?,
    )
    .bind(pull.user.id)
    .bind(&pull.user.login)
    .bind(&pull.user.kind)
    .execute(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use axum::http::StatusCode;
    use serde_json::{Value, json};

    use super::*;
    use crate::fixtures::{github, repo, rows};

    /// GitHub's pull requests, numbered from 1: the minute since 2020 each was last updated, and
    /// whether it is merged.
    type Known = Arc<Mutex<Vec<(i64, bool)>>>;

    /// Serves `known`'s pages, newest update first, and counts them. Fails page 3 while `down`.
    fn fake(known: Known, down: Arc<AtomicBool>, pages: Arc<AtomicUsize>) -> Octocrab {
        github(move |request| {
            let (known, down, pages) = (known.clone(), down.clone(), pages.clone());
            async move {
                pages.fetch_add(1, Ordering::SeqCst);
                let query = request.uri().query().unwrap();
                let page: usize = query.split("page=").last().unwrap().parse().unwrap();
                if page == 3 && down.load(Ordering::SeqCst) {
                    return (StatusCode::BAD_REQUEST, json!({ "message": "down" }));
                }
                let mut listed = Vec::from_iter(known.lock().unwrap().iter().copied().enumerate());
                listed.sort_by_key(|&(_, (updated, _))| -updated);
                let time =
                    |minute: i64| format!("2020-01-01T{:02}:{:02}:00Z", minute / 60, minute % 60);
                let listed = listed.into_iter().skip((page - 1) * limits::PER_PAGE);
                let listed = listed.take(limits::PER_PAGE).map(|(i, (updated, merged))| {
                    json!({
                        "number": i + 1,
                        "state": if merged { "closed" } else { "open" },
                        "draft": false,
                        "base": { "ref": "main", "sha": format!("{:040x}", 0) },
                        "head": { "ref": "topic", "sha": format!("{i:040x}") },
                        "created_at": time(0),
                        "updated_at": time(updated),
                        "merged_at": merged.then(|| time(updated)),
                        "closed_at": merged.then(|| time(updated)),
                        "merge_commit_sha": format!("{:040x}", i + 1_000),
                        "user": { "id": 1, "login": "ada", "type": "User" },
                    })
                });
                (StatusCode::OK, Value::from_iter(listed))
            }
        })
    }

    /// A first sync of 250 pull requests fails at page 3. Meanwhile the oldest is merged, which
    /// moves it to the front. The next sync resumes at page 3, and the one after reads the front.
    #[sqlx::test]
    async fn a_first_sync_resumes_and_misses_nothing(db: PgPool) {
        repo(&db).await;
        let known: Known = Arc::new(Mutex::new((1..=250).map(|m| (m, false)).collect()));
        let (down, pages) = (
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicUsize::new(0)),
        );
        let github = fake(known.clone(), down.clone(), pages.clone());
        let shown = "SELECT concat_ws(' ', number, state, extract(epoch FROM updated)::bigint,
            (merge_commit IS NOT NULL)::text) FROM pull";
        let sync = async |expected_pages| {
            pages.store(0, Ordering::SeqCst);
            let synced = record(&github, &db, RepositoryId(1), "o/r").await;
            assert_eq!(pages.load(Ordering::SeqCst), expected_pages);
            synced
        };

        assert!(sync(3).await.is_err());
        assert_eq!(rows(&db, shown).await.len(), 200);

        known.lock().unwrap()[0] = (1439, true);
        down.store(false, Ordering::SeqCst);
        sync(1).await.unwrap();
        sync(1).await.unwrap();
        let known = known.lock().unwrap().clone();
        let mut expected =
            Vec::from_iter(known.iter().enumerate().map(|(i, (updated, merged))| {
                let state = if *merged { "closed" } else { "open" };
                format!(
                    "{} {state} {} {merged}",
                    i + 1,
                    1_577_836_800 + updated * 60
                )
            }));
        expected.sort();
        assert_eq!(rows(&db, shown).await, expected);
    }
}

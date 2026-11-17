//! Every pull request of a repository, whatever its state, as `pull` rows.
//!
//! GitHub lists pull requests by update time, newest first. An update moves a pull request to the
//! front and shifts the rest back, so a list read page by page may repeat one but skips none.
//! A sync [`upsert`]s each pull request it reads, from the front down to `repo.pulls_since`, and
//! once there moves `pulls_since` to the newest update it read. A first sync reads to the end; one
//! cut short starts over.

use octocrab::Octocrab;
use serde_json::Value;
use sqlx::{
    PgPool,
    types::chrono::{DateTime, Utc},
};

use crate::{Result, limits, page};

/// Returns how many it read.
pub async fn record(github: &Octocrab, db: &PgPool, repo: i64, full_name: &str) -> Result<usize> {
    let path = format!("/repos/{full_name}/pulls?state=all&sort=updated&direction=desc");
    let since: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT pulls_since FROM repo WHERE id = $1")
            .bind(repo)
            .fetch_one(db)
            .await?;
    let (mut newest, mut read) = (None, 0);
    'read: for number in 1.. {
        let pulls: Vec<Value> = page(github, &path, number).await?;
        limits::check(read + pulls.len(), limits::PULLS, "pull requests")?;
        for pull in &pulls {
            let updated: DateTime<Utc> = serde_json::from_value(pull["updated_at"].clone())?;
            newest.get_or_insert(updated);
            if since.is_some_and(|since| updated < since) {
                break 'read;
            }
            upsert(db, repo, pull).await?;
            read += 1;
        }
        if pulls.len() < limits::PER_PAGE {
            break;
        }
    }
    sqlx::query("UPDATE repo SET pulls_since = coalesce($2, pulls_since) WHERE id = $1")
        .bind(repo)
        .bind(newest)
        .execute(db)
        .await?;
    Ok(read)
}

/// As GitHub lists it, a field it leaves out an error. A deleted account is GitHub's `ghost`. Until
/// merged, `merge_commit_sha` is a trial merge GitHub redoes without moving `updated_at`, so it is
/// kept only once merged. Never replaces a row with an older description: a pull request listed
/// twice is written once.
async fn upsert(db: &PgPool, repo: i64, pull: &Value) -> Result {
    sqlx::query(
        r#"INSERT INTO pull
         SELECT $1, p.number, p.state, p.draft, p.base->>'ref', decode(p.head->>'sha', 'hex'),
             p.created_at, p.updated_at, p.merged_at, p.closed_at,
             decode(CASE WHEN p.merged_at IS NOT NULL THEN p.merge_commit_sha END, 'hex'),
             (p."user"->>'id')::bigint, p."user"->>'login', p."user"->>'type'
         FROM jsonb_to_record($2::jsonb) AS p (
             number int, state text, draft bool, base jsonb, head jsonb, created_at timestamptz,
             updated_at timestamptz, merged_at timestamptz, closed_at timestamptz,
             merge_commit_sha text, "user" jsonb
         )
         ON CONFLICT (repo, number) DO UPDATE SET
             (repo, number, state, draft, base_ref, head, created, updated, merged, closed,
              merge_commit, author_id, author_login, author_type) = ROW(EXCLUDED.*)
         WHERE pull.updated < EXCLUDED.updated"#,
    )
    .bind(repo)
    .bind(pull.to_string())
    .execute(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::http::StatusCode;
    use serde_json::{Value, json};

    use super::*;
    use crate::fixtures::{github, repo, rows};

    /// 250 pull requests are read in full. Then the oldest is merged, which moves it to the front,
    /// and the next sync reads one page.
    #[sqlx::test]
    async fn a_sync_reads_down_to_the_last_complete_read(db: PgPool) {
        repo(&db).await;
        // Numbered from 1: the minute since 2020 each was last updated, and whether it is merged.
        let known = Arc::new(Mutex::new(Vec::from_iter((1..=250).map(|m| (m, false)))));
        let pages = Arc::new(AtomicUsize::new(0));
        let (listed, counted) = (known.clone(), pages.clone());
        let github = github(move |request| {
            counted.fetch_add(1, Ordering::SeqCst);
            let query = request.uri().query().unwrap();
            let page: usize = query.split("page=").last().unwrap().parse().unwrap();
            let mut known = Vec::from_iter(listed.lock().unwrap().iter().copied().enumerate());
            known.sort_by_key(|&(_, (updated, _))| -updated);
            let time =
                |minute: i64| format!("2020-01-01T{:02}:{:02}:00Z", minute / 60, minute % 60);
            let known = known.into_iter().skip((page - 1) * limits::PER_PAGE);
            let page = known.take(limits::PER_PAGE).map(|(i, (updated, merged))| {
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
            let page = Value::from_iter(page);
            async move { (StatusCode::OK, page) }
        });
        let sync = async |expected_pages| {
            pages.store(0, Ordering::SeqCst);
            record(&github, &db, 1, "o/r").await.unwrap();
            assert_eq!(pages.load(Ordering::SeqCst), expected_pages);
        };

        sync(3).await;
        known.lock().unwrap()[0] = (1439, true);
        sync(1).await;

        let shown = "SELECT concat_ws(' ', number, state, extract(epoch FROM updated)::bigint,
            (merge_commit IS NOT NULL)::text) FROM pull";
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

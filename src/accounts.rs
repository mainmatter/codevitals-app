//! The GitHub account each email belongs to: the one link from git to a person.
//!
//! 1. [`unasked`]: each email on a reachable commit, in a role GitHub attributes, without an
//!    `email_account` row, with one commit it is on.
//! 2. [`ask`] GitHub for everyone it attributes on those commits, [`limits::EMAILS_PER_QUERY`] at
//!    a time.
//! 3. [`insert`] each email's account, or none if GitHub gave it none.
//!
//! GitHub links people to the account holding their email, so one commit answers for the email
//! everywhere. Asked once: an email added to an account later stays unlinked.

use std::collections::HashMap;

use gix::ObjectId;
use octocrab::Octocrab;
use serde::Deserialize;
use serde_json::json;
use sqlx::PgPool;

use crate::{Result, column, limits};

/// `commits` must be in the repository `full_name`.
pub async fn record(
    github: &Octocrab,
    db: &PgPool,
    full_name: &str,
    commits: &[ObjectId],
) -> Result {
    for batch in unasked(db, commits).await?.chunks(limits::EMAILS_PER_QUERY) {
        let mut accounts = ask(github, full_name, batch).await?;
        for (email, _) in batch {
            insert(db, email, accounts.remove(email).flatten()).await?;
        }
    }
    Ok(())
}

// 1. Unasked

/// Lowercased emails.
async fn unasked(db: &PgPool, commits: &[ObjectId]) -> Result<Vec<(String, ObjectId)>> {
    let rows: Vec<(String, Vec<u8>)> = sqlx::query_as(
        "SELECT DISTINCT ON (lower(p.email)) lower(p.email), p.commit
         FROM unnest($1::bytea[]) AS r (sha)
         JOIN commit_person p ON p.commit = r.sha
         WHERE p.role IN ('author', 'committer', 'co-authored-by')
             AND NOT EXISTS (SELECT FROM email_account a WHERE a.email = lower(p.email))",
    )
    .bind(column(commits, |sha| sha.as_slice()))
    .fetch_all(db)
    .await?;
    let unasked = rows
        .into_iter()
        .map(|(email, sha)| (email, ObjectId::from_bytes_or_panic(&sha)));
    Ok(unasked.collect())
}

// 2. Ask

#[derive(Deserialize)]
struct Account {
    #[serde(rename = "databaseId")]
    id: i64,
    login: String,
}

/// About the commit of each `(email, commit)`. By lowercased email; `authors` holds the author and
/// each co-author GitHub parsed.
async fn ask(
    github: &Octocrab,
    full_name: &str,
    unasked: &[(String, ObjectId)],
) -> Result<HashMap<String, Option<Account>>> {
    /// A `null` or a non-commit object fails to parse.
    #[derive(Deserialize)]
    struct Data {
        repository: HashMap<String, Commit>,
    }
    #[derive(Deserialize)]
    struct Commit {
        authors: Authors,
        committer: Actor,
    }
    #[derive(Deserialize)]
    struct Authors {
        #[serde(rename = "totalCount")]
        total: usize,
        nodes: Vec<Actor>,
    }
    #[derive(Deserialize)]
    struct Actor {
        email: Option<String>,
        user: Option<Account>,
    }

    const ACTOR: &str = "email user { databaseId login }";
    let max = limits::AUTHORS_PER_COMMIT;
    let objects: String = (unasked.iter().enumerate())
        .map(|(i, (_, commit))| {
            format!(
                "c{i}: object(oid: \"{commit}\") {{ ... on Commit {{
                     authors(first: {max}) {{ totalCount nodes {{ {ACTOR} }} }}
                     committer {{ {ACTOR} }}
                 }} }}"
            )
        })
        .collect();
    let (owner, name) = full_name
        .split_once('/')
        .ok_or("a full name without an owner")?;
    let request = json!({
        "query": format!(
            "query($owner: String!, $name: String!) {{
                 repository(owner: $owner, name: $name) {{ {objects} }}
             }}"
        ),
        "variables": { "owner": owner, "name": name },
    });
    let data: Data = github.graphql(&request).await?;

    if data.repository.len() != unasked.len() {
        return Err("an answer missing a commit asked about".into());
    }
    let mut accounts = HashMap::new();
    for commit in data.repository.into_values() {
        limits::check(commit.authors.total, max, "authors on a commit")?;
        for actor in commit.authors.nodes.into_iter().chain([commit.committer]) {
            if let Some(email) = actor.email {
                let account = accounts.entry(email.to_lowercase()).or_insert(None);
                *account = account.take().or(actor.user);
            }
        }
    }
    Ok(accounts)
}

// 3. Insert

async fn insert(db: &PgPool, email: &str, account: Option<Account>) -> Result {
    let (id, login) = account.map(|account| (account.id, account.login)).unzip();
    sqlx::query("INSERT INTO email_account VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
        .bind(email)
        .bind(id)
        .bind(login)
        .execute(db)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use axum::{
        body::{Body, to_bytes},
        http::StatusCode,
    };
    use serde_json::Value;

    use super::*;
    use crate::{
        commits,
        fixtures::{git, github, rows, scratch},
        mirror::Mirror,
    };

    /// Ada commits from work, then from elsewhere with Ada from home as co-author. Work and home
    /// are her account's emails; elsewhere is another Ada, unknown to GitHub.
    #[sqlx::test]
    async fn a_person_is_their_account_not_their_name(db: PgPool) {
        let mirror = Mirror(scratch("accounts"));
        let work = &mirror.0;
        let at_work = "--author=Ada <ada@work.example>";
        let elsewhere = "--author=Ada <ada@elsewhere.example>";
        let home = "--trailer=Co-authored-by: Ada <ADA@home.example>";
        git(work, &["init", "--initial-branch=main"]);
        git(work, &["commit", "--allow-empty", at_work, "-m", "1"]);
        git(
            work,
            &["commit", "--allow-empty", "-m", "2", elsewhere, home],
        );
        let tip: ObjectId = git(work, &["rev-parse", "main"]).parse().unwrap();
        let first = git(work, &["rev-parse", "main~"]);

        let actor = |email: &str| {
            let user = ["ada@work.example", "ada@home.example"].contains(&&*email.to_lowercase());
            json!({ "email": email, "user": user.then(|| json!({ "databaseId": 1, "login": "ada" })) })
        };
        let on = |authors: &[&str]| {
            let nodes: Vec<Value> = authors.iter().map(|email| actor(email)).collect();
            json!({
                "authors": { "totalCount": nodes.len(), "nodes": nodes },
                "committer": actor("a@example.com"),
            })
        };
        let github_commits = HashMap::from([
            (first, on(&["ada@work.example"])),
            (
                tip.to_string(),
                on(&["ada@elsewhere.example", "ADA@home.example"]),
            ),
        ]);
        let queries = Arc::new(AtomicUsize::new(0));
        let counted = queries.clone();
        let github = github(move |request| {
            counted.fetch_add(1, Ordering::SeqCst);
            let github_commits = github_commits.clone();
            async move {
                let body = to_bytes(Body::new(request.into_body()), usize::MAX);
                let body: Value = serde_json::from_slice(&body.await.unwrap()).unwrap();
                assert_eq!(body["variables"], json!({ "owner": "o", "name": "r" }));
                let query = body["query"].as_str().unwrap();
                let repository: serde_json::Map<_, _> = (query.split("object(oid: \"").skip(1))
                    .enumerate()
                    .map(|(i, object)| (format!("c{i}"), github_commits[&object[..40]].clone()))
                    .collect();
                (
                    StatusCode::OK,
                    json!({ "data": { "repository": repository } }),
                )
            }
        });

        for _ in 0..2 {
            let commits = commits::record(&db, &mirror, vec![tip]).await.unwrap();
            record(&github, &db, "o/r", &commits).await.unwrap();
        }
        // Four emails, one query; none left to ask the second time.
        assert_eq!(queries.load(Ordering::SeqCst), 1);

        let persons = "SELECT DISTINCT format('%s %s <%s>: %s', role, name, email, person)
            FROM commit_person_key";
        assert_eq!(
            rows(&db, persons).await,
            [
                "author Ada <ada@elsewhere.example>: email:ada@elsewhere.example",
                "author Ada <ada@work.example>: account:1",
                "co-authored-by Ada <ADA@home.example>: account:1",
                "committer A <a@example.com>: email:a@example.com",
            ]
        );
        fs::remove_dir_all(work).unwrap();
    }
}

//! The GitHub account each email belongs to: the one link from git to a person.
//!
//! 1. [`unasked`]: each email in a role GitHub attributes, on a commit the repository reaches,
//!    without an `email_account` row, with one such commit.
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

use crate::{Result, limits};

/// `full_name` names the repository `repo`.
pub async fn record(github: &Octocrab, db: &PgPool, repo: i64, full_name: &str) -> Result<usize> {
    let unasked = unasked(db, repo).await?;
    for batch in unasked.chunks(limits::EMAILS_PER_QUERY) {
        let mut accounts = ask(github, full_name, batch).await?;
        for (email, _) in batch {
            insert(db, email, accounts.remove(email).flatten()).await?;
        }
    }
    Ok(unasked.len())
}

// 1. Unasked

/// Lowercased emails.
async fn unasked(db: &PgPool, repo: i64) -> Result<Vec<(String, ObjectId)>> {
    let rows: Vec<(String, Vec<u8>)> = sqlx::query_as(
        "SELECT DISTINCT ON (lower(p.email)) lower(p.email), p.commit
         FROM repo_commit r JOIN commit_person p ON p.commit = r.commit
         WHERE r.repo = $1 AND p.role IN ('author', 'committer', 'co-authored-by')
             AND NOT EXISTS (SELECT FROM email_account a WHERE a.email = lower(p.email))
         LIMIT $2",
    )
    .bind(repo)
    .bind(limits::EMAILS as i64 + 1)
    .fetch_all(db)
    .await?;
    limits::check(rows.len(), limits::EMAILS, "emails without accounts")?;
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
    /// `null` for a commit GitHub no longer has, after a force-push: its emails get no account.
    #[derive(Deserialize)]
    struct Data {
        repository: HashMap<String, Option<Commit>>,
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
    let mut accounts = HashMap::new();
    for commit in data.repository.into_values().flatten() {
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

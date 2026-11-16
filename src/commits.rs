//! Records every commit reachable from a set of tips, with everyone on it, and returns them.
//!
//! 1. [`reachable`]: the commits that should have a row.
//! 2. [`missing`]: those without one.
//! 3. [`insert`] each with its people.
//!
//! A commit's people are its author, its committer, and each trailer whose key ends in `-by`, with
//! the key in lower case as role. Names and emails are as git wrote them; text that is not UTF-8
//! is rejected.

use gix::{
    ObjectId,
    bstr::{BStr, ByteSlice},
};
use sqlx::PgPool;
use tokio::runtime::Handle;

use crate::{Result, checkout::Checkout, limits};

pub async fn record(
    db: &PgPool,
    checkout: &Checkout,
    tips: Vec<ObjectId>,
) -> Result<Vec<ObjectId>> {
    let (db, checkout, runtime) = (db.clone(), checkout.clone(), Handle::current());
    tokio::task::spawn_blocking(move || {
        let repo = checkout.open()?;
        let reachable = reachable(&repo, tips)?;
        for sha in runtime.block_on(missing(&db, &reachable))? {
            runtime.block_on(insert(&db, &repo, sha))?;
        }
        Ok(reachable)
    })
    .await?
}

// 1. Reachable

fn reachable(repo: &gix::Repository, tips: Vec<ObjectId>) -> Result<Vec<ObjectId>> {
    let walk = repo.rev_walk(tips).all()?.take(limits::COMMITS + 1);
    let commits = walk.map(|info| Ok(info?.id)).collect::<Result<Vec<_>>>()?;
    if commits.len() > limits::COMMITS {
        return Err(format!("more than {} reachable commits", limits::COMMITS).into());
    }
    Ok(commits)
}

// 2. Missing

async fn missing(db: &PgPool, commits: &[ObjectId]) -> Result<Vec<ObjectId>> {
    let shas: Vec<&[u8]> = commits.iter().map(|sha| sha.as_slice()).collect();
    let missing: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT s FROM unnest($1::bytea[]) s WHERE NOT EXISTS (SELECT FROM commit WHERE sha = s)",
    )
    .bind(&shas)
    .fetch_all(db)
    .await?;
    Ok(missing
        .iter()
        .map(|sha| ObjectId::from_bytes_or_panic(sha))
        .collect())
}

// 3. Insert

/// One statement, so a commit and its people go in together. Rows are determined by the sha, so
/// any that another sync inserts meanwhile are the same rows. People are bounded by the message, and
/// that by `OBJECT_BYTES`.
async fn insert(db: &PgPool, repo: &gix::Repository, sha: ObjectId) -> Result {
    let object = repo.find_commit(sha)?;
    let commit = object.decode()?;
    let (author, committer) = (commit.author()?, commit.committer()?);
    let (authored, committed) = (author.time()?, committer.time()?);
    let parents: Vec<Vec<u8>> = commit
        .parents()
        .map(|sha| sha.as_slice().to_vec())
        .collect();

    // Roles, names and emails, as columns, in message order.
    let mut people: [Vec<String>; 3] = Default::default();
    let mut add = |person: [&BStr; 3]| -> Result {
        for (column, field) in people.iter_mut().zip(person) {
            column.push(field.to_str()?.to_owned());
        }
        Ok(())
    };
    add(["author".into(), author.name, author.email])?;
    add(["committer".into(), committer.name, committer.email])?;
    for trailer in commit
        .message()
        .body()
        .iter()
        .flat_map(|body| body.trailers())
    {
        let role = trailer.token.to_ascii_lowercase();
        if let Some((name, email)) = identity(&trailer.value)
            && role.ends_with(b"-by")
        {
            add([role.as_bstr(), name, email])?;
        }
    }
    let [roles, names, emails] = people;

    sqlx::query(
        "WITH commit AS (
             INSERT INTO commit (sha, parents, author_time, author_tz, commit_time, commit_tz, subject)
             VALUES ($1, $2::sha[], to_timestamp($3::int8), $4, to_timestamp($5::int8), $6, $7)
             ON CONFLICT DO NOTHING
         )
         INSERT INTO commit_person (commit, role, position, name, email)
         SELECT $1, role, row_number() OVER (PARTITION BY role ORDER BY n) - 1, name, email
         FROM unnest($8::text[], $9::text[], $10::text[]) WITH ORDINALITY AS p (role, name, email, n)
         ON CONFLICT DO NOTHING",
    )
    .bind(sha.as_slice())
    .bind(&parents)
    .bind(authored.seconds)
    .bind(authored.offset / 60)
    .bind(committed.seconds)
    .bind(committed.offset / 60)
    .bind(commit.message().summary().to_str()?)
    .bind(roles)
    .bind(names)
    .bind(emails)
    .execute(db)
    .await?;
    Ok(())
}

/// `Name <email>`, split as git splits its own signatures, neither part empty nor holding an
/// angle bracket.
fn identity(value: &BStr) -> Option<(&BStr, &BStr)> {
    let (name, email) = value.strip_suffix(b">")?.split_once_str("<")?;
    let name = name.strip_suffix(b" ").unwrap_or(name);
    let valid = |part: &[u8]| !part.is_empty() && !part.contains(&b'<') && !part.contains(&b'>');
    (valid(name) && valid(email)).then(|| (name.as_bstr(), email.as_bstr()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::checkout::tests::{git, scratch};

    /// `main` holds a root commit, then a merge of a commit naming people in trailers.
    #[sqlx::test]
    async fn every_person_on_every_reachable_commit_is_recorded_once(db: PgPool) {
        let checkout = Checkout(scratch("commits"));
        let work = &checkout.0;
        let author = "--author=Ada <ada@example.com>";
        for args in [
            &["init", "--initial-branch=main"][..],
            &["commit", "--allow-empty", author, "-m", "root"],
            &["switch", "-c", "side"],
            &["commit", "--allow-empty", "-m", TRAILERS],
            &["switch", "main"],
            &["merge", "--no-ff", "-m", "merge", "side"],
        ] {
            git(work, args);
        }
        let main: ObjectId = git(work, &["rev-parse", "main"]).parse().unwrap();

        let rows = async || {
            let mut rows: Vec<String> = sqlx::query_scalar(
                "SELECT format('%s %s: %s %s %s <%s>', subject, author_tz, role, position, name, email)
                 FROM commit JOIN commit_person ON commit = sha",
            )
            .fetch_all(&db)
            .await
            .unwrap();
            rows.sort();
            rows
        };
        record(&db, &checkout, vec![main]).await.unwrap();
        let recorded = rows().await;
        assert_eq!(
            recorded,
            [
                "Teach the parser trailers 330: author 0 A <a@example.com>",
                "Teach the parser trailers 330: co-authored-by 0 Bea <bea@example.com>",
                "Teach the parser trailers 330: co-authored-by 1 Dan  <DAN@example.com>",
                "Teach the parser trailers 330: committer 0 A <a@example.com>",
                "Teach the parser trailers 330: reviewed-by 0 Eve <eve@example.com>",
                "Teach the parser trailers 330: signed-off-by 0 Ada <ada@example.com>",
                "merge 330: author 0 A <a@example.com>",
                "merge 330: committer 0 A <a@example.com>",
                "root 330: author 0 Ada <ada@example.com>",
                "root 330: committer 0 A <a@example.com>",
            ]
        );
        record(&db, &checkout, vec![main]).await.unwrap();
        assert_eq!(rows().await, recorded);
        fs::remove_dir_all(work).unwrap();
    }

    const TRAILERS: &str = "\
Teach the parser trailers

Fixes: #1
Co-authored-by: Bea <bea@example.com>
Co-Authored-By: Dan  <DAN@example.com>
Signed-off-by: Ada <ada@example.com>
Reviewed-by: Eve <eve@example.com>
Co-authored-by: just a name
Helped-by: <nobody@example.com>
";
}

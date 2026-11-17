//! Records every commit reachable from a set of tips, with everyone on it, and returns them.
//!
//! 1. [`reachable`]: the commits that should have rows.
//! 2. [`missing`]: those that have none.
//! 3. [`read_commit`] each, and [`insert`] it.
//!
//! A commit's people are its author, its committer, and each trailer whose key ends in `-by`, the
//! key in lower case as their role. Names and emails are as git wrote them.

use gix::{
    ObjectId,
    bstr::{BStr, ByteSlice},
};
use sqlx::PgPool;

use crate::{Result, column, limits, mirror::Mirror};

pub async fn record(db: &PgPool, mirror: &Mirror, tips: Vec<ObjectId>) -> Result<Vec<ObjectId>> {
    let reachable = mirror.run(move |repo| reachable(repo, tips)).await?;
    let missing = missing(db, &reachable).await?;
    mirror
        .read_each(&missing, read_commit, |commit| insert(db, commit))
        .await?;
    Ok(reachable)
}

// 1. Reachable

fn reachable(repo: &gix::Repository, tips: Vec<ObjectId>) -> Result<Vec<ObjectId>> {
    let walk = repo.rev_walk(tips).all()?.map(|info| Ok(info?.id));
    limits::collect(walk, limits::COMMITS, "reachable commits")
}

// 2. Missing

async fn missing(db: &PgPool, commits: &[ObjectId]) -> Result<Vec<ObjectId>> {
    let missing: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT c.sha FROM unnest($1::bytea[]) AS c (sha)
         WHERE NOT EXISTS (SELECT FROM commit WHERE commit.sha = c.sha)",
    )
    .bind(column(commits, |sha| sha.as_slice()))
    .fetch_all(db)
    .await?;
    Ok(missing
        .iter()
        .map(|sha| ObjectId::from_bytes_or_panic(sha))
        .collect())
}

// 3. Read and insert

/// A `commit` row and its `commit_person` rows.
struct Commit {
    sha: ObjectId,
    parents: Vec<Vec<u8>>,
    author_time: i64,
    author_tz: i32,
    commit_time: i64,
    commit_tz: i32,
    subject: String,
    people: Vec<Person>,
}

struct Person {
    role: String,
    name: String,
    email: String,
}

fn read_commit(repo: &gix::Repository, sha: ObjectId) -> Result<Commit> {
    let object = repo.find_commit(sha)?;
    let commit = object.decode()?;
    let (author, committer) = (commit.author()?, commit.committer()?);
    let (authored, committed) = (author.time()?, committer.time()?);
    let person = |role: &BStr, name: &BStr, email: &BStr| -> Result<Person> {
        Ok(Person {
            role: role.to_str()?.to_owned(),
            name: name.to_str()?.to_owned(),
            email: email.to_str()?.to_owned(),
        })
    };
    let mut people = vec![
        person("author".into(), author.name, author.email)?,
        person("committer".into(), committer.name, committer.email)?,
    ];
    for trailer in commit
        .message()
        .body()
        .iter()
        .flat_map(|body| body.trailers())
    {
        let role = trailer.token.to_ascii_lowercase();
        if role.ends_with(b"-by")
            && let Some((name, email)) = name_and_email(&trailer.value)
        {
            people.push(person(role.as_bstr(), name, email)?);
        }
    }
    Ok(Commit {
        sha,
        parents: commit
            .parents()
            .map(|parent| parent.as_slice().to_vec())
            .collect(),
        author_time: authored.seconds,
        author_tz: authored.offset / 60,
        commit_time: committed.seconds,
        commit_tz: committed.offset / 60,
        subject: commit.message().summary().to_str()?.to_owned(),
        people,
    })
}

/// `Name <email>`, split as git splits its own signatures: neither part empty nor holding an angle
/// bracket.
fn name_and_email(value: &BStr) -> Option<(&BStr, &BStr)> {
    let (name, email) = value.strip_suffix(b">")?.split_once_str("<")?;
    let name = name.strip_suffix(b" ").unwrap_or(name);
    let valid = |part: &[u8]| !part.is_empty() && !part.contains(&b'<') && !part.contains(&b'>');
    (valid(name) && valid(email)).then(|| (name.as_bstr(), email.as_bstr()))
}

/// One statement, so a commit and its people go in together.
async fn insert(db: &PgPool, commit: Commit) -> Result {
    let people = &commit.people;
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
    .bind(commit.sha.as_slice())
    .bind(&commit.parents)
    .bind(commit.author_time)
    .bind(commit.author_tz)
    .bind(commit.commit_time)
    .bind(commit.commit_tz)
    .bind(&commit.subject)
    .bind(column(people, |person| person.role.as_str()))
    .bind(column(people, |person| person.name.as_str()))
    .bind(column(people, |person| person.email.as_str()))
    .execute(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::{
        fixtures::{git, rows, scratch},
        mirror::Mirror,
    };

    /// `main` holds a root commit, then a merge of a commit naming people in trailers.
    #[sqlx::test]
    async fn every_person_on_every_reachable_commit_is_recorded_once(db: PgPool) {
        let mirror = Mirror(scratch("commits"));
        let work = &mirror.0;
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

        let people =
            "SELECT format('%s %s: %s %s %s <%s>', subject, author_tz, role, position, name, email)
            FROM commit JOIN commit_person ON commit = sha";
        record(&db, &mirror, vec![main]).await.unwrap();
        let recorded = rows(&db, people).await;
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
        record(&db, &mirror, vec![main]).await.unwrap();
        assert_eq!(rows(&db, people).await, recorded);
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

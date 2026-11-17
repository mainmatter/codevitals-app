//! Records every commit a repository's tips reach, with everyone on it, as `commit`,
//! `commit_person` and `repo_commit` rows.
//!
//! 1. [`reachable`]: the commits the repository should have.
//! 2. [`missing`]: those without its `repo_commit` row.
//! 3. [`read_commit`] each, and [`insert`] it: a commit another repository has is read again,
//!    which is simpler than telling the two apart.
//!
//! A commit's people are its author, its committer, and each trailer whose key ends in `-by`, the
//! key in lower case as their role. Names and emails are as git wrote them, read as [`text`].

use gix::{
    ObjectId,
    bstr::{BStr, ByteSlice},
};
use sqlx::PgPool;

use crate::{Result, column, limits, mirror::Mirror, text};

pub async fn record(db: &PgPool, mirror: &Mirror, repo: i64, tips: Vec<ObjectId>) -> Result<usize> {
    let reachable = mirror.run(move |git| reachable(git, tips)).await?;
    let missing = missing(db, repo, &reachable).await?;
    mirror
        .read_each(missing, read_commit, |commit| insert(db, repo, commit))
        .await
}

// 1. Reachable

fn reachable(git: &gix::Repository, tips: Vec<ObjectId>) -> Result<Vec<ObjectId>> {
    let walk = git.rev_walk(tips).all()?.map(|info| Ok(info?.id));
    limits::collect(walk, limits::COMMITS, "reachable commits")
}

// 2. Missing

async fn missing(db: &PgPool, repo: i64, commits: &[ObjectId]) -> Result<Vec<ObjectId>> {
    let missing: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT c.sha FROM unnest($2::bytea[]) AS c (sha)
         WHERE NOT EXISTS (SELECT FROM repo_commit r WHERE (r.repo, r.commit) = ($1, c.sha))",
    )
    .bind(repo)
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

fn read_commit(git: &gix::Repository, sha: ObjectId) -> Result<Commit> {
    let object = git.find_commit(sha)?;
    let commit = object.decode()?;
    let (author, committer) = (commit.author()?, commit.committer()?);
    let (authored, committed) = (author.time()?, committer.time()?);
    let person = |role: &[u8], name: &BStr, email: &BStr| Person {
        role: text(role),
        name: text(name),
        email: text(email),
    };
    let mut people = vec![
        person(b"author", author.name, author.email),
        person(b"committer", committer.name, committer.email),
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
            people.push(person(&role, name, email));
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
        subject: text(&commit.message().summary()),
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

/// One statement, so a commit, its people and its repository go in together.
async fn insert(db: &PgPool, repo: i64, commit: Commit) -> Result {
    let people = &commit.people;
    sqlx::query(
        "WITH commit_row AS (
             INSERT INTO commit (sha, parents, author_time, author_tz, commit_time, commit_tz, subject)
             VALUES ($1, $2::sha[], to_timestamp($3::int8), $4, to_timestamp($5::int8), $6, $7)
             ON CONFLICT DO NOTHING
         ), person_rows AS (
             INSERT INTO commit_person (commit, role, position, name, email)
             SELECT $1, role, row_number() OVER (PARTITION BY role ORDER BY n) - 1, name, email
             FROM unnest($8::text[], $9::text[], $10::text[]) WITH ORDINALITY AS p (role, name, email, n)
             ON CONFLICT DO NOTHING
         )
         INSERT INTO repo_commit VALUES ($11, $1)",
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
    .bind(repo)
    .execute(db)
    .await?;
    Ok(())
}

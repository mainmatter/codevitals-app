//! What `migrations/` promises. Each test gets a fresh database, migrated from empty.

use sqlx::{AssertSqlSafe, PgPool, Row};

/// Object id `n`, as a SQL literal: the byte `n`, 20 times. `get_byte(oid, 0)` reads `n` back.
fn oid(n: u8) -> String {
    format!("'\\x{}'", format!("{n:02x}").repeat(20))
}

/// Runs SQL built from literals only.
async fn run(db: &PgPool, sql: String) {
    sqlx::raw_sql(AssertSqlSafe(sql)).execute(db).await.unwrap();
}

async fn column(db: &PgPool, sql: String) -> Vec<String> {
    let rows = sqlx::query(AssertSqlSafe(sql)).fetch_all(db).await.unwrap();
    rows.iter().map(|row| row.get(0)).collect()
}

async fn commit(db: &PgPool, n: u8, parents: &[u8]) {
    let parents: Vec<String> = parents.iter().map(|&p| oid(p)).collect();
    run(
        db,
        format!(
            "INSERT INTO commit VALUES ({}, ARRAY[{}]::sha[], now(), 0, now(), 0, '')",
            oid(n),
            parents.join(", "),
        ),
    )
    .await;
}

/// `main` at A, B, C: A adds `a`, `b` and submodule `s`, B rewrites `a` and bumps `s`, C deletes
/// `b`.
#[sqlx::test]
async fn tree_is_what_a_branch_held_at_each_commit(db: PgPool) {
    let [a, b, c] = [0xa, 0xb, 0xc];
    commit(&db, a, &[]).await;
    commit(&db, b, &[a]).await;
    commit(&db, c, &[b]).await;
    let [a, b, c] = [oid(a), oid(b), oid(c)];
    let [a1, a2, b1, s1, s2] = [oid(1), oid(2), oid(3), oid(4), oid(5)];
    run(
        &db,
        format!(
            "INSERT INTO repo VALUES (1, 'o/r');
             INSERT INTO mainline VALUES (1, 'main', 0, {a}), (1, 'main', 1, {b}), (1, 'main', 2, {c});
             INSERT INTO change VALUES
                 (NULL, {a}, 'a', NULL, NULL, {a1}, 'file', 1, 0),
                 (NULL, {a}, 'b', NULL, NULL, {b1}, 'executable', 1, 0),
                 (NULL, {a}, 's', NULL, NULL, {s1}, 'submodule', NULL, NULL),
                 ({a}, {b}, 'a', {a1}, 'file', {a2}, 'file', 1, 1),
                 ({a}, {b}, 's', {s1}, 'submodule', {s2}, 'submodule', NULL, NULL),
                 ({b}, {c}, 'b', {b1}, 'executable', NULL, NULL, 0, 1);"
        ),
    )
    .await;

    let at = |commit: &str| {
        column(
            &db,
            format!(
                "SELECT concat_ws(' ', path, mode, get_byte(oid, 0))
                 FROM tree JOIN mainline USING (repo, branch)
                 WHERE mainline.commit = {commit} AND tree.seqs @> mainline.seq ORDER BY path"
            ),
        )
    };
    assert_eq!(
        at(&a).await,
        ["a file 1", "b executable 3", "s submodule 4"]
    );
    assert_eq!(
        at(&b).await,
        ["a file 2", "b executable 3", "s submodule 5"]
    );
    assert_eq!(at(&c).await, ["a file 2", "s submodule 5"]);
}

#[sqlx::test]
async fn person_is_an_account_where_one_can_be_told(db: PgPool) {
    for n in 0..5 {
        commit(&db, n, &[]).await;
    }
    run(
        &db,
        format!(
            "INSERT INTO commit_person VALUES
                 -- Known to GitHub.
                 ({0}, 'author', 0, 'Ada', 'ada@example.com', 1, 'ada'),
                 -- The same email in other case, recorded before Ada verified it: Ada's account.
                 ({1}, 'author', 1, 'A. L.', 'ADA@example.com', NULL, NULL),
                 -- An email seen with two accounts: not merged.
                 ({2}, 'author', 0, 'Bot', 'bot@example.com', 2, 'bot-a'),
                 ({3}, 'author', 0, 'Bot', 'bot@example.com', 3, 'bot-b'),
                 ({4}, 'author', 0, 'Bot', 'Bot@example.com', NULL, NULL),
                 -- Unknown anywhere. Sharing a known person's name does not matter.
                 ({4}, 'committer', 0, 'Ada', 'Other@example.com', NULL, NULL);",
            oid(0),
            oid(1),
            oid(2),
            oid(3),
            oid(4),
        ),
    )
    .await;

    let persons = column(
        &db,
        "SELECT person FROM commit_person_key ORDER BY commit, role".into(),
    )
    .await;
    assert_eq!(
        persons,
        [
            "account:1",
            "account:1",
            "account:2",
            "account:3",
            "email:bot@example.com",
            "email:other@example.com",
        ]
    );
}

//! What the app keeps about repositories: the two functions `sync` calls.
//!
//! [`sync`] runs every step, in order, each inserting the rows it finds missing:
//!
//! 1. The `repo` row.
//! 2. The mirror, fetched.
//! 3. Pull requests.
//! 4. Commits and people the tracked branches and pull request heads reach.
//! 5. The GitHub account of each person on them.
//! 6. Each pull request head's merge-base, commits and diff.
//! 7. Each tracked branch's changes and first-parent history.
//! 8. Facts about every blob either side of those changes.
//!
//! No step keeps progress, so a first sync, a later one and one after an interrupted run are the
//! same code. [`forget`] deletes the repository's rows and the content only it reaches.

use std::{iter, time::Instant};

use gix::ObjectId;
use octocrab::{Octocrab, models::RepositoryId, secrecy::SecretString};
use sqlx::PgPool;
use tracing::{Instrument, info, info_span};

use crate::{
    App, Result, accounts, commits, facts, mainline,
    mirror::{self, Mirror, Refs},
    pull_heads, pulls,
};

/// Read from GitHub at the start of each sync.
pub struct Context {
    pub repository: RepositoryId,
    pub full_name: String,
    /// To fetch from.
    pub url: String,
    /// Tracked, as are the branches configured for the repository.
    pub default_branch: String,
    /// Acting as the installation that sees the repository.
    pub github: Octocrab,
    /// An installation access token, for git over HTTPS.
    pub token: SecretString,
}

/// Must be idempotent. Never runs twice at once for one repository. Logs how many items each step
/// recorded.
pub async fn sync(app: &App, context: &Context) -> Result {
    let started = Instant::now();
    let (db, github, full_name) = (&app.db, &context.github, &context.full_name);
    let id = context.repository.0 as i64;
    let mirror = Mirror::new(&app.config.mirrors, context.repository);

    step("repo", upsert(db, id, full_name)).await?;
    let fetch = mirror.update(context.url.clone(), context.token.clone());
    let refs = step("fetch", fetch).await?;
    let pulls = step("pulls", pulls::record(github, db, id, full_name)).await?;
    let branches = tracked_branches(app, context, &refs)?;
    let tips = (branches.iter().map(|(_, tip)| *tip)).chain(mirror::pull_heads(&refs));
    let commits = step("commits", commits::record(db, &mirror, id, tips.collect())).await?;
    let accounts = step("accounts", accounts::record(github, db, id, full_name)).await?;
    let pull_heads = step("pull_heads", pull_heads::record(db, &mirror, id, &refs)).await?;
    let mainline = step("mainline", mainline::record(db, &mirror, id, &branches)).await?;
    let facts = step("facts", facts::record(db, &mirror, id)).await?;

    let duration_ms = started.elapsed().as_millis() as u64;
    info!(
        duration_ms,
        pulls, commits, accounts, pull_heads, mainline, facts, "synced"
    );
    Ok(())
}

/// In a span named after the step, which records how long it took; an error names the step.
async fn step<T>(name: &'static str, work: impl Future<Output = Result<T>>) -> Result<T> {
    let done = work.instrument(info_span!("step", name)).await;
    done.map_err(|error| format!("{name}: {error}").into())
}

/// Moves `full_name` with a rename or transfer.
async fn upsert(db: &PgPool, id: i64, full_name: &str) -> Result {
    sqlx::query("INSERT INTO repo VALUES ($1, $2) ON CONFLICT (id) DO UPDATE SET full_name = $2")
        .bind(id)
        .bind(full_name)
        .execute(db)
        .await?;
    Ok(())
}

/// Names and tips. A configured branch may not exist yet, or any more.
fn tracked_branches(app: &App, context: &Context, refs: &Refs) -> Result<Vec<(String, ObjectId)>> {
    let configured = (app.config.tracked_branches.iter())
        .filter(|(repository, _)| *repository == context.repository)
        .map(|(_, branch)| branch);
    let mut branches = Vec::new();
    for name in iter::once(&context.default_branch).chain(configured) {
        if let Some(tip) = mirror::branch(refs, name)? {
            branches.push((name.clone(), tip));
        }
    }
    Ok(branches)
}

/// One statement, so a reader never sees half of it. Content goes with the commits no other
/// repository reaches, as far as nothing else still refers to it; what a sync of another repository
/// inserts meanwhile, its next sync inserts again.
pub async fn forget(app: &App, repository: RepositoryId) -> Result {
    sqlx::query(
        "WITH gone AS (
             SELECT commit FROM repo_commit r
             WHERE repo = $1
                 AND NOT EXISTS (SELECT FROM repo_commit o WHERE o.commit = r.commit AND o.repo <> $1)
         ), repo_row AS (
             DELETE FROM repo WHERE id = $1
         ), reached AS (
             DELETE FROM repo_commit WHERE repo = $1
         ), changes AS (
             DELETE FROM change WHERE head IN (SELECT commit FROM gone) RETURNING old_oid, new_oid
         ), people AS (
             DELETE FROM commit_person WHERE commit IN (SELECT commit FROM gone)
             RETURNING lower(email) AS email
         ), commits AS (
             DELETE FROM commit WHERE sha IN (SELECT commit FROM gone)
         ), blobs AS (
             DELETE FROM blob_fact WHERE blob IN (
                 SELECT old_oid FROM changes UNION SELECT new_oid FROM changes
                 EXCEPT
                 SELECT s.oid FROM change c
                 CROSS JOIN LATERAL (VALUES (c.old_oid), (c.new_oid)) AS s (oid)
                 WHERE NOT EXISTS (SELECT FROM gone WHERE gone.commit = c.head)
             )
         )
         DELETE FROM email_account WHERE email IN (
             SELECT email FROM people
             EXCEPT
             SELECT lower(p.email) FROM commit_person p
             WHERE NOT EXISTS (SELECT FROM gone WHERE gone.commit = p.commit)
         )",
    )
    .bind(repository.0 as i64)
    .execute(&app.db)
    .await?;
    Mirror::new(&app.config.mirrors, repository)
        .remove()
        .await?;
    info!("forgot");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        path::{Path, PathBuf},
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use axum::http::StatusCode;
    use serde_json::{Value, json};
    use sqlx::{
        PgConnection,
        postgres::{PgConnectOptions, PgPoolOptions},
    };

    use super::*;
    use crate::{
        Config,
        fixtures::{self, commit, git, github, rows, scratch, submodule, write},
        limits,
    };

    // # What a sync records is what git holds

    #[sqlx::test]
    async fn a_sync_records_what_git_holds(db: PgPool) {
        let (root, [first, later]) = scenario("sync");
        let app = app(db, &root.join("mirrors"));

        sync(&app, &first.context(1)).await.unwrap();
        check(&app, &first).await;
        let people = "SELECT format('%s %s: %s %s %s <%s> %s', subject, author_tz, role, position,
                name, email, person)
            FROM commit JOIN commit_person_key ON commit = sha WHERE subject LIKE 'Teach%'";
        assert_eq!(
            rows(&app.db, people).await,
            [
                "Teach the parser trailers 330: author 0 A <a@example.com> account:1",
                "Teach the parser trailers 330: co-authored-by 0 Bea <bea@example.com> email:bea@example.com",
                "Teach the parser trailers 330: co-authored-by 1 Dan  <DAN@example.com> account:1",
                "Teach the parser trailers 330: committer 0 A <a@example.com> account:1",
                "Teach the parser trailers 330: reviewed-by 0 Eve <eve@example.com> email:eve@example.com",
                "Teach the parser trailers 330: signed-off-by 0 Ada <ada@example.com> email:ada@example.com",
            ]
        );
        let latin1 = "SELECT subject || ': ' || name FROM commit JOIN commit_person ON commit = sha
            WHERE role = 'author' AND name LIKE 'Andr%'";
        assert_eq!(
            rows(&app.db, latin1).await,
            ["caf\u{10FFE9}: Andr\u{10FFE9}"]
        );
        let facts = "SELECT concat_ws(' ', ch.path, f.language, f.status, f.bytes, f.lines,
                f.tokens, f.errors,
                (SELECT count(*) FROM fn_fact n WHERE (n.blob, n.language) = (f.blob, f.language)))
            FROM change ch JOIN commit c ON c.sha = ch.head
            JOIN blob_fact f ON f.blob = ch.new_oid
                AND f.language IS NOT DISTINCT FROM language(ch.path, ch.new_mode)
            WHERE c.subject = 'everything'";
        assert_eq!(
            rows(&app.db, facts).await,
            [
                "a.c c parsed 26 1 10 0 1",
                "a.cpp cpp parsed 26 1 10 0 1",
                "a.h cpp parsed 26 1 10 0 1",
                "a.py python parsed 18 2 6 0 1",
                "a.rs rust parsed 10 1 6 0 1",
                "a.txt unsupported 10 1 0",
                "binary.rs rust binary 4 0",
                "broken.rs rust parsed 53 3 21 1 3",
                "deep.rs rust too_deep 2216 1 0",
                "dir/b.txt unsupported 2 1 0",
                "generated.rs rust generated 34 2 0",
                "large.rs rust too_large 1048577 0",
                "latin1.rs rust unparseable 12 1 0",
                "lfs.rs rust lfs_pointer 63 0",
                "link.rs unsupported 4 1 0",
                "run.sh unsupported 10 1 0",
            ]
        );

        sync(&app, &later.context(1)).await.unwrap();
        check(&app, &later).await;

        let synced = rows(&app.db, &every_row()).await;
        let older = sqlx::query("UPDATE blob_fact SET extractor = extractor - 1");
        older.execute(&app.db).await.unwrap();
        sync(&app, &later.context(1)).await.unwrap();
        assert_eq!(
            rows(&app.db, &every_row()).await,
            synced,
            "a newer extractor replaces facts"
        );
    }

    /// The rows for `state`, as git lists what they describe.
    async fn check(app: &App, state: &State) {
        let (db, github) = (&app.db, &state.github);
        let mirror = Mirror::new(&app.config.mirrors, RepositoryId(1));
        let refs = ["for-each-ref", "--format=%(refname) %(objectname)"];
        assert_eq!(git(&mirror.0, &refs), git(github, &refs));
        let config = fs::read_to_string(mirror.0.join("config")).unwrap();
        assert!(!config.contains(github.to_str().unwrap()), "{config}");

        let (mut mainline, mut trees, mut pairs) = (Vec::new(), Vec::new(), Vec::new());
        for branch in state.branches {
            let line = [
                "rev-list",
                "--first-parent",
                "--reverse",
                "--parents",
                branch,
            ];
            for (seq, ids) in git(github, &line).lines().enumerate() {
                let mut ids = ids.split(' ');
                let commit = ids.next().unwrap();
                mainline.push(format!("{branch} {seq} {commit}"));
                let format =
                    format!("--format={branch} {commit} %(objectmode) %(objectname) %(path)");
                let tree = git(github, &["ls-tree", "-r", &format, commit]);
                trees.extend(tree.lines().map(String::from));
                pairs.push((
                    ids.next().unwrap_or(EMPTY_TREE).to_owned(),
                    commit.to_owned(),
                ));
            }
        }
        let shown = "SELECT concat_ws(' ', branch, seq, encode(commit, 'hex')) FROM mainline";
        assert_eq!(rows(db, shown).await, sorted(mainline));
        let shown = "SELECT concat_ws(' ', branch, encode(commit, 'hex'),
                CASE mode WHEN 'file' THEN '100644' WHEN 'executable' THEN '100755'
                    WHEN 'symlink' THEN '120000' ELSE '160000' END,
                encode(oid, 'hex'), path)
            FROM tree JOIN mainline m USING (repo, branch) WHERE seqs @> m.seq";
        assert_eq!(rows(db, shown).await, sorted(trees));

        let mut added = Vec::new();
        for head in &state.heads {
            let [number, head, merge_base] =
                <[&str; 3]>::try_from(Vec::from_iter(head.split(' '))).unwrap();
            let range = format!("{merge_base}..{head}");
            let listed = git(github, &["rev-list", "--topo-order", "--reverse", &range]);
            let listed = listed.lines().enumerate();
            added.extend(listed.map(|(i, commit)| format!("{number} {head} {i} {commit}")));
            pairs.push((merge_base.to_owned(), head.to_owned()));
        }
        let shown = "SELECT concat_ws(' ', number, encode(head, 'hex'), encode(merge_base, 'hex'))
            FROM pull_head";
        assert_eq!(rows(db, shown).await, sorted(state.heads.clone()));
        let shown = "SELECT concat_ws(' ', number, encode(head, 'hex'), position,
            encode(commit, 'hex')) FROM pull_commit";
        assert_eq!(rows(db, shown).await, sorted(added));

        // Submodules aside: git counts their `Subproject commit` lines.
        let mut numstat = Vec::new();
        for (base, head) in &pairs {
            let files = git(github, &["diff", "--numstat", base, head]);
            let files = files.lines().filter(|file| !file.ends_with("\tsub"));
            numstat.extend(files.map(|file| format!("{base} {head}\t{file}")));
        }
        let base = format!("coalesce(encode(base, 'hex'), '{EMPTY_TREE}')");
        let pairs = Vec::from_iter(pairs.iter().map(|(b, h)| format!("('{b}', '{h}')")));
        let shown = format!(
            "SELECT concat_ws(E'\\t', {base} || ' ' || encode(head, 'hex'),
                coalesce(lines_added::text, '-'), coalesce(lines_deleted::text, '-'), path)
            FROM change
            WHERE ({base}, encode(head, 'hex')) IN ({})
                AND 'submodule' NOT IN (coalesce(old_mode, ''), coalesce(new_mode, ''))",
            pairs.join(", ")
        );
        assert_eq!(rows(db, &shown).await, sorted(numstat));

        let without_facts = "SELECT ch.path FROM change ch
            CROSS JOIN LATERAL (VALUES (ch.old_oid, ch.old_mode), (ch.new_oid, ch.new_mode))
                AS s (oid, mode)
            WHERE s.mode <> 'submodule' AND NOT EXISTS (SELECT FROM blob_fact f
                WHERE f.blob = s.oid AND f.language IS NOT DISTINCT FROM language(ch.path, s.mode))";
        assert_eq!(rows(db, without_facts).await, Vec::<String>::new());
    }

    /// git's empty tree, the base of a root commit.
    const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

    /// Sorted, once each: rows that should exist.
    fn sorted(mut lines: Vec<String>) -> Vec<String> {
        lines.sort();
        lines.dedup();
        lines
    }

    // # An interrupted sync is finished by the next, and forget leaves nothing

    /// Each sync of the scenario, interrupted in turn at every point it reaches Postgres, then run
    /// again, ends with the rows of an uninterrupted one. Then two repositories with the same
    /// commits are forgotten one by one.
    #[sqlx::test]
    async fn interrupted_syncs_finish_and_forget_leaves_nothing(
        _: PgPoolOptions,
        connect: PgConnectOptions,
    ) {
        let (root, [first, later]) = scenario("kill");
        let mirrors = root.join("mirrors");
        let (db, handed_out) = interrupted_at(&connect, 0);
        let app = app(db, &mirrors);
        let (first, later, fork) = (first.context(1), later.context(1), later.context(2));
        every_kill_point(&app, &handed_out, &connect, None, &first).await;
        every_kill_point(&app, &handed_out, &connect, Some(&first), &later).await;

        reset(&app).await;
        sync(&app, &fork).await.unwrap();
        let only_fork = rows(&app.db, &every_row()).await;
        reset(&app).await;
        sync(&app, &later).await.unwrap();
        sync(&app, &fork).await.unwrap();
        forget(&app, RepositoryId(1)).await.unwrap();
        assert_eq!(rows(&app.db, &every_row()).await, only_fork);
        forget(&app, RepositoryId(2)).await.unwrap();
        assert_eq!(rows(&app.db, &every_row()).await, Vec::<String>::new());
        assert_eq!(mirrors.read_dir().unwrap().count(), 0);
    }
    /// Syncs `context` uninterrupted after `earlier`. Then for every point that sync reaches
    /// Postgres: syncs `earlier` again, `context` interrupted there and once more uninterrupted,
    /// and compares.
    async fn every_kill_point(
        app: &App,
        handed_out: &AtomicUsize,
        connect: &PgConnectOptions,
        earlier: Option<&Context>,
        context: &Context,
    ) {
        let start = async || {
            reset(app).await;
            if let Some(earlier) = earlier {
                sync(app, earlier).await.unwrap();
            }
        };
        start().await;
        handed_out.store(0, Ordering::SeqCst);
        sync(app, context).await.unwrap();
        let points = handed_out.load(Ordering::SeqCst);
        let uninterrupted = rows(&app.db, &every_row()).await;

        let mut failed = 0;
        for point in 1..=points {
            start().await;
            let interrupted = self::app(interrupted_at(connect, point).0, &app.config.mirrors);
            failed += usize::from(sync(&interrupted, context).await.is_err());
            sync(app, context).await.unwrap();
            let rows = rows(&app.db, &every_row()).await;
            assert_eq!(rows, uninterrupted, "interrupted at {point} of {points}");
        }
        assert!(
            points > 10 && failed > 0,
            "{failed} of {points} interrupted"
        );
    }

    const TABLES: &str = "repo, commit, repo_commit, commit_person, change, mainline, pull, \
        pull_head, pull_commit, email_account, blob_fact, fn_fact";

    /// Every row, but when a pull request head was first seen.
    fn every_row() -> String {
        let row = |table| {
            format!("SELECT '{table} ' || (to_jsonb(t) - 'first_seen')::text FROM {table} t")
        };
        Vec::from_iter(TABLES.split(", ").map(row)).join(" UNION ALL ")
    }

    /// Mirrors stay: an interruption leaves them as they were.
    async fn reset(app: &App) {
        let truncate = format!("TRUNCATE {TABLES}");
        sqlx::query(sqlx::AssertSqlSafe(truncate))
            .execute(&app.db)
            .await
            .unwrap();
    }

    /// One connection, which turns read-only the `point`th time it is handed out, counting from 1:
    /// Postgres then holds what a kill there would have left. Also returns the count so far.
    fn interrupted_at(connect: &PgConnectOptions, point: usize) -> (PgPool, Arc<AtomicUsize>) {
        let handed_out = Arc::new(AtomicUsize::new(0));
        let (connected, acquired) = (handed_out.clone(), handed_out.clone());
        let interrupts =
            move |count: &AtomicUsize| count.fetch_add(1, Ordering::SeqCst) + 1 == point;
        let db = PgPoolOptions::new()
            .max_connections(1)
            .after_connect(move |connection, _| hand_out(connection, interrupts(&connected)))
            .before_acquire(move |connection, _| {
                let handed_out = hand_out(connection, interrupts(&acquired));
                Box::pin(async move { handed_out.await.map(|()| true) })
            })
            .connect_lazy_with(connect.clone());
        (db, handed_out)
    }

    type HandOut<'c> = Pin<Box<dyn Future<Output = sqlx::Result<()>> + Send + 'c>>;

    fn hand_out(connection: &mut PgConnection, interrupt: bool) -> HandOut<'_> {
        Box::pin(async move {
            if interrupt {
                let read_only = sqlx::query("SET default_transaction_read_only = on");
                read_only.execute(connection).await?;
            }
            Ok(())
        })
    }

    /// Tracking `release` besides the default branch.
    fn app(db: PgPool, mirrors: &Path) -> Arc<App> {
        let github = github(async |_| (StatusCode::NOT_FOUND, Value::Null));
        let release = |repository| (RepositoryId(repository), "release".into());
        let config = Config {
            mirrors: mirrors.into(),
            tracked_branches: vec![release(1), release(2)],
            ..fixtures::config()
        };
        App::new(config, github.clone(), github, db)
    }

    // # The scenario

    /// GitHub at one time: its repository, its pull requests, the tracked branches it has and the
    /// `pull_head` rows syncing up to it leaves, `number head merge_base`.
    struct State {
        github: PathBuf,
        pulls: Vec<Value>,
        branches: &'static [&'static str],
        heads: Vec<String>,
    }

    impl State {
        /// As repository `id`, on a GitHub that links `a@example.com` and `dan@example.com` to
        /// one account, and no longer has some commit.
        fn context(&self, id: u64) -> Context {
            let pulls = self.pulls.clone();
            let github = github(move |request| {
                let answer = match request.uri().path() {
                    "/graphql" => {
                        let ada = json!({ "email": "a@example.com", "user": { "databaseId": 1, "login": "a" } });
                        let dan = json!({ "email": "DAN@example.com", "user": { "databaseId": 1, "login": "a" } });
                        let commit = json!({
                            "authors": { "totalCount": 2, "nodes": [ada, dan] },
                            "committer": ada,
                        });
                        json!({ "data": { "repository": { "c0": commit, "c1": null } } })
                    }
                    _ => Value::from(pulls.clone()),
                };
                async move { (StatusCode::OK, answer) }
            });
            Context {
                repository: RepositoryId(id),
                full_name: format!("o/r{id}"),
                url: self.github.to_str().unwrap().into(),
                default_branch: "main".into(),
                github,
                token: "".into(),
            }
        }
    }

    /// First, `main` holds every kind of entry and content, a merge of a commit naming people in
    /// trailers and a commit in Latin-1, and `release` is `main`. Pull request 1 is opened from `topic`; 2's base branch is
    /// gone; 3 is unrelated to its base; 5 is listed before its head is pushed.
    ///
    /// Later, `main` is rewritten under its tip, its first parents rerouted through a merge, and
    /// `release` deleted. Pull request 1 merges `main` and is force-pushed; 4 is opened with a head
    /// `main` already has.
    fn scenario(name: &str) -> (PathBuf, [State; 2]) {
        let root = scratch(name);
        mirror::prepare(&root.join("mirrors")).unwrap();
        let (work, github) = (root.join("work"), root.join("github.git"));
        git(&root, &["init", "--bare", "github.git"]);
        git(&root, &["init", "--initial-branch=main", "work"]);
        let (mut pulls, mut heads) = (Vec::new(), Vec::new());
        let mode = |path: &str, mode| {
            fs::set_permissions(work.join(path), fs::Permissions::from_mode(mode)).unwrap()
        };

        git(&work, &["commit", "--allow-empty", "-m", "root"]);
        let root_commit = git(&work, &["rev-parse", "HEAD"]);
        let (rust, c) = ("fn f() {}\n", "int f(void) { return 0; }\n");
        let deep = format!("const D: u8 = {}1{};", "(".repeat(1100), ")".repeat(1100));
        let large = "/".repeat(limits::FACT_BYTES as usize + 1);
        let lfs = "version https://git-lfs.github.com/spec/v1\noid sha256:0\nsize 1\n";
        let broken = "fn before() {}\nfn broken() { let = ; }\nfn after() {}\n";
        for (path, content) in [
            ("a.rs", rust),
            ("a.txt", rust),
            ("a.c", c),
            ("a.h", c),
            ("a.cpp", c),
            ("a.py", "def f():\n    pass\n"),
            ("binary.rs", "fn\0\n"),
            ("broken.rs", broken),
            ("generated.rs", "// @generated by a tool\nfn x() {}\n"),
            ("lfs.rs", lfs),
            ("deep.rs", &deep),
            ("large.rs", &large),
            ("dir/b.txt", "b\n"),
            ("run.sh", "#!/bin/sh\n"),
        ] {
            write(&work, path, content);
        }
        mode("run.sh", 0o755);
        fs::write(work.join("latin1.rs"), b"fn caf\xe9() {}").unwrap();
        symlink("a.rs", work.join("link.rs")).unwrap();
        submodule(&work, &root_commit);
        commit(&work, "everything");
        git(&work, &["switch", "-c", "side"]);
        git(&work, &["commit", "--allow-empty", "-m", TRAILERS]);
        git(&work, &["switch", "main"]);
        git(&work, &["merge", "--no-ff", "-m", "merge side", "side"]);
        // Written as an older tool would: `git commit` converts Latin-1 to UTF-8.
        let (tree, parent) = (
            git(&work, &["rev-parse", "HEAD^{tree}"]),
            git(&work, &["rev-parse", "HEAD"]),
        );
        let header = format!("tree {tree}\nparent {parent}\n");
        let person = |role: &str| {
            [
                role.as_bytes(),
                b" Andr\xe9 <a@example.com> 1577860200 +0530\n",
            ]
            .concat()
        };
        let object = [
            header.as_bytes(),
            &person("author"),
            &person("committer"),
            b"\ncaf\xe9\n",
        ]
        .concat();
        let mut hash = std::process::Command::new("git")
            .current_dir(&work)
            .args([
                "hash-object",
                "-t",
                "commit",
                "-w",
                "--literally",
                "--stdin",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(&mut hash.stdin.take().unwrap(), &object).unwrap();
        let latin1 = String::from_utf8(hash.wait_with_output().unwrap().stdout).unwrap();
        git(&work, &["update-ref", "refs/heads/main", latin1.trim()]);
        git(&work, &["branch", "release"]);
        git(&work, &["switch", "-c", "topic"]);
        write(&work, "t.py", "def t():\n    pass\n");
        commit(&work, "add t");
        git(&work, &["switch", "--orphan", "unrelated"]);
        write(&work, "u.py", "def u():\n    pass\n");
        commit(&work, "unrelated");
        git(&work, &["switch", "-c", "late", "main"]);
        write(&work, "late.rs", "fn late() {}\n");
        commit(&work, "late");
        git(&work, &["switch", "main"]);
        open(&work, &mut pulls, &mut heads, 1, "main", "topic");
        open(&work, &mut pulls, &mut heads, 2, "gone", "topic");
        open(&work, &mut pulls, &mut heads, 3, "main", "unrelated");
        list(&work, &mut pulls, 5, "main", "late");
        push(&work, &["main", "release", "topic"]);
        git(&root, &["clone", "--mirror", "github.git", "first.git"]);
        let first = State {
            github: root.join("first.git"),
            pulls: pulls.clone(),
            branches: &["main", "release"],
            heads: heads.clone(),
        };

        git(&work, &["reset", "--hard", "HEAD~"]);
        write(&work, "a.txt", "fn f() { 1 }\n");
        fs::remove_dir_all(work.join("dir")).unwrap();
        write(&work, "dir", "now a file\n");
        mode("run.sh", 0o644);
        write(&work, "binary.rs", "fn\0\x03\n");
        write(&work, "a.rs", "fn edited() {}\n");
        submodule(&work, &git(&work, &["rev-parse", "HEAD"]));
        commit(&work, "rewrite");
        git(&work, &["commit", "--allow-empty", "-m", "nothing"]);
        git(&work, &["switch", "-c", "reroute", "HEAD~"]);
        write(&work, "r.txt", "r\n");
        commit(&work, "reroute");
        git(&work, &["merge", "--no-ff", "-m", "merge main", "main"]);
        git(&work, &["switch", "main"]);
        git(&work, &["merge", "--ff-only", "reroute"]);
        git(&work, &["switch", "topic"]);
        git(&work, &["merge", "--no-ff", "-m", "merge main", "main"]);
        write(&work, "t.py", "def t():\n    return 1\n");
        git(
            &work,
            &["commit", "--all", "--amend", "-m", "merge main, amended"],
        );
        git(&work, &["switch", "main"]);
        open(&work, &mut pulls, &mut heads, 1, "main", "topic");
        open(&work, &mut pulls, &mut heads, 4, "main", "main");
        push(&work, &["main", "topic", ":release"]);
        let later = State {
            github,
            pulls,
            branches: &["main"],
            heads,
        };
        (root, [first, later])
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

    /// Lists pull request `number` from `branch` into `base`, or updates it, as GitHub does: newest
    /// first.
    fn list(work: &Path, pulls: &mut Vec<Value>, number: usize, base: &str, branch: &str) {
        let head = git(work, &["rev-parse", branch]);
        pulls.retain(|listed| listed["number"] != number);
        static MINUTES: AtomicUsize = AtomicUsize::new(1);
        let minute = MINUTES.fetch_add(1, Ordering::SeqCst);
        let updated = format!("2020-01-01T{:02}:{:02}:00Z", minute / 60, minute % 60);
        pulls.insert(
            0,
            json!({
                "number": number,
                "state": "open",
                "draft": false,
                "base": { "ref": base },
                "head": { "sha": head },
                "created_at": "2020-01-01T00:00:00Z",
                "updated_at": updated,
                "merged_at": null,
                "closed_at": null,
                "merge_commit_sha": null,
                "user": { "id": 1, "login": "ada", "type": "User" },
            }),
        );
    }

    /// Lists pull request `number` and pushes its head. Expects its row where its base branch
    /// exists and shares history with it.
    fn open(
        work: &Path,
        pulls: &mut Vec<Value>,
        heads: &mut Vec<String>,
        number: usize,
        base: &str,
        branch: &str,
    ) {
        list(work, pulls, number, base, branch);
        push(work, &[&format!("{branch}:refs/pull/{number}/head")]);
        let head = git(work, &["rev-parse", branch]);
        let merge_base = std::process::Command::new("git")
            .current_dir(work)
            .args(["merge-base", base, &head])
            .output()
            .unwrap();
        if merge_base.status.success() {
            let merge_base = String::from_utf8(merge_base.stdout).unwrap();
            heads.push(format!("{number} {head} {}", merge_base.trim()));
        }
    }

    /// Forced; `:branch` deletes.
    fn push(work: &Path, refspecs: &[&str]) {
        for refspec in refspecs {
            let refspec = match refspec.split_once(':') {
                Some(_) => format!("+{refspec}"),
                None => format!("+{refspec}:{refspec}"),
            };
            git(work, &["push", "../github.git", &refspec]);
        }
    }
}

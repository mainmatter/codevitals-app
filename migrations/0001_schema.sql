-- What `sync` derives from a repository's mirror and from GitHub.
--
-- Content (commits, the people on them, diffs) is keyed by content and shared by every repository
-- and branch that reaches it. What is per repository (branches) refers to content and goes with
-- its `repo` row. A row is inserted whole and never
-- updated, except `repo.full_name`.

-- Object ids, raw. gix hashes with SHA-1.
CREATE DOMAIN sha AS bytea CHECK (octet_length(VALUE) = 20);

-- # Repositories

CREATE TABLE repo (
    -- GitHub's id, which survives renames and transfers.
    id bigint PRIMARY KEY,
    -- As of the latest sync.
    full_name text NOT NULL
);

-- # Commits

CREATE TABLE commit (
    sha sha PRIMARY KEY,
    -- In order; the first is the first parent.
    parents sha[] NOT NULL,
    author_time timestamptz NOT NULL,
    -- Minutes east of UTC, as the author's clock read.
    author_tz int NOT NULL,
    commit_time timestamptz NOT NULL,
    commit_tz int NOT NULL,
    subject text NOT NULL
);

-- Everyone on a commit, inserted with it: the one `author` and one `committer`, at 0, and each
-- trailer whose key ends in `-by`, the key in lower case as `role`, numbered in order per role.
-- Names and emails are as git wrote them.
CREATE TABLE commit_person (
    commit sha NOT NULL REFERENCES commit,
    role text NOT NULL CHECK (role IN ('author', 'committer') OR role LIKE '%-by'),
    position int NOT NULL CHECK (position >= 0 AND (role LIKE '%-by' OR position = 0)),
    name text NOT NULL,
    email text NOT NULL,
    -- The GitHub user this email belonged to when the commit was recorded, if any.
    account_id bigint,
    account_login text,
    PRIMARY KEY (commit, role, position),
    CHECK (num_nulls(account_id, account_login) IN (0, 2))
);

-- # Diffs

-- Git's file modes, 100644, 100755, 120000 and 160000. A submodule's oid is a commit in another
-- repository, absent from this one's mirror.
CREATE DOMAIN mode AS text CHECK (VALUE IN ('file', 'executable', 'symlink', 'submodule'));

-- One row per path that differs between two commits: a commit's first parent and the commit, or a
-- pull request's merge-base and head. A root commit is diffed against nothing, `base` null. A
-- pair's rows are inserted together; two commits with equal trees have none.
CREATE TABLE change (
    base sha REFERENCES commit,
    head sha NOT NULL REFERENCES commit,
    path text NOT NULL,
    -- Null where the path is absent on that side.
    old_oid sha,
    old_mode mode,
    new_oid sha,
    new_mode mode,
    -- Null for binary content and submodules.
    lines_added int CHECK (lines_added >= 0),
    lines_deleted int CHECK (lines_deleted >= 0),
    UNIQUE NULLS NOT DISTINCT (head, base, path),
    CHECK (num_nulls(old_oid, new_oid) < 2),
    CHECK (num_nulls(old_oid, old_mode) IN (0, 2)),
    CHECK (num_nulls(new_oid, new_mode) IN (0, 2)),
    CHECK (num_nulls(lines_added, lines_deleted) IN (0, 2))
);

-- # Branches

-- A tracked branch's first-parent history: `seq` counts from its root commit, 0. Rows only ever
-- extend it, so sync fails on a force-push: on a row that should exist but conflicts, or on a row
-- that exists but should not, as after a rewind.
CREATE TABLE mainline (
    repo bigint NOT NULL REFERENCES repo ON DELETE CASCADE,
    branch text NOT NULL,
    seq int NOT NULL CHECK (seq >= 0),
    commit sha NOT NULL REFERENCES commit,
    PRIMARY KEY (repo, branch, seq),
    UNIQUE (repo, branch, commit)
);

-- # The tree

-- Each path's content on a branch, and the `seq`s it was current for: from the commit that wrote it
-- up to, not including, the next that changed it. Built from first-parent diffs, so it is
-- complete only where `change` is.
--
-- What was in the tree at commit X:
--
--     SELECT path, oid, mode FROM tree JOIN mainline USING (repo, branch)
--     WHERE mainline.commit = X AND tree.seqs @> mainline.seq
CREATE VIEW tree AS
WITH written AS (
    SELECT
        m.repo,
        m.branch,
        m.seq,
        ch.path,
        ch.new_oid,
        ch.new_mode,
        lead(m.seq) OVER (PARTITION BY m.repo, m.branch, ch.path ORDER BY m.seq) AS replaced
    FROM mainline m
    JOIN commit c ON c.sha = m.commit
    JOIN change ch ON ch.head = m.commit AND ch.base IS NOT DISTINCT FROM c.parents[1]
)
SELECT repo, branch, path, new_oid AS oid, new_mode AS mode, int4range(seq, replaced) AS seqs
FROM written
WHERE new_oid IS NOT NULL;

-- # People

-- Each `commit_person` row with `person`, who they are: their GitHub account where known;
-- otherwise the one account their email belonged to on any other commit; otherwise their email,
-- lowercased. Rows keep what GitHub knew when they were recorded, so this carries an email
-- verified later back to older commits. Names are never matched, and an email seen with two
-- accounts stays unmerged.
CREATE VIEW commit_person_key AS
WITH email_account AS (
    SELECT lower(email) AS email, min(account_id) AS account_id
    FROM commit_person
    WHERE account_id IS NOT NULL
    GROUP BY lower(email)
    HAVING count(DISTINCT account_id) = 1
)
SELECT
    p.*,
    coalesce('account:' || coalesce(p.account_id, e.account_id), 'email:' || lower(p.email))
        AS person
FROM commit_person p
LEFT JOIN email_account e ON e.email = lower(p.email);

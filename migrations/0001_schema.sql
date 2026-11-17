-- What `sync` derives from a repository's mirror and from GitHub.
--
-- Content (commits, the people on them, diffs, facts, the accounts of their emails) is keyed by
-- content and shared by every repository and branch that reaches it. What is per repository
-- (branches, pull requests) refers to content and goes with its `repo` row. A row is inserted whole
-- and never updated, except `repo` and a `pull` whose update time moves; `mainline` rows are
-- deleted and inserted, as are a blob's facts when the extractor changes.

-- Object ids, raw. gix hashes with SHA-1.
CREATE DOMAIN sha AS bytea CHECK (octet_length(VALUE) = 20);

-- # Repositories

CREATE TABLE repo (
    -- GitHub's id, which survives renames and transfers.
    id bigint PRIMARY KEY,
    -- As of the latest sync.
    full_name text NOT NULL,
    -- How far `pull` is read, in GitHub's list by update time, newest first: every pull request
    -- updated before `pulls_since` has a current row, except those from page `pulls_page` on,
    -- which a first sync has yet to read. Null before the first.
    pulls_since timestamptz,
    pulls_page int
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
    PRIMARY KEY (commit, role, position)
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

-- A tracked branch's first-parent history: `seq` counts from its root commit, 0. Replaced whole, in
-- one transaction, whenever the tip moves, so a reader sees one history, never a mixture. Each
-- commit's diff against its first parent is in `change` first.
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

-- # Pull requests

-- Each pull request, whatever its state, as GitHub described it at `updated`. Its commits need not
-- have rows.
CREATE TABLE pull (
    repo bigint NOT NULL REFERENCES repo ON DELETE CASCADE,
    number int NOT NULL CHECK (number >= 1),
    state text NOT NULL CHECK (state IN ('open', 'closed')),
    draft bool NOT NULL,
    base_ref text NOT NULL,
    head sha NOT NULL,
    created timestamptz NOT NULL,
    updated timestamptz NOT NULL,
    merged timestamptz,
    closed timestamptz,
    -- What it merged as; null unless merged.
    merge_commit sha,
    -- GitHub's account; a deleted one is its `ghost`.
    author_id bigint NOT NULL,
    author_login text NOT NULL,
    -- `User`, `Bot`, `Organization`, ...
    author_type text NOT NULL,
    PRIMARY KEY (repo, number)
);

-- # People

-- The GitHub account an email belonged to when GitHub was asked, or null for none. GitHub links
-- commits to accounts by email, so this is the one link from git to a person. Asked about: emails
-- in the roles GitHub attributes, `author`, `committer` and `co-authored-by`.
CREATE TABLE email_account (
    email text PRIMARY KEY CHECK (email = lower(email)),
    account_id bigint,
    account_login text,
    CHECK (num_nulls(account_id, account_login) IN (0, 2))
);

-- Each `commit_person` row with `person`, who they are: their email's account if it has one,
-- otherwise their email, lowercased. Names are never matched.
CREATE VIEW commit_person_key AS
SELECT p.*, coalesce('account:' || a.account_id, 'email:' || lower(p.email)) AS person
FROM commit_person p
LEFT JOIN email_account a ON a.email = lower(p.email);

-- # Facts

-- The language a path's content is read as; null where no extractor applies. Analyses join
-- `change` to `blob_fact` through it. A `.h` header is read as C++.
CREATE FUNCTION language(path text, mode mode) RETURNS text
LANGUAGE sql IMMUTABLE STRICT
RETURN CASE WHEN mode IN ('file', 'executable') THEN CASE
    WHEN path ~ '\.rs$' THEN 'rust'
    WHEN path ~ '\.c$' THEN 'c'
    WHEN path ~ '\.(h|C|H|cc|hh|cpp|hpp|cxx|hxx|c\+\+|h\+\+)$' THEN 'cpp'
    WHEN path ~ '\.pyi?$' THEN 'python'
END END;

-- What version `extractor` found in a blob read as `language`; a later version replaces the row.
-- `status` is the first that applies:
--
--   too_large    larger than the extractor reads; `bytes` only
--   binary       `bytes` only
--   lfs_pointer  a Git LFS pointer; `bytes` only
--   generated    a generator's marker in its first five lines
--   unsupported  no extractor for `language`
--   unparseable  not UTF-8
--   too_deep     its syntax nests too deeply
--   parsed       everything, and a `fn_fact` row per function
CREATE TABLE blob_fact (
    blob sha NOT NULL,
    language text,
    extractor int NOT NULL,
    status text NOT NULL CHECK (status IN (
        'too_large', 'binary', 'lfs_pointer', 'generated', 'unsupported', 'unparseable', 'too_deep',
        'parsed'
    )),
    bytes bigint NOT NULL CHECK (bytes >= 0),
    lines int CHECK (lines >= 0),
    tokens int CHECK (tokens >= 0),
    -- Syntax errors the parser recovered from.
    errors int CHECK (errors >= 0),
    UNIQUE NULLS NOT DISTINCT (blob, language),
    CHECK ((lines IS NULL) = (status IN ('too_large', 'binary', 'lfs_pointer'))),
    CHECK (num_nulls(tokens, errors) = CASE status WHEN 'parsed' THEN 0 ELSE 2 END)
);

-- Defined per language, in `src/facts/<language>.rs`. Names need not be unique.
CREATE TABLE fn_fact (
    blob sha NOT NULL,
    language text NOT NULL,
    name text NOT NULL,
    start_line int NOT NULL CHECK (start_line >= 1),
    end_line int NOT NULL CHECK (end_line >= start_line),
    tokens int NOT NULL CHECK (tokens >= 0),
    parameters int NOT NULL CHECK (parameters >= 0),
    branch_points int NOT NULL CHECK (branch_points >= 0),
    max_nesting int NOT NULL CHECK (max_nesting >= 0),
    -- Equal for functions that differ only in names, literals, comments and layout.
    shape bytea NOT NULL CHECK (octet_length(shape) = 32),
    test bool NOT NULL,
    errors int NOT NULL CHECK (errors >= 0),
    FOREIGN KEY (blob, language) REFERENCES blob_fact (blob, language) ON DELETE CASCADE
);

-- Replacing a blob's facts deletes its functions through this.
CREATE INDEX ON fn_fact (blob, language);

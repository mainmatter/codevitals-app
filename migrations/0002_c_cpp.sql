-- The language a path's content is read as; null where no extractor applies. Analyses join
-- `change` to `blob_fact` through it. A `.h` header is read as C++.
CREATE OR REPLACE FUNCTION language(path text, mode mode) RETURNS text
LANGUAGE sql IMMUTABLE STRICT
RETURN CASE WHEN mode IN ('file', 'executable') THEN CASE
    WHEN path ~ '\.rs$' THEN 'rust'
    WHEN path ~ '\.c$' THEN 'c'
    WHEN path ~ '\.(h|C|H|cc|hh|cpp|hpp|cxx|hxx|c\+\+|h\+\+)$' THEN 'cpp'
END END;

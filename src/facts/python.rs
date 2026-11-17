//! Python, as tree-sitter-python parses it.
//!
//! A function is a `def`, `async def` included, wherever it is, or a lambda. A string is one token,
//! an f-string too: its replacement fields are checked for errors and depth, but lambdas in them
//! are not seen. A docstring is a string.
//!
//! Names qualify by the functions and classes around them, as `__qualname__` does without its
//! `<locals>`: `outer::inner`, `Point::norm`, `make::Local::method`, `run::{lambda#0}`. A
//! decorator is outside what it decorates: a lambda in its arguments belongs to the scope around.
//!
//! - parameters: those declared, `self`, `*args` and `**kwargs` included; the bare `*` and `/`
//!   separators are not parameters.
//! - branch points: `if`, `elif`, `while`, `for`, each `except`, each `case` after the first,
//!   `… if … else …`, `and`, `or`, a comprehension's `for` and `if` clauses, and a case guard.
//! - nesting: `if`, `match`, `while`, `for` and `try`. `elif` is a sibling and does not add nesting.
//! - test: what pytest and unittest collect, and what pytest decorates: a function named `test…`,
//!   a class named `Test…` or deriving from a `…TestCase`, and a definition with a decorator from
//!   `pytest`, such as `@pytest.fixture` or `@pytest.mark.parametrize(…)`.

use tree_sitter::Node;

use super::syntax::{Language, Source, Test};

pub const PYTHON: Language = Language {
    grammar: || tree_sitter_python::LANGUAGE.into(),
    functions: &["function_definition", "lambda"],
    scopes: &["class_definition"],
    #[rustfmt::skip]
    nesting: &[
        "if_statement", "match_statement", "while_statement", "for_statement", "try_statement",
    ],
    unnamed: "lambda",
    name: |source, node| source.field(node, "name"),
    parameters,
    branch_points,
    test,
};

fn parameters(_: &Source, node: Node) -> usize {
    let is_parameter = |parameter: &Node| {
        !matches!(
            parameter.kind(),
            "comment" | "keyword_separator" | "positional_separator"
        )
    };
    node.child_by_field_name("parameters").map_or(0, |list| {
        let mut cursor = list.walk();
        list.named_children(&mut cursor)
            .filter(is_parameter)
            .count()
    })
}

fn branch_points(node: Node) -> i32 {
    match node.kind() {
        "if_statement"
        | "elif_clause"
        | "while_statement"
        | "for_statement"
        | "except_clause"
        | "conditional_expression"
        | "boolean_operator"
        | "for_in_clause"
        | "if_clause" => 1,
        "match_statement" => node.child_by_field_name("body").map_or(0, |cases| {
            let mut cursor = cases.walk();
            let count = cases
                .named_children(&mut cursor)
                .filter(|c| c.kind() == "case_clause");
            (count.count() as i32 - 1).max(0)
        }),
        _ => 0,
    }
}

fn test(source: &Source, node: Node) -> Option<Test> {
    let field = |name| source.field(node, name).unwrap_or_default();
    let marked = match node.kind() {
        "function_definition" => field("name").starts_with("test"),
        "class_definition" => {
            let bases = field("superclasses");
            let mut bases = bases.trim_matches(['(', ')']).split(',');
            field("name").starts_with("Test") || bases.any(|b| b.trim().ends_with("TestCase"))
        }
        "decorator" => {
            return source
                .text(node)
                .starts_with("@pytest.")
                .then_some(Test::Next);
        }
        _ => false,
    };
    marked.then_some(Test::Itself)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::syntax::{parse, summary};

    #[test]
    fn elif_is_flat_but_else_if_nests() {
        for (statement, nesting) in [
            ("if x:\n  pass\nelif x:\n  pass", 1),
            ("if x:\n  pass\nelse:\n  if x:\n    pass", 2),
            ("if x:\n  pass\nelse:\n  for _ in x:\n    pass", 2),
        ] {
            let source = format!("def f(x):\n {}\n", statement.replace('\n', "\n "));
            let code = parse(source.as_bytes(), &PYTHON).unwrap();
            assert_eq!(code.errors, 0, "{source}");
            assert_eq!(code.functions[0].max_nesting, nesting, "{source}");
        }
    }

    #[test]
    fn code_in_f_strings_is_checked_not_counted() {
        let tokens = |source: &str| parse(source.as_bytes(), &PYTHON).unwrap().functions[0].tokens;
        assert_eq!(
            tokens("def f(): return f'{x}'"),
            tokens("def f(): return 'x'")
        );

        let code = parse(b"def f(): return f'{1 + }'", &PYTHON).unwrap();
        assert!(code.errors > 0 && code.functions[0].errors > 0);

        let deep = format!("x = f'{{{}1{}}}'", "(".repeat(1100), ")".repeat(1100));
        assert!(matches!(parse(deep.as_bytes(), &PYTHON), Err("too_deep")));
    }

    #[test]
    fn golden() {
        // `clamp` and `bound` share a shape, as do `make::Local::method` and `memoized`, the two
        // `speed`s, and the lambdas returning `Point(0, 0)`.
        assert_eq!(
            summary(&parse(include_bytes!("python/golden.py"), &PYTHON).unwrap()),
            GOLDEN.trim()
        );
    }

    const GOLDEN: &str = "
tokens 558 errors 0
clamp 9-15 tokens 27 parameters 3 branch points 2 nesting 1 shape c248833c
bound 18-25 tokens 27 parameters 3 branch points 2 nesting 1 shape c248833c
signature 28-29 tokens 33 parameters 7 branch points 0 nesting 0 shape e476ce45
keyword_only 32-33 tokens 11 parameters 2 branch points 0 nesting 0 shape 59d4ec0e
keyword_only::{lambda#0} 33-33 tokens 5 parameters 0 branch points 0 nesting 0 shape cd711881
fetch 36-44 tokens 48 parameters 2 branch points 3 nesting 2 shape b8b17361
describe 47-54 tokens 49 parameters 1 branch points 6 nesting 1 shape 6ce60225
outer 57-65 tokens 32 parameters 1 branch points 2 nesting 0 shape 71307cbe
outer::inner 58-62 tokens 10 parameters 1 branch points 0 nesting 0 shape 00bc5cd4
outer::inner::innermost 59-60 tokens 7 parameters 0 branch points 0 nesting 0 shape efb3f49a
outer::{lambda#0} 65-65 tokens 8 parameters 1 branch points 0 nesting 0 shape 20161a0b
outer::{lambda#0}::{lambda#0} 65-65 tokens 5 parameters 1 branch points 0 nesting 0 shape 7f1af455
Point::{lambda#0} 69-69 tokens 8 parameters 0 branch points 0 nesting 0 shape d93f18cb
Point::__init__ 71-72 tokens 21 parameters 3 branch points 0 nesting 0 shape e7568fe3
Point::norm 75-76 tokens 22 parameters 1 branch points 0 nesting 0 shape aec3fa2a
Point::parse 80-82 tokens 29 parameters 1 branch points 0 nesting 0 shape 9e7d1eca
Point::Builder::build 85-86 tokens 13 parameters 1 branch points 0 nesting 0 shape 507dc5c1
make 89-94 tokens 10 parameters 0 branch points 0 nesting 0 shape 0e282b77
make::Local::method 91-92 tokens 8 parameters 1 branch points 0 nesting 0 shape 229560c4
{lambda#0} 97-97 tokens 3 parameters 0 branch points 0 nesting 0 shape e294620e
memoized 98-99 tokens 8 parameters 1 branch points 0 nesting 0 shape 229560c4
speed 103-103 tokens 7 parameters 0 branch points 0 nesting 0 shape ff64c652
speed 105-105 tokens 7 parameters 0 branch points 0 nesting 0 shape ff64c652
test_clamp 108-109 tokens 26 parameters 0 branch points 1 nesting 0 shape d50f10b7 test
point 113-114 tokens 12 parameters 0 branch points 0 nesting 0 shape 57d839ce test
parses 119-120 tokens 13 parameters 1 branch points 0 nesting 0 shape d8e78101 test
TestPoint::helper 124-125 tokens 7 parameters 1 branch points 0 nesting 0 shape dbbc56f8 test
TestPoint::helper::{lambda#0} 125-125 tokens 8 parameters 0 branch points 0 nesting 0 shape d93f18cb test
PointTests::setUp 129-130 tokens 16 parameters 1 branch points 0 nesting 0 shape cfb4d679 test
";
}

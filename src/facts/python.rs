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

use super::syntax::{Language, Source, Test, count_children};

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
        "match_statement" => node
            .child_by_field_name("body")
            .map_or(0, |cases| (count_children(cases, "case_clause") - 1).max(0)),
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

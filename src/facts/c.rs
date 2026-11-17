//! C and C++, as tree-sitter-c and tree-sitter-cpp parse them, by one set of rules: C's syntax is
//! C++'s, less what C++ adds.
//!
//! The preprocessor is not evaluated: code is parsed as written. Every branch of an `#if` is
//! parsed, so a function defined in two branches is two functions. A macro's body is one token.
//! Where an invocation stands in for syntax, the parse has errors.
//!
//! A function is a definition with a body: `int f(void) { … }`, a method defined in its class or
//! out of it, or a lambda. A prototype, a pure virtual method, `= default` and `= delete` are
//! declarations, not functions.
//!
//! Names are as written, qualified by the namespaces and classes around them: `f`,
//! `geometry::Point::norm`, `geometry::Point::operator==`, `geometry::Point::~Point`,
//! `(anonymous namespace)::helper`, `Box<T>::get`, `main::{lambda#0}`. A test is named as it is
//! invoked: `TEST(Point, Norm)`.
//!
//! - parameters: those declared; `(void)` declares none and `...` is not one.
//! - branch points: `if`, `while`, `do`, `for`, each `case` but not `default`, each `catch`, `?:`,
//!   `&&` and `||`, spelled `and` and `or` too.
//! - nesting: `if`, `switch`, `while`, `do`, `for` and `try`.
//! - test: a body given to a test framework's macro, `TEST(Suite, Name) { … }`, which parses as a
//!   function without a return type named in [`TEST_MACROS`]. C requires a return type, so in C
//!   nothing is a test.

use std::iter;

use tree_sitter::Node;

use super::syntax::{Language, Source, Test};
use crate::limits;

pub const C: Language = Language {
    grammar: || tree_sitter_c::LANGUAGE.into(),
    functions: &["function_definition", "lambda_expression"],
    scopes: &[
        "namespace_definition",
        "class_specifier",
        "struct_specifier",
        "union_specifier",
    ],
    #[rustfmt::skip]
    nesting: &[
        "if_statement", "switch_statement", "while_statement", "do_statement", "for_statement",
        "for_range_loop", "try_statement",
    ],
    unnamed: "lambda",
    name,
    parameters,
    branch_points,
    test,
};

pub const CPP: Language = Language {
    grammar: || tree_sitter_cpp::LANGUAGE.into(),
    ..C
};

/// GoogleTest's, Catch2's, doctest's and Boost.Test's.
#[rustfmt::skip]
const TEST_MACROS: [&str; 12] = [
    "TEST", "TEST_F", "TEST_P", "TYPED_TEST", "TYPED_TEST_P",
    "TEST_CASE", "TEST_CASE_METHOD", "TEMPLATE_TEST_CASE", "SCENARIO",
    "BOOST_AUTO_TEST_CASE", "BOOST_FIXTURE_TEST_CASE", "BOOST_DATA_TEST_CASE",
];

fn name(source: &Source, node: Node) -> Option<String> {
    match node.kind() {
        "function_definition" => match test_macro(source, node) {
            Some(invocation) => Some(source.text(invocation)),
            None => declarators(node).last().map(|name| source.text(name)),
        },
        "namespace_definition" => Some(
            source
                .field(node, "name")
                .unwrap_or("(anonymous namespace)".into()),
        ),
        _ => source.field(node, "name"),
    }
}

fn parameters(source: &Source, node: Node) -> usize {
    let function = declarators(node).filter(|d| d.kind().ends_with("function_declarator"));
    let Some(list) = function
        .last()
        .and_then(|f| f.child_by_field_name("parameters"))
    else {
        return 0;
    };
    if test_macro(source, node).is_some() {
        return 0;
    }
    let mut cursor = list.walk();
    let mut declared = list
        .named_children(&mut cursor)
        .filter(|p| !matches!(p.kind(), "comment" | "variadic_parameter"));
    match (declared.next(), declared.count()) {
        (Some(only), 0) if source.text(only) == "void" => 0,
        (first, rest) => usize::from(first.is_some()) + rest,
    }
}

fn branch_points(node: Node) -> i32 {
    let operator = node.child_by_field_name("operator").map(|o| o.kind());
    i32::from(match node.kind() {
        "if_statement"
        | "while_statement"
        | "do_statement"
        | "for_statement"
        | "for_range_loop"
        | "catch_clause"
        | "conditional_expression" => true,
        "case_statement" => node.child_by_field_name("value").is_some(),
        "binary_expression" => matches!(operator, Some("&&" | "||" | "and" | "or")),
        _ => false,
    })
}

fn test(source: &Source, node: Node) -> Option<Test> {
    test_macro(source, node).map(|_| Test::Itself)
}

/// What `node` declares, outermost first, down to the declared name: `int (*f(int))(char)` is a
/// function returning a pointer to a function, and declares `f`, with one `int` parameter.
fn declarators(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    fn inner<'t>(declarator: &Node<'t>) -> Option<Node<'t>> {
        match declarator.kind() {
            "function_declarator"
            | "abstract_function_declarator"
            | "pointer_declarator"
            | "array_declarator" => declarator.child_by_field_name("declarator"),
            "parenthesized_declarator" | "reference_declarator" | "attributed_declarator" => {
                declarator.named_child(0)
            }
            _ => None,
        }
    }
    iter::successors(node.child_by_field_name("declarator"), inner).take(limits::SYNTAX_DEPTH)
}

/// The invocation, `TEST(Suite, Name)`, where `node` is a test macro's body.
fn test_macro<'t>(source: &Source, node: Node<'t>) -> Option<Node<'t>> {
    if node.kind() != "function_definition" || node.child_by_field_name("type").is_some() {
        return None;
    }
    let invocation = node
        .child_by_field_name("declarator")
        .filter(|d| d.kind() == "function_declarator")?;
    let name = source.field(invocation, "declarator")?;
    TEST_MACROS.contains(&name.as_str()).then_some(invocation)
}

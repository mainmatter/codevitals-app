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
    match declared.next() {
        None => 0,
        Some(first) => {
            let rest = declared.count();
            if rest == 0 && source.text(first) == "void" {
                0
            } else {
                rest + 1
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::syntax::{parse, summary};

    #[test]
    fn else_if_is_flat_but_else_loops_nest() {
        for (statement, nesting) in [
            ("if (x) {} else if (x) {}", 1),
            ("if (x) {} else for (;;) {}", 2),
            ("if (x) {} else while (x) {}", 2),
        ] {
            let source = format!("void f(int x) {{ {statement} }}");
            for language in [&C, &CPP] {
                let code = parse(source.as_bytes(), language).unwrap();
                assert_eq!(code.errors, 0, "{source}");
                assert_eq!(code.functions[0].max_nesting, nesting, "{source}");
            }
        }
    }

    #[test]
    fn functions_returning_pointers_to_arrays() {
        for language in [&C, &CPP] {
            let code = parse(b"int (*f(int x))[3] { return 0; }", language).unwrap();
            assert_eq!(code.errors, 0);
            assert_eq!(code.functions.len(), 1);
            assert_eq!(code.functions[0].name, "f");
            assert_eq!(code.functions[0].parameters, 1);
        }
    }

    #[test]
    fn golden() {
        let facts = |source, language| summary(&parse(source, language).unwrap());
        assert_eq!(facts(include_bytes!("c/golden.c"), &C), GOLDEN_C.trim());
        assert_eq!(facts(include_bytes!("c/golden.h"), &CPP), GOLDEN_H.trim());
        assert_eq!(
            facts(include_bytes!("c/golden.cpp"), &CPP),
            GOLDEN_CPP.trim()
        );
    }

    // `clamp` and `bound` share a shape. The prototype of `area` is not a function; each `speed`
    // is one.
    const GOLDEN_C: &str = "
tokens 366 errors 0
area 12-14 tokens 16 parameters 2 branch points 0 nesting 0 shape d653b7e4
clamp 16-23 tokens 41 parameters 3 branch points 2 nesting 1 shape fccdb840
bound 26-33 tokens 41 parameters 3 branch points 2 nesting 1 shape fccdb840
describe 35-45 tokens 43 parameters 1 branch points 4 nesting 1 shape 51255981
search 47-59 tokens 84 parameters 3 branch points 6 nesting 3 shape cbcfc742
pick 61-63 tokens 23 parameters 1 branch points 1 nesting 0 shape 9a6b7039
log_all 65-67 tokens 19 parameters 1 branch points 0 nesting 0 shape 867d4bda
old_style 69-74 tokens 20 parameters 2 branch points 0 nesting 0 shape 29ba3fef
speed 77-77 tokens 10 parameters 0 branch points 0 nesting 0 shape 83dc6e60
speed 79-79 tokens 10 parameters 0 branch points 0 nesting 0 shape 83dc6e60
main 82-85 tokens 25 parameters 0 branch points 0 nesting 0 shape f6bf33cc
";

    const GOLDEN_H: &str = "
tokens 117 errors 0
point_sum 21-23 tokens 20 parameters 1 branch points 0 nesting 0 shape 326acd6a
sign 25-27 tokens 25 parameters 1 branch points 2 nesting 0 shape cd1e0c15
";

    // `Point()` and `draw` are declarations.
    const GOLDEN_CPP: &str = "
tokens 392 errors 0
geometry::Point::Point 10-10 tokens 20 parameters 2 branch points 0 nesting 0 shape bca9654c
geometry::Point::~Point 12-12 tokens 6 parameters 0 branch points 0 nesting 0 shape 9f2a5f21
geometry::Point::operator== 15-17 tokens 25 parameters 1 branch points 1 nesting 0 shape 0f96de02
geometry::Point::norm 24-26 tokens 24 parameters 0 branch points 0 nesting 0 shape 45a80c91
geometry::(anonymous namespace)::helper 29-37 tokens 46 parameters 1 branch points 3 nesting 2 shape 84e93eef
Box<T>::get 49-51 tokens 15 parameters 0 branch points 0 nesting 0 shape 61dbb6bc
parse 53-61 tokens 44 parameters 1 branch points 2 nesting 1 shape f3e1c7b2
main 63-70 tokens 53 parameters 0 branch points 0 nesting 0 shape e33fe1d6
main::{lambda#0} 65-65 tokens 16 parameters 2 branch points 0 nesting 0 shape 362c3577
main::{lambda#1} 66-68 tokens 15 parameters 0 branch points 0 nesting 0 shape 666c752a
main::{lambda#1}::{lambda#0} 67-67 tokens 13 parameters 1 branch points 0 nesting 0 shape 8f245cfd
TEST(Point, Norm) 72-75 tokens 31 parameters 0 branch points 0 nesting 0 shape 0946578d test
TEST(Point, Norm)::{lambda#0} 73-73 tokens 13 parameters 1 branch points 0 nesting 0 shape af5b837b test
";
}

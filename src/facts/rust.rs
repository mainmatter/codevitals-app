//! Rust, as tree-sitter-rust parses it.
//!
//! A function is a `fn` with a body, wherever it is, or a closure. Code inside macro invocations is
//! not parsed, so functions there are not seen.
//!
//! Names are as rustc writes paths: `outer::inner`, `tests::helper`, `Foo::new`,
//! `<Foo as Display>::fmt`, `Trait::provided`, `run::{closure#0}`. Types are as written.
//!
//! - parameters: `self` included.
//! - branch points: `if`, `while`, `for`, each match arm after the first, match guards, `&&`,
//!   `||`, `?` and `let … else`.
//! - nesting: `if`, `match`, `while`, `for` and `loop`.
//! - test: a test attribute, or a `cfg` that holds only with `test`.

use std::{iter::Peekable, vec};

use tree_sitter::Node;

use super::syntax::{Language, Source, Test};

pub const RUST: Language = Language {
    grammar: || tree_sitter_rust::LANGUAGE.into(),
    functions: &["function_item", "closure_expression"],
    scopes: &["mod_item", "trait_item", "impl_item"],
    nesting: &[
        "if_expression",
        "match_expression",
        "while_expression",
        "for_expression",
        "loop_expression",
    ],
    unnamed: "closure",
    name,
    parameters,
    branch_points,
    test,
};

/// And any attribute whose name ends in `_test`.
#[rustfmt::skip]
const TEST_ATTRIBUTES: [&str; 6] = ["test", "bench", "rstest", "quickcheck", "proptest", "test_case"];

fn name(source: &Source, node: Node) -> Option<String> {
    if node.kind() != "impl_item" {
        return source.field(node, "name");
    }
    let ty = source.field(node, "type")?;
    Some(match source.field(node, "trait") {
        Some(t) => format!("<{ty} as {t}>"),
        None => ty,
    })
}

/// `_` is an unnamed node, like punctuation, so this counts by kind.
fn parameters(_: &Source, node: Node) -> usize {
    let is_parameter = |child: &Node| {
        !child.is_extra() && !matches!(child.kind(), "(" | ")" | "|" | "," | "attribute_item")
    };
    node.child_by_field_name("parameters").map_or(0, |list| {
        list.children(&mut list.walk()).filter(is_parameter).count()
    })
}

fn branch_points(node: Node) -> i32 {
    let has = |field: &str| i32::from(node.child_by_field_name(field).is_some());
    let count = |kind: &str| {
        let mut cursor = node.walk();
        let children = node.children(&mut cursor);
        children.filter(|child| child.kind() == kind).count() as i32
    };
    let operator = node.child_by_field_name("operator").map(|o| o.kind());
    match node.kind() {
        "if_expression" | "while_expression" | "for_expression" | "try_expression" => 1,
        "match_block" => (count("match_arm") - 1).max(0),
        "match_pattern" => has("condition"),
        "let_declaration" => has("alternative"),
        "let_chain" => count("&&"),
        "binary_expression" => i32::from(matches!(operator, Some("&&" | "||"))),
        _ => 0,
    }
}

fn test(source: &Source, node: Node) -> Option<Test> {
    let marks = match node.kind() {
        "inner_attribute_item" => Test::Parent,
        "attribute_item" => Test::Next,
        _ => return None,
    };
    is_test_attribute(source, node).then_some(marks)
}

fn is_test_attribute(source: &Source, item: Node) -> bool {
    let Some(attribute) = item.named_child(0).filter(|a| a.kind() == "attribute") else {
        return false;
    };
    let path = attribute
        .named_child(0)
        .map_or(String::new(), |p| source.text(p));
    if path == "cfg" {
        let arguments = attribute.child_by_field_name("arguments");
        return arguments.is_some_and(|arguments| cfg_requires_test(source, arguments));
    }
    let name = path.rsplit("::").next().unwrap_or_default().trim();
    TEST_ATTRIBUTES.contains(&name) || name.ends_with("_test")
}

/// Whether a `cfg` predicate holds only when `test` does: `test` itself, `all` with such a part,
/// or `any` of only such parts; `not` never. Its lists are token trees, evaluated on a stack,
/// each with its operator, its tokens still to read and the values of its parts so far.
fn cfg_requires_test(source: &Source, arguments: Node) -> bool {
    fn tokens(list: Node<'_>) -> Peekable<vec::IntoIter<Node<'_>>> {
        let mut cursor = list.walk();
        let tokens = list
            .children(&mut cursor)
            .filter(|token| !token.is_extra() && !matches!(token.kind(), "(" | ")" | ","));
        tokens.collect::<Vec<_>>().into_iter().peekable()
    }
    let mut lists = vec![(String::new(), tokens(arguments), Vec::new())];
    while let Some((_, remaining, parts)) = lists.last_mut() {
        let Some(token) = remaining.next() else {
            let (operator, _, parts) = lists.pop().expect("the list just read");
            let value = match operator.as_str() {
                "" => parts == [true],
                "all" => parts.contains(&true),
                "any" => !parts.is_empty() && !parts.contains(&false),
                _ => false,
            };
            match lists.last_mut() {
                Some((_, _, parts)) => parts.push(value),
                None => return value,
            }
            continue;
        };
        match remaining.peek().map(|next| next.kind()) {
            Some("token_tree") => {
                let list = remaining.next().expect("peeked");
                lists.push((source.text(token), tokens(list), Vec::new()));
            }
            Some("=") => {
                remaining.nth(1);
                parts.push(false);
            }
            _ => parts.push(source.text(token) == "test"),
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::{
        CodeFacts,
        syntax::{parse, summary},
    };

    fn parse_rust(source: &[u8]) -> CodeFacts {
        parse(source, &RUST).unwrap()
    }

    #[test]
    fn golden() {
        // `helper` and `same_shape_as_helper` share a shape, as do `slow` and `sometimes`.
        assert_eq!(
            summary(&parse_rust(include_bytes!("rust/golden.rs"))),
            GOLDEN.trim()
        );

        let shape = |source: &str| parse_rust(source.as_bytes()).functions[0].shape;
        assert_eq!(
            shape(r#"fn f() { "hello" }"#),
            shape(r#"fn g() { "hello\n" }"#)
        );
    }

    const GOLDEN: &str = "
tokens 569 errors 0
Point::new 12-14 tokens 22 parameters 2 branch points 0 nesting 0 shape f765e5fe
Point::scaled 17-19 tokens 38 parameters 2 branch points 0 nesting 0 shape 09eb1bd7
Point::scaled::{closure#0} 18-18 tokens 6 parameters 1 branch points 0 nesting 0 shape 5c9ef157
<Point as fmt::Display>::fmt 23-25 tokens 39 parameters 2 branch points 0 nesting 0 shape 29173e1b
Shape::describe 31-33 tokens 33 parameters 1 branch points 1 nesting 1 shape ac200362
classify 36-62 tokens 131 parameters 2 branch points 10 nesting 3 shape 9b3c0ba1
classify::helper 37-39 tokens 25 parameters 1 branch points 2 nesting 0 shape 4b4645a5
classify::{closure#0} 43-43 tokens 24 parameters 1 branch points 0 nesting 0 shape 09a13a40
classify::{closure#0}::{closure#0} 43-43 tokens 8 parameters 1 branch points 0 nesting 0 shape 3fb2884d
classify::{closure#1} 58-58 tokens 3 parameters 0 branch points 0 nesting 0 shape 3c7207f0
same_shape_as_helper 64-66 tokens 25 parameters 1 branch points 2 nesting 0 shape 4b4645a5
tests::classifies 73-76 tokens 21 parameters 0 branch points 0 nesting 0 shape fbb654b2 test
tests::classifies::{closure#0} 74-74 tokens 13 parameters 1 branch points 0 nesting 0 shape 6d574216 test
tests::fixture 78-80 tokens 16 parameters 0 branch points 0 nesting 0 shape 54e1d5b9 test
support::always 86-88 tokens 13 parameters 1 branch points 0 nesting 0 shape 87a4e560 test
runs_async 92-92 tokens 7 parameters 0 branch points 0 nesting 0 shape b224bb64 test
slow 95-95 tokens 6 parameters 0 branch points 0 nesting 0 shape 41e038db test
sometimes 98-98 tokens 6 parameters 0 branch points 0 nesting 0 shape 41e038db
";

    #[test]
    fn cfg_requires_test_only_where_test_must_hold() {
        for (predicate, expected) in [
            ("(test)", true),
            ("(/* reason */ test)", true),
            ("(all(test, feature = \"slow\"))", true),
            ("(all(unix, any(test, all(test, doc))))", true),
            ("(any(test, feature = \"test\"))", false),
            ("(any(test, doc))", false),
            ("(not(test))", false),
            ("(feature = \"test\")", false),
            ("(all())", false),
            ("(any())", false),
        ] {
            let code = parse_rust(format!("#[cfg{predicate}] fn f() {{}}").as_bytes());
            assert_eq!(code.functions[0].test, expected, "{predicate}");
        }
    }
}

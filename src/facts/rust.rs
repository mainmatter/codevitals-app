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

use super::syntax::{Language, Source, Test, count_children};

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
    let operator = node.child_by_field_name("operator").map(|o| o.kind());
    match node.kind() {
        "if_expression" | "while_expression" | "for_expression" | "try_expression" => 1,
        "match_block" => (count_children(node, "match_arm") - 1).max(0),
        "match_pattern" => has("condition"),
        "let_declaration" => has("alternative"),
        "let_chain" => count_children(node, "&&"),
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

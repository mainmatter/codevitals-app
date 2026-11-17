//! Rust, as tree-sitter-rust parses it.
//!
//! A function is a `fn` with a body, wherever it is, or a closure. Code inside macro invocations is
//! not parsed, so functions there are not seen.
//!
//! Names are qualified from the file's root, as rustc writes paths: `outer::inner`,
//! `tests::helper`, `Foo::new`, `<Foo as Display>::fmt`, `Trait::provided`, `run::{closure#0}`.
//! Types are as written, whitespace collapsed. Closures are numbered within their enclosing name.
//!
//! A function's counts leave out the functions and closures inside it, which have their own:
//!
//! - tokens: leaves of the syntax tree, a string or boolean literal one leaf, comments and empty
//!   leaves excluded.
//! - parameters: `self` included.
//! - branch points: `if`, `while`, `for`, each match arm after the first, match guards, `&&`,
//!   `||`, `?` and `let … else`.
//! - maximum nesting: of `if`, `match`, `while`, `for` and `loop`. An `else if` does not nest.
//! - shape: a hash of its node kinds and their depths, in order.
//! - test: it or anything around it has a test attribute, or a `cfg` that holds only with `test`.
//! - errors: syntax errors the parser recovered from. Where there are any, counts are approximate.

use std::{iter::Peekable, vec};

use sha2::{Digest, Sha256};
use tree_sitter::{Node, Parser, Tree};

use super::{CodeFacts, FunctionFacts};
use crate::limits;

/// Or the status saying why it could not be.
pub fn parse(source: &[u8]) -> Result<CodeFacts, &'static str> {
    let source = str::from_utf8(source).map_err(|_| "unparseable")?;
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .expect("a grammar for this version of tree-sitter");
    let tree = parser
        .parse(source, None)
        .expect("parsing without a timeout finishes");
    TreeWalk::new(source).run(&tree)
}

/// And any attribute whose name ends in `_test`.
#[rustfmt::skip]
const TEST_ATTRIBUTES: [&str; 6] = ["test", "bench", "rstest", "quickcheck", "proptest", "test_case"];

/// Walked as one token: what is inside them is their value.
const LITERALS: [&str; 3] = ["string_literal", "raw_string_literal", "boolean_literal"];

/// One pass over the syntax tree, in order.
struct TreeWalk<'s> {
    source: &'s str,
    tokens: i32,
    errors: i32,
    scopes: Vec<Scope>,
    functions: Vec<OpenFunction>,
}

/// A qualified name, and the closures numbered in it so far.
#[derive(Default)]
struct Scope {
    name: String,
    closures: usize,
}

/// A function whose nodes are still being walked.
struct OpenFunction {
    facts: FunctionFacts,
    depth: usize,
    shape: Sha256,
}

/// What a node's ancestors pass on to it.
#[derive(Clone, Copy, Default)]
struct Enclosing<'t> {
    kind: &'t str,
    scope: usize,
    function: Option<usize>,
    nesting: i32,
    test: bool,
    next_is_test: bool,
}

impl<'s> TreeWalk<'s> {
    fn new(source: &'s str) -> Self {
        let scopes = vec![Scope::default()];
        TreeWalk {
            source,
            tokens: 0,
            errors: 0,
            scopes,
            functions: Vec::new(),
        }
    }

    fn run(mut self, tree: &Tree) -> Result<CodeFacts, &'static str> {
        let mut cursor = tree.walk();
        // What each node above the cursor passes on: a stack as deep as the tree.
        let mut ancestors = vec![Enclosing::default()];
        'walk: loop {
            let node = cursor.node();
            if !matches!(node.kind(), "line_comment" | "block_comment") {
                let parent = ancestors.last_mut().expect("the root has a parent entry");
                match node.kind() {
                    "inner_attribute_item" => parent.test |= self.is_test_attribute(node),
                    "attribute_item" => parent.next_is_test |= self.is_test_attribute(node),
                    _ => {}
                }
                let parent_kind = parent.kind;
                let mut enclosing = Enclosing {
                    kind: node.kind(),
                    test: parent.test || parent.next_is_test,
                    next_is_test: false,
                    ..*parent
                };
                parent.next_is_test &= node.kind() == "attribute_item";
                let leaf = node.child_count() == 0 || LITERALS.contains(&node.kind());
                self.visit_node(node, leaf, parent_kind, &mut enclosing, ancestors.len() - 1);
                if !leaf && cursor.goto_first_child() {
                    if ancestors.len() > limits::SYNTAX_DEPTH {
                        return Err("too_deep");
                    }
                    ancestors.push(enclosing);
                    continue;
                }
            }
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    break 'walk;
                }
                ancestors.pop();
            }
        }
        let functions = self.functions.into_iter().map(|open| FunctionFacts {
            shape: open.shape.finalize().into(),
            ..open.facts
        });
        Ok(CodeFacts {
            tokens: self.tokens,
            errors: self.errors,
            functions: functions.collect(),
        })
    }

    /// Counts `node` and updates what it passes on to its children.
    fn visit_node(
        &mut self,
        node: Node,
        leaf: bool,
        parent_kind: &str,
        enclosing: &mut Enclosing,
        depth: usize,
    ) {
        match node.kind() {
            "function_item" | "closure_expression" => self.open_function(node, enclosing, depth),
            "mod_item" | "trait_item" => {
                enclosing.scope = self.push_scope(enclosing.scope, &self.field_text(node, "name"));
            }
            "impl_item" => {
                let ty = self.field_text(node, "type");
                let name = match node.child_by_field_name("trait") {
                    Some(t) => format!("<{ty} as {}>", self.node_text(t)),
                    None => ty,
                };
                enclosing.scope = self.push_scope(enclosing.scope, &name);
            }
            _ => {}
        }
        let token = i32::from(leaf && !node.byte_range().is_empty());
        let error = i32::from(node.is_error() || node.is_missing());
        self.tokens += token;
        self.errors += error;

        let Some(index) = enclosing.function else {
            return;
        };
        let function = &mut self.functions[index];
        function
            .shape
            .update((depth - function.depth).to_le_bytes());
        function.shape.update(node.kind());
        function.shape.update([0]);
        let facts = &mut function.facts;
        facts.tokens += token;
        facts.errors += error;
        facts.branch_points += branch_points(node);
        if adds_nesting(node, parent_kind) {
            enclosing.nesting += 1;
            facts.max_nesting = facts.max_nesting.max(enclosing.nesting);
        }
    }

    /// Makes `node` the function its descendants are counted for.
    fn open_function(&mut self, node: Node, enclosing: &mut Enclosing, depth: usize) {
        let name = match node.kind() {
            "function_item" => self.field_text(node, "name"),
            _ => {
                let scope = &mut self.scopes[enclosing.scope];
                scope.closures += 1;
                format!("{{closure#{}}}", scope.closures - 1)
            }
        };
        enclosing.scope = self.push_scope(enclosing.scope, &name);
        // `_` is an unnamed node, like punctuation, so this counts by kind.
        let is_parameter = |child: &Node| {
            !child.is_extra() && !matches!(child.kind(), "(" | ")" | "|" | "," | "attribute_item")
        };
        let parameters = node.child_by_field_name("parameters").map_or(0, |list| {
            list.children(&mut list.walk()).filter(is_parameter).count()
        });
        self.functions.push(OpenFunction {
            facts: FunctionFacts {
                name: self.scopes[enclosing.scope].name.clone(),
                start_line: node.start_position().row as i32 + 1,
                end_line: node.end_position().row as i32 + 1,
                parameters: parameters as i32,
                test: enclosing.test,
                ..FunctionFacts::default()
            },
            depth,
            shape: Sha256::new(),
        });
        enclosing.function = Some(self.functions.len() - 1);
        enclosing.nesting = 0;
    }

    fn is_test_attribute(&self, item: Node) -> bool {
        let Some(attribute) = item.named_child(0).filter(|a| a.kind() == "attribute") else {
            return false;
        };
        let path = attribute
            .named_child(0)
            .map_or(String::new(), |p| self.node_text(p));
        if path == "cfg" {
            let arguments = attribute.child_by_field_name("arguments");
            return arguments.is_some_and(|arguments| self.cfg_requires_test(arguments));
        }
        let name = path.rsplit("::").next().unwrap_or_default().trim();
        TEST_ATTRIBUTES.contains(&name) || name.ends_with("_test")
    }

    /// Whether a `cfg` predicate holds only when `test` does: `test` itself, `all` with such a part,
    /// or `any` of only such parts; `not` never. Its lists are token trees, evaluated on a stack,
    /// each with its operator, its tokens still to read and the values of its parts so far.
    fn cfg_requires_test(&self, arguments: Node) -> bool {
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
                    lists.push((self.node_text(token), tokens(list), Vec::new()));
                }
                Some("=") => {
                    remaining.nth(1);
                    parts.push(false);
                }
                _ => parts.push(self.node_text(token) == "test"),
            }
        }
        false
    }

    fn push_scope(&mut self, parent: usize, name: &str) -> usize {
        let name = match &self.scopes[parent].name[..] {
            "" => name.to_owned(),
            parent => format!("{parent}::{name}"),
        };
        self.scopes.push(Scope { name, closures: 0 });
        self.scopes.len() - 1
    }

    /// Empty where a tree recovered from an error lacks the field.
    fn field_text(&self, node: Node, field: &str) -> String {
        node.child_by_field_name(field)
            .map_or(String::new(), |n| self.node_text(n))
    }

    /// Whitespace collapsed.
    fn node_text(&self, node: Node) -> String {
        let text = self.source.get(node.byte_range()).unwrap_or_default();
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

fn branch_points(node: Node) -> i32 {
    let has = |field: &str| i32::from(node.child_by_field_name(field).is_some());
    let count = |kind: &str| {
        let mut cursor = node.walk();
        let children = node.children(&mut cursor);
        children.filter(|child| child.kind() == kind).count() as i32
    };
    match node.kind() {
        "if_expression" | "while_expression" | "for_expression" | "try_expression" => 1,
        "match_block" => (count("match_arm") - 1).max(0),
        "match_pattern" => has("condition"),
        "let_declaration" => has("alternative"),
        "let_chain" => count("&&"),
        "binary_expression" => {
            let operator = node.child_by_field_name("operator");
            i32::from(operator.is_some_and(|o| matches!(o.kind(), "&&" | "||")))
        }
        _ => 0,
    }
}

fn adds_nesting(node: Node, parent_kind: &str) -> bool {
    match node.kind() {
        "if_expression" => parent_kind != "else_clause",
        "match_expression" | "while_expression" | "for_expression" | "loop_expression" => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden() {
        // `helper` and `same_shape_as_helper` share a shape, as do `slow` and `sometimes`.
        let code = parse(include_bytes!("rust/golden.rs")).unwrap();
        assert_eq!(code.errors, 0);
        let mut lines = vec![format!("tokens {}", code.tokens)];
        for function in code.functions {
            let shape: String = function.shape[..4]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            lines.push(format!(
                "{} {}-{} tokens {} parameters {} branch points {} nesting {} shape {shape}{}",
                function.name,
                function.start_line,
                function.end_line,
                function.tokens,
                function.parameters,
                function.branch_points,
                function.max_nesting,
                if function.test { " test" } else { "" },
            ));
        }
        assert_eq!(lines.join("\n"), GOLDEN.trim());

        let shape = |source: &str| parse(source.as_bytes()).unwrap().functions[0].shape;
        assert_eq!(
            shape(r#"fn f() { "hello" }"#),
            shape(r#"fn g() { "hello\n" }"#)
        );
    }

    const GOLDEN: &str = "
tokens 569
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
            let code = parse(format!("#[cfg{predicate}] fn f() {{}}").as_bytes()).unwrap();
            assert_eq!(code.functions[0].test, expected, "{predicate}");
        }
    }
}

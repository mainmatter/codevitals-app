//! A blob's facts, from its syntax tree and its [`Language`]'s definitions.
//!
//! Every language counts the same way:
//!
//! - functions and scopes: nodes of the language's kinds with a body. A function's name, and a
//!   scope's, qualifies the names inside it: `outer::inner`. An unnamed function is numbered within
//!   its enclosing name, `run::{closure#0}`; an unnamed scope qualifies nothing.
//! - tokens: leaves, a literal one leaf, comments and empty leaves excluded. Inside a literal only
//!   errors count, and depth, so code in a Python f-string is checked but not counted.
//! - A function's counts leave out the functions inside it, which have their own.
//! - maximum nesting: of the language's nesting kinds, from the function down. An `else if` is a
//!   sibling and does not add nesting.
//! - shape: a hash of its node kinds and their depths, in order.
//! - test: it, or anything around it, is marked as test code.
//! - errors: syntax errors the parser recovered from. Where there are any, counts are approximate.

use sha2::{Digest, Sha256};
use tree_sitter::{Node, Parser};

use crate::limits;

/// What parsing a blob found.
#[derive(Default)]
pub struct CodeFacts {
    pub tokens: i32,
    pub errors: i32,
    pub functions: Vec<FunctionFacts>,
}

#[derive(Default)]
pub struct FunctionFacts {
    pub name: String,
    pub start_line: i32,
    pub end_line: i32,
    pub tokens: i32,
    pub parameters: i32,
    pub branch_points: i32,
    pub max_nesting: i32,
    pub shape: [u8; 32],
    pub test: bool,
    pub errors: i32,
}

pub struct Language {
    pub grammar: fn() -> tree_sitter::Language,
    pub functions: &'static [&'static str],
    pub scopes: &'static [&'static str],
    pub nesting: &'static [&'static str],
    /// What an unnamed function is called, before its number.
    pub unnamed: &'static str,
    /// A function's or a scope's.
    pub name: fn(&Source, Node) -> Option<String>,
    pub parameters: fn(&Source, Node) -> usize,
    pub branch_points: fn(Node) -> i32,
    pub test: fn(&Source, Node) -> Option<Test>,
}

/// What a node marks as test code.
#[derive(PartialEq)]
pub enum Test {
    Itself,
    Parent,
    /// Its next sibling of another kind, so that markers stack.
    Next,
}

pub struct Source<'s>(&'s str);

impl Source<'_> {
    /// Whitespace collapsed.
    pub fn text(&self, node: Node) -> String {
        let text = &self.0[node.byte_range()];
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    pub fn field(&self, node: Node, field: &str) -> Option<String> {
        node.child_by_field_name(field).map(|n| self.text(n))
    }
}

/// Or the status saying why it could not be.
pub fn parse(source: &[u8], language: &Language) -> Result<CodeFacts, &'static str> {
    let source = Source(str::from_utf8(source).map_err(|_| "unparseable")?);
    let mut parser = Parser::new();
    parser
        .set_language(&(language.grammar)())
        .expect("a grammar for this version of tree-sitter");
    let tree = parser
        .parse(source.0, None)
        .expect("parsing without a timeout finishes");

    let mut walk = Walk {
        language,
        source,
        code: CodeFacts::default(),
        shapes: Vec::new(),
        scopes: vec![Scope::default()],
    };
    let mut cursor = tree.walk();
    // What each node above the cursor passes on: a stack as deep as the tree.
    let mut ancestors = vec![Enclosing::default()];
    'walk: loop {
        let node = cursor.node();
        let comment = node.is_extra() && !node.is_error();
        if !comment {
            let depth = ancestors.len();
            let parent = ancestors.last_mut().expect("the root has a parent entry");
            let enclosing = walk.visit(node, parent, depth);
            if cursor.goto_first_child() {
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
    for (function, shape) in walk.code.functions.iter_mut().zip(walk.shapes) {
        function.shape = shape.finalize().into();
    }
    Ok(walk.code)
}

/// Children of `node` of `kind`.
pub fn count_children(node: Node, kind: &str) -> i32 {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .filter(|child| child.kind() == kind)
        .count() as i32
}

/// Counted as one token: what is inside it is its value. The C and Rust grammars name literals
/// `…_literal`; tree-sitter-python's one literal with children is `string`.
fn is_literal(node: Node) -> bool {
    node.kind().ends_with("_literal") || node.kind() == "string"
}

struct Walk<'l, 's> {
    language: &'l Language,
    source: Source<'s>,
    code: CodeFacts,
    /// Each function's shape so far.
    shapes: Vec<Sha256>,
    scopes: Vec<Scope>,
}

/// A qualified name, and the unnamed functions numbered in it so far.
#[derive(Default)]
struct Scope {
    name: String,
    unnamed: usize,
}

/// What a node's ancestors pass on to it.
#[derive(Clone, Copy, Default)]
struct Enclosing<'t> {
    kind: &'t str,
    scope: usize,
    function: Option<usize>,
    /// The depth of the function's node.
    function_depth: usize,
    nesting: i32,
    test: bool,
    /// The kind of the child that marks its next sibling as test code.
    test_next: Option<&'t str>,
    literal: bool,
}

impl Walk<'_, '_> {
    /// Counts `node`, and returns what it passes on to its children.
    fn visit<'t>(
        &mut self,
        node: Node<'t>,
        parent: &mut Enclosing<'t>,
        depth: usize,
    ) -> Enclosing<'t> {
        let error = i32::from(node.is_error() || node.is_missing());
        self.code.errors += error;
        if parent.literal {
            if let Some(index) = parent.function {
                self.code.functions[index].errors += error;
            }
            return *parent;
        }
        let language = self.language;
        let kind = node.kind();
        let mark = (language.test)(&self.source, node);
        match mark {
            Some(Test::Parent) => parent.test = true,
            Some(Test::Next) => parent.test_next = Some(kind),
            _ => {}
        }
        let marked_by_sibling = parent.test_next.is_some_and(|marker| marker != kind);
        if marked_by_sibling {
            parent.test_next = None;
        }
        let mut enclosing = Enclosing {
            kind,
            test: parent.test || marked_by_sibling || mark == Some(Test::Itself),
            test_next: None,
            literal: is_literal(node),
            ..*parent
        };

        let has_body = || node.child_by_field_name("body").is_some();
        if language.functions.contains(&kind) && has_body() {
            let name = (language.name)(&self.source, node).unwrap_or_else(|| {
                let scope = &mut self.scopes[enclosing.scope];
                scope.unnamed += 1;
                format!("{{{}#{}}}", language.unnamed, scope.unnamed - 1)
            });
            enclosing.scope = self.push_scope(enclosing.scope, &name);
            enclosing.function = Some(self.code.functions.len());
            enclosing.function_depth = depth;
            enclosing.nesting = 0;
            self.code.functions.push(FunctionFacts {
                name: self.scopes[enclosing.scope].name.clone(),
                start_line: node.start_position().row as i32 + 1,
                end_line: node.end_position().row as i32 + 1,
                parameters: (language.parameters)(&self.source, node) as i32,
                test: enclosing.test,
                ..FunctionFacts::default()
            });
            self.shapes.push(Sha256::new());
        } else if language.scopes.contains(&kind)
            && has_body()
            && let Some(name) = (language.name)(&self.source, node)
        {
            enclosing.scope = self.push_scope(enclosing.scope, &name);
        }

        let leaf = node.child_count() == 0 || enclosing.literal;
        let token = i32::from(leaf && !node.byte_range().is_empty());
        self.code.tokens += token;
        let Some(index) = enclosing.function else {
            return enclosing;
        };
        let shape = &mut self.shapes[index];
        shape.update((depth - enclosing.function_depth).to_le_bytes());
        shape.update(kind);
        shape.update([0]);
        let function = &mut self.code.functions[index];
        function.tokens += token;
        function.errors += error;
        function.branch_points += (language.branch_points)(node);
        let else_if =
            parent.kind == "else_clause" && matches!(kind, "if_statement" | "if_expression");
        if language.nesting.contains(&kind) && !else_if {
            enclosing.nesting += 1;
            function.max_nesting = function.max_nesting.max(enclosing.nesting);
        }
        enclosing
    }

    fn push_scope(&mut self, parent: usize, name: &str) -> usize {
        let name = match &self.scopes[parent].name[..] {
            "" => name.to_owned(),
            parent => format!("{parent}::{name}"),
        };
        self.scopes.push(Scope { name, unnamed: 0 });
        self.scopes.len() - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::{c, python, rust};

    /// Each golden source beside its `.facts`: a line for the file, then one per function.
    #[test]
    fn golden() {
        for (source, language, expected) in [
            (
                &include_bytes!("rust/golden.rs")[..],
                &rust::RUST,
                include_str!("rust/golden.rs.facts"),
            ),
            (
                include_bytes!("c/golden.c"),
                &c::C,
                include_str!("c/golden.c.facts"),
            ),
            (
                include_bytes!("c/golden.h"),
                &c::CPP,
                include_str!("c/golden.h.facts"),
            ),
            (
                include_bytes!("c/golden.cpp"),
                &c::CPP,
                include_str!("c/golden.cpp.facts"),
            ),
            (
                include_bytes!("python/golden.py"),
                &python::PYTHON,
                include_str!("python/golden.py.facts"),
            ),
        ] {
            let code = parse(source, language).unwrap();
            let mut lines = vec![format!("tokens {} errors {}", code.tokens, code.errors)];
            for f in &code.functions {
                let shape: String = f.shape[..4]
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect();
                let test = if f.test { " test" } else { "" };
                lines.push(format!(
                    "{} {}-{} tokens {} parameters {} branch points {} nesting {} shape {shape}{test}",
                    f.name, f.start_line, f.end_line, f.tokens, f.parameters, f.branch_points,
                    f.max_nesting,
                ));
            }
            assert_eq!(lines.join("\n"), expected.trim());
        }
    }
}

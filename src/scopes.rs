//! Semantic structure through real parsers.
//!
//! Everything "semantic" about a file — which definition encloses a line,
//! where definitions begin and end for window splitting — is answered by
//! tree-sitter grammars, the same parsers editors ship. Nothing here scans
//! braces or guesses at strings: a fixture constant containing `"fn main() {"`
//! is a string to the grammar, full stop (the hand-rolled scanner this module
//! replaced was fooled by exactly that).
//!
//! A language without a bundled grammar simply fails [`parse`], and the
//! caller falls back to hunk context and blank-line windows — degraded
//! gracefully, never parsed wrongly.

use tree_sitter::{Node, Point, Tree};

/// How many contiguous lines of attributes, decorators, and doc comments may
/// ride along above a definition as part of its span.
const MAX_ATTACHED_LINES: usize = 12;

#[derive(Clone, Copy, PartialEq)]
enum Grammar {
    Rust,
    Python,
    Js,
    Ts,
    Go,
    Java,
    C,
    Cpp,
    CSharp,
    Php,
}

fn grammar_for(path: &str) -> Option<(Grammar, tree_sitter::Language)> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => (Grammar::Rust, tree_sitter_rust::LANGUAGE.into()),
        "py" | "pyi" => (Grammar::Python, tree_sitter_python::LANGUAGE.into()),
        "js" | "jsx" | "mjs" | "cjs" => (Grammar::Js, tree_sitter_javascript::LANGUAGE.into()),
        "ts" => (
            Grammar::Ts,
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        ),
        "tsx" => (Grammar::Ts, tree_sitter_typescript::LANGUAGE_TSX.into()),
        "go" => (Grammar::Go, tree_sitter_go::LANGUAGE.into()),
        "java" => (Grammar::Java, tree_sitter_java::LANGUAGE.into()),
        "c" | "h" => (Grammar::C, tree_sitter_c::LANGUAGE.into()),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => (Grammar::Cpp, tree_sitter_cpp::LANGUAGE.into()),
        "cs" => (Grammar::CSharp, tree_sitter_c_sharp::LANGUAGE.into()),
        "php" => (Grammar::Php, tree_sitter_php::LANGUAGE_PHP.into()),
        _ => return None,
    })
}

pub struct Scope {
    /// 0-based inclusive rows, attached attributes/doc comments included.
    pub start0: usize,
    pub end0: usize,
    /// The definition's own header line, trimmed and de-braced.
    pub header: String,
}

/// A file parsed once, queried many times.
pub struct ParsedFile {
    tree: Tree,
    grammar: Grammar,
    lines: Vec<String>,
}

pub fn parse(path: &str, source: &str) -> Option<ParsedFile> {
    let (grammar, language) = grammar_for(path)?;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(source, None)?;
    Some(ParsedFile {
        tree,
        grammar,
        lines: source.lines().map(|s| s.to_string()).collect(),
    })
}

impl ParsedFile {
    /// The innermost definition enclosing the given row, or None at file
    /// scope. Errors in the source degrade to smaller answers, never wrong
    /// ones — tree-sitter always produces a tree.
    pub fn scope_for(&self, target0: usize) -> Option<Scope> {
        // Anchor past the indentation: at column 0 an indented `def` line
        // resolves to the *enclosing* block, not the definition on the line.
        let column = self
            .lines
            .get(target0)
            .map(|l| l.len() - l.trim_start().len())
            .unwrap_or(0);
        let point = Point {
            row: target0,
            column,
        };
        let mut node = self
            .tree
            .root_node()
            .descendant_for_point_range(point, point)?;
        loop {
            if node.parent().is_some() && self.is_definition(&node) {
                break;
            }
            node = node.parent()?;
        }
        // A decorated definition is one thing with its decorators.
        if let Some(parent) = node.parent() {
            if parent.kind() == "decorated_definition" {
                node = parent;
            }
        }
        Some(Scope {
            start0: self.attached_start(&node),
            end0: node.end_position().row,
            header: self.label(&node),
        })
    }

    /// Definition spans intersecting [lo0, hi0] (0-based rows, inclusive),
    /// in source order. A definition still longer than `max_len` rows is
    /// replaced by the definitions inside it when at least two exist — an
    /// `impl` block yields its methods, a class its defs.
    pub fn definition_spans(&self, lo0: usize, hi0: usize, max_len: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        self.collect_spans(self.tree.root_node(), lo0, hi0, max_len, 0, &mut out);
        out
    }

    fn collect_spans(
        &self,
        node: Node,
        lo0: usize,
        hi0: usize,
        max_len: usize,
        depth: usize,
        out: &mut Vec<(usize, usize)>,
    ) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            let (cs, ce) = (child.start_position().row, child.end_position().row);
            if ce < lo0 || cs > hi0 {
                continue;
            }
            if self.is_definition(&child) {
                let start = self.attached_start(&child);
                if ce - start + 1 > max_len && depth < 3 {
                    let mut inner = Vec::new();
                    self.collect_spans(child, lo0, hi0, max_len, depth + 1, &mut inner);
                    if inner.len() >= 2 {
                        out.extend(inner);
                        continue;
                    }
                }
                out.push((start, ce));
            } else {
                // Wrappers (export statements, declaration lists, error
                // recovery nodes) are transparent on the way down.
                self.collect_spans(child, lo0, hi0, max_len, depth, out);
            }
        }
    }

    fn is_definition(&self, node: &Node) -> bool {
        let k = node.kind();
        match self.grammar {
            Grammar::Rust => matches!(
                k,
                "function_item"
                    | "struct_item"
                    | "enum_item"
                    | "union_item"
                    | "trait_item"
                    | "impl_item"
                    | "mod_item"
                    | "macro_definition"
                    | "static_item"
                    | "const_item"
                    | "type_item"
            ),
            Grammar::Python => {
                matches!(
                    k,
                    "function_definition" | "class_definition" | "decorated_definition"
                )
            }
            Grammar::Js | Grammar::Ts => {
                matches!(
                    k,
                    "function_declaration"
                        | "generator_function_declaration"
                        | "class_declaration"
                        | "method_definition"
                        | "interface_declaration"
                        | "enum_declaration"
                        | "type_alias_declaration"
                        | "abstract_class_declaration"
                        | "module_declaration"
                ) || declares_function(node)
            }
            Grammar::Go => {
                matches!(
                    k,
                    "function_declaration" | "method_declaration" | "type_declaration"
                )
            }
            Grammar::Java => matches!(
                k,
                "class_declaration"
                    | "interface_declaration"
                    | "enum_declaration"
                    | "record_declaration"
                    | "annotation_type_declaration"
                    | "method_declaration"
                    | "constructor_declaration"
            ),
            Grammar::C => matches!(
                k,
                "function_definition"
                    | "type_definition"
                    | "struct_specifier"
                    | "enum_specifier"
                    | "union_specifier"
            ),
            Grammar::Cpp => matches!(
                k,
                "function_definition"
                    | "type_definition"
                    | "class_specifier"
                    | "struct_specifier"
                    | "enum_specifier"
                    | "union_specifier"
                    | "namespace_definition"
                    | "template_declaration"
            ),
            Grammar::CSharp => matches!(
                k,
                "method_declaration"
                    | "class_declaration"
                    | "struct_declaration"
                    | "interface_declaration"
                    | "enum_declaration"
                    | "namespace_declaration"
                    | "file_scoped_namespace_declaration"
                    | "constructor_declaration"
                    | "property_declaration"
                    | "record_declaration"
                    | "local_function_statement"
            ),
            Grammar::Php => matches!(
                k,
                "function_definition"
                    | "method_declaration"
                    | "class_declaration"
                    | "interface_declaration"
                    | "trait_declaration"
                    | "enum_declaration"
            ),
        }
    }

    /// The definition's start row, pulled up over the attributes, decorators,
    /// and doc comments sitting directly above it.
    fn attached_start(&self, node: &Node) -> usize {
        let mut start = node.start_position().row;
        let mut prev = node.prev_named_sibling();
        while let Some(p) = prev {
            let k = p.kind();
            let attached = k.contains("comment")
                || k == "attribute_item"
                || k == "attribute_list"
                || k == "decorator"
                || k == "annotation"
                || k == "marker_annotation";
            // A node whose end position is column 0 of the next row really
            // ended on the row before (it swallowed the newline).
            let end_row = if p.end_position().column == 0 {
                p.end_position().row.saturating_sub(1)
            } else {
                p.end_position().row
            };
            let contiguous = end_row + 1 == start;
            if !attached
                || !contiguous
                || node
                    .start_position()
                    .row
                    .saturating_sub(p.start_position().row)
                    > MAX_ATTACHED_LINES
            {
                break;
            }
            start = p.start_position().row;
            prev = p.prev_named_sibling();
        }
        start
    }

    /// A short human label for the definition: the line its name (or
    /// declarator) sits on, trimmed of trailing block punctuation.
    fn label(&self, node: &Node) -> String {
        let mut named = *node;
        if let Some(d) = node.child_by_field_name("definition") {
            named = d; // python decorated_definition → the def itself
        }
        let row = named
            .child_by_field_name("name")
            .or_else(|| named.child_by_field_name("declarator"))
            .map(|n| n.start_position().row)
            .unwrap_or_else(|| named.start_position().row);
        let text = self.lines.get(row).map(|s| s.trim()).unwrap_or("");
        let cleaned = text
            .trim_end_matches('{')
            .trim_end()
            .trim_end_matches(':')
            .trim_end();
        crate::app::truncate(cleaned, 60)
    }
}

/// JS/TS `const f = () => {...}` and friends: a variable declaration whose
/// value is a function is a definition in every way that matters here.
fn declares_function(node: &Node) -> bool {
    if !matches!(node.kind(), "lexical_declaration" | "variable_declaration") {
        return false;
    }
    let mut cursor = node.walk();
    for d in node.named_children(&mut cursor) {
        let is_fn = d.kind() == "variable_declarator"
            && d.child_by_field_name("value").is_some_and(|v| {
                matches!(
                    v.kind(),
                    "arrow_function" | "function_expression" | "function" | "generator_function"
                )
            });
        if is_fn {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(path: &str, src: &str) -> ParsedFile {
        parse(path, src).expect("grammar")
    }

    #[test]
    fn the_enclosing_function_wins_over_the_if_and_brings_its_docs() {
        let src = "use x;\n\nimpl Foo {\n    /// Doc.\n    #[inline]\n    pub fn bar(&self) -> u32 {\n        if self.ready {\n            self.count += 1;\n        }\n        self.count\n    }\n}\n";
        let p = parsed("a.rs", src);
        let s = p.scope_for(7).expect("scope");
        assert_eq!(s.header, "pub fn bar(&self) -> u32");
        assert_eq!(
            (s.start0, s.end0),
            (3, 10),
            "doc comment and attribute ride along"
        );
        // A line outside every definition scopes to nothing.
        assert!(p.scope_for(0).is_none() || p.scope_for(0).unwrap().header.contains("use"));
    }

    #[test]
    fn strings_containing_code_fool_nobody() {
        // The exact failure that killed the hand-rolled scanner: a fixture
        // string full of braces and a fake fn header.
        let src = "const FIXTURE: &str = \"fn main() {\\n    boom();\\n}\";\n\nfn real() {\n    work();\n}\n";
        let p = parsed("a.rs", src);
        let s = p.scope_for(3).expect("scope");
        assert_eq!(s.header, "fn real()");
        assert_eq!((s.start0, s.end0), (2, 4));
        let spans = p.definition_spans(0, 5, 500);
        assert_eq!(
            spans.len(),
            2,
            "the const and the fn — no phantom items: {spans:?}"
        );
    }

    #[test]
    fn c_functions_span_their_multiline_signatures() {
        let src = "#include <stdio.h>\n\nstatic int\nhelper(int a,\n       int b)\n{\n    return a + b;\n}\n";
        let p = parsed("a.c", src);
        let s = p.scope_for(6).expect("scope");
        assert_eq!(
            (s.start0, s.end0),
            (2, 7),
            "the return type line belongs to the function"
        );
    }

    #[test]
    fn go_methods_are_definitions() {
        let src = "package main\n\nfunc (s *Server) Handle(w http.ResponseWriter) {\n\tif s.ok {\n\t\tserve(w)\n\t}\n}\n";
        let p = parsed("a.go", src);
        let s = p.scope_for(4).expect("scope");
        assert!(
            s.header.starts_with("func (s *Server) Handle"),
            "{}",
            s.header
        );
    }

    #[test]
    fn python_defs_carry_their_decorators() {
        let src = "import os\n\nclass Greeter:\n    @property\n    def name(self):\n        if self.formal:\n            return self.title\n\n        return self.nick\n\nTOP = 1\n";
        let p = parsed("a.py", src);
        let s = p.scope_for(8).expect("scope");
        assert_eq!(s.header, "def name(self)");
        assert_eq!((s.start0, s.end0), (3, 8));
        assert!(p.scope_for(10).is_none(), "module level is not a scope");
    }

    #[test]
    fn a_changed_def_line_scopes_to_itself() {
        let src = "class A:\n    def f(self, x):\n        return x\n";
        let p = parsed("a.py", src);
        let s = p.scope_for(1).expect("scope");
        assert_eq!(s.header, "def f(self, x)");
        assert_eq!((s.start0, s.end0), (1, 2));
    }

    #[test]
    fn js_arrow_bindings_and_exports_are_definitions() {
        let src = "import x from 'x';\n\nexport const handler = async (req) => {\n  return x(req);\n};\n\nfunction plain() {\n  return 1;\n}\n";
        let p = parsed("a.js", src);
        let s = p.scope_for(3).expect("scope");
        assert!(s.header.contains("handler"), "{}", s.header);
        let spans = p.definition_spans(0, 8, 500);
        assert_eq!(
            spans.len(),
            2,
            "the export wrapper is transparent: {spans:?}"
        );
    }

    #[test]
    fn an_oversized_container_yields_its_children() {
        let mut src = String::from("impl S {\n");
        for k in 0..4 {
            src.push_str(&format!("    fn m{k}() {{\n"));
            for _ in 0..10 {
                src.push_str("        step();\n");
            }
            src.push_str("    }\n");
        }
        src.push_str("}\n");
        let p = parsed("a.rs", &src);
        // Cap below the impl's size: it must hand back its methods.
        let spans = p.definition_spans(0, 60, 20);
        assert_eq!(spans.len(), 4, "{spans:?}");
        assert_eq!(spans[0].0, 1);
        // Cap above its size: the impl stays whole.
        let spans = p.definition_spans(0, 60, 500);
        assert_eq!(spans.len(), 1);
    }

    #[test]
    fn unknown_languages_do_not_parse() {
        assert!(parse("a.kt", "fun main() {}").is_none());
        assert!(parse("notes.conf", "a = 1").is_none());
    }

    #[test]
    fn broken_source_still_answers_what_it_can() {
        // tree-sitter recovers around errors; a missing brace must not turn
        // into a wrong scope, just possibly a smaller one.
        let src = "fn ok() {\n    fine();\n}\n\nfn broken( {\n    what
";
        let p = parsed("a.rs", src);
        let s = p.scope_for(1).expect("scope");
        assert_eq!(s.header, "fn ok()");
        assert_eq!((s.start0, s.end0), (0, 2));
    }
}

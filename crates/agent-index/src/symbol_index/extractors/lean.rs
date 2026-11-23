//! Lean 4 symbol extractor.
//!
//! The grammar emits `namespace`, `section`, `mutual` and `end` as flat sibling
//! commands under the root `module` node, so nesting is rebuilt here with a
//! stack of open scopes. Namespaces become `Module` symbols that own the
//! declarations inside them; sections and `mutual` blocks are transparent.

use tree_sitter::{Node, Parser};

use super::super::types::SymbolDigest;
use super::safe_slice;
use crate::symbol_index::{SymbolEntry, SymbolError, SymbolKind};

pub fn extract(source: &str) -> Result<Vec<SymbolEntry>, SymbolError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_lean::LANGUAGE.into())
        .map_err(|e| SymbolError::ParseError(format!("Failed to load Lean grammar: {e}")))?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| SymbolError::ParseError("Failed to parse Lean source".to_string()))?;

    let mut walker = Walker {
        source,
        stack: Vec::new(),
        top: Vec::new(),
    };
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        walker.handle(&child);
    }
    walker.finish_at_eof();
    Ok(walker.top)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FrameKind {
    Namespace,
    Section,
    Mutual,
}

struct Frame {
    kind: FrameKind,
    /// Name as written in the opening command (`A.B` for `namespace A.B`).
    name: Option<String>,
    /// Scopes still open for this frame, one per dotted component. Lean's
    /// `namespace A.B` opens two scopes, so `end B` closes only the last one
    /// and `end A.B` closes both. Empty for anonymous sections and `mutual`.
    components: Vec<String>,
    /// Qualified name and parent of the namespace symbol, fixed at open time.
    qualified: String,
    parent: Option<String>,
    start_byte: usize,
    start_line: usize,
    children: Vec<SymbolEntry>,
}

struct Walker<'a> {
    source: &'a str,
    stack: Vec<Frame>,
    top: Vec<SymbolEntry>,
}

impl Walker<'_> {
    fn handle(&mut self, node: &Node) {
        if node.is_missing() {
            return;
        }
        match node.kind() {
            "ERROR" => {
                // Recover whatever the parser still recognised inside a
                // malformed region.
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    self.handle(&child);
                }
            }
            "open" | "set_option" | "variable" => {
                // `<cmd> … in <command>` scopes the prefix to the single
                // command that follows. That command is a nested node, so it
                // must be handled here or it is lost.
                match node.child_by_field_name("body") {
                    Some(body) => self.handle(&body),
                    None if node.kind() == "open" => self.push_import(node),
                    None => {}
                }
            }
            "import" | "export" => self.push_import(node),
            "namespace" => {
                if let Some(name) = field_text(node, "name", self.source) {
                    self.push_frame(FrameKind::Namespace, Some(name), node);
                }
            }
            "section" => {
                let name = field_text(node, "name", self.source);
                self.push_frame(FrameKind::Section, name, node);
            }
            // `public section` is an ordinary section: a later `end` closes it.
            "public_section" => self.push_frame(FrameKind::Section, None, node),
            "mutual" => self.push_frame(FrameKind::Mutual, None, node),
            "end" => self.handle_end(node),
            "declaration" => {
                if let Some(entry) = self.declaration(node) {
                    self.push_symbol(entry);
                }
            }
            "macro_cmd"
            | "syntax_cmd"
            | "notation_decl_cmd"
            | "macro_rules_cmd"
            | "elab_rules_cmd"
            | "declare_syntax_cat_cmd" => {
                let signature = first_line(node, self.source);
                let (name, qualified) = match field_text(node, "name", self.source) {
                    Some(name) => {
                        let qualified = qualify(&self.prefix(), &name);
                        (name, qualified)
                    }
                    None => (signature.clone(), signature.clone()),
                };
                let entry = self.entry(node, SymbolKind::Macro, name, qualified, signature, vec![]);
                self.push_symbol(entry);
            }
            _ => {}
        }
    }

    fn push_import(&mut self, node: &Node) {
        let text = first_line(node, self.source);
        let entry = self.entry(
            node,
            SymbolKind::Import,
            text.clone(),
            text.clone(),
            text,
            Vec::new(),
        );
        self.push_symbol(entry);
    }

    fn push_frame(&mut self, kind: FrameKind, name: Option<String>, node: &Node) {
        let components = name.as_deref().map(split_components).unwrap_or_default();
        let prefix = self.prefix();
        let qualified = match name.as_deref() {
            Some(name) => qualify(&prefix, name),
            None => prefix.clone(),
        };
        self.stack.push(Frame {
            kind,
            name,
            components,
            qualified,
            parent: (!prefix.is_empty()).then_some(prefix),
            start_byte: node.start_byte(),
            start_line: node.start_position().row + 1,
            children: Vec::new(),
        });
    }

    /// Finds the frame a named `end` closes. Returns the index of the lowest
    /// frame it touches and how many of that frame's trailing components it
    /// consumes. Frames above it are closed implicitly (unclosed sections).
    fn match_end(&self, target: &[String]) -> Option<(usize, usize)> {
        for top in (0..self.stack.len()).rev() {
            let mut pos = target.len();
            let mut index = top;
            loop {
                let components = &self.stack[index].components;
                if components.is_empty() {
                    break;
                }
                let take = pos.min(components.len());
                if components[components.len() - take..] != target[pos - take..pos] {
                    break;
                }
                pos -= take;
                if pos == 0 {
                    return Some((index, take));
                }
                if index == 0 {
                    break;
                }
                index -= 1;
            }
        }
        None
    }

    fn handle_end(&mut self, node: &Node) {
        let end_byte = node.end_byte();
        let end_line = node.end_position().row + 1;
        match field_text(node, "name", self.source) {
            Some(name) => {
                let target = split_components(&name);
                if target.is_empty() {
                    return;
                }
                let Some((index, take)) = self.match_end(&target) else {
                    // A stray `end Foo` must not destroy the open scopes.
                    return;
                };
                while self.stack.len() > index + 1 {
                    self.close_top(end_byte, end_line);
                }
                let remaining = self.stack[index].components.len();
                if take >= remaining {
                    self.close_top(end_byte, end_line);
                } else {
                    // `namespace A.B` … `end B` leaves scope `A` open.
                    self.stack[index].components.truncate(remaining - take);
                }
            }
            None => {
                if !self.stack.is_empty() {
                    self.close_top(end_byte, end_line);
                }
            }
        }
    }

    fn finish_at_eof(&mut self) {
        let end_byte = self.source.trim_end().len();
        let end_line = self.source[..end_byte].matches('\n').count() + 1;
        while !self.stack.is_empty() {
            self.close_top(end_byte, end_line);
        }
    }

    /// Pops the innermost frame. A namespace becomes a `Module` symbol that
    /// ends at `end_byte`.
    fn close_top(&mut self, end_byte: usize, end_line: usize) {
        let Some(frame) = self.stack.pop() else {
            return;
        };
        if frame.kind != FrameKind::Namespace {
            return;
        }
        let Some(name) = frame.name else {
            return;
        };
        let text = safe_slice(self.source, frame.start_byte, end_byte);
        let entry = SymbolEntry {
            kind: SymbolKind::Module,
            qualified_name: frame.qualified,
            signature: format!("namespace {name}"),
            name,
            start_line: frame.start_line,
            end_line,
            start_byte: frame.start_byte,
            end_byte,
            body_start_line: None,
            body_end_line: None,
            parent: frame.parent,
            children: frame.children,
            digest: SymbolDigest::new(text.as_bytes(), text.lines().count()),
        };
        self.push_symbol(entry);
    }

    /// Dotted names of all open namespaces.
    fn prefix(&self) -> String {
        self.stack
            .iter()
            .filter(|frame| frame.kind == FrameKind::Namespace)
            .flat_map(|frame| frame.components.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(".")
    }

    fn push_symbol(&mut self, mut entry: SymbolEntry) {
        // Imports and `open`s always list at top level, even when written
        // inside a namespace, so the `imports` outline section stays complete.
        if entry.kind == SymbolKind::Import {
            entry.parent = None;
            self.top.push(entry);
            return;
        }
        match self
            .stack
            .iter_mut()
            .rev()
            .find(|frame| frame.kind == FrameKind::Namespace)
        {
            Some(frame) => frame.children.push(entry),
            None => self.top.push(entry),
        }
    }

    /// Builds an entry for `range_node` with the common range and digest
    /// fields. `parent` is derived from the enclosing namespaces.
    fn entry(
        &self,
        range_node: &Node,
        kind: SymbolKind,
        name: String,
        qualified_name: String,
        signature: String,
        children: Vec<SymbolEntry>,
    ) -> SymbolEntry {
        let prefix = self.prefix();
        let mut entry = build_entry(
            range_node,
            self.source,
            kind,
            name,
            qualified_name,
            signature,
            children,
        );
        entry.parent = (!prefix.is_empty()).then_some(prefix);
        entry
    }

    fn declaration(&self, node: &Node) -> Option<SymbolEntry> {
        let mut cursor = node.walk();
        let body = node.named_children(&mut cursor).find(|child| {
            matches!(
                child.kind(),
                "def"
                    | "abbrev"
                    | "opaque"
                    | "theorem"
                    | "instance"
                    | "structure"
                    | "inductive"
                    | "axiom"
                    | "constant"
                    | "example"
            )
        })?;

        let signature = header_signature(node, self.source);
        let prefix = self.prefix();

        if body.kind() == "example" {
            let qualified = qualify(&prefix, "example");
            return Some(self.entry(
                node,
                SymbolKind::Test,
                "example".to_string(),
                qualified,
                signature,
                Vec::new(),
            ));
        }

        let name = match body.kind() {
            "instance" => field_text(&body, "name", self.source)
                .or_else(|| instance_type_text(&body, self.source))?,
            _ => field_text(&body, "name", self.source)?,
        };
        let qualified = qualify(&prefix, &name);

        let (kind, children) = match body.kind() {
            "def" | "abbrev" | "opaque" | "theorem" => {
                (SymbolKind::Function, self.where_helpers(&body, &qualified))
            }
            "instance" => (SymbolKind::Impl, self.instance_fields(&body, &qualified)),
            "structure" => {
                let is_class = node_text(&body, self.source)
                    .trim_start()
                    .starts_with("class");
                if is_class {
                    (
                        SymbolKind::Trait,
                        self.structure_members(&body, &qualified, true),
                    )
                } else {
                    (
                        SymbolKind::Struct,
                        self.structure_members(&body, &qualified, false),
                    )
                }
            }
            "inductive" => (SymbolKind::Enum, self.constructors(&body, &qualified)),
            // axiom | constant
            _ => (SymbolKind::Const, Vec::new()),
        };

        Some(self.entry(node, kind, name, qualified, signature, children))
    }

    /// `where` helper functions of a definition or theorem.
    fn where_helpers(&self, body: &Node, parent: &str) -> Vec<SymbolEntry> {
        let mut out = Vec::new();
        let mut cursor = body.walk();
        for decls in body
            .named_children(&mut cursor)
            .filter(|c| c.kind() == "where_decls")
        {
            let mut inner = decls.walk();
            for aux in decls
                .named_children(&mut inner)
                .filter(|c| c.kind() == "where_aux_def")
            {
                self.push_child(&aux, "name", SymbolKind::Method, parent, &mut out);
            }
        }
        out
    }

    /// `where` field assignments of an instance.
    fn instance_fields(&self, body: &Node, parent: &str) -> Vec<SymbolEntry> {
        let mut out = Vec::new();
        let mut cursor = body.walk();
        for decls in body
            .named_children(&mut cursor)
            .filter(|c| c.kind() == "where_struct")
        {
            let mut inner = decls.walk();
            for field in decls
                .named_children(&mut inner)
                .filter(|c| c.kind() == "struct_field")
            {
                self.push_child(&field, "name", SymbolKind::Method, parent, &mut out);
            }
        }
        out
    }

    fn structure_members(&self, body: &Node, parent: &str, is_class: bool) -> Vec<SymbolEntry> {
        let mut out = Vec::new();
        let mut cursor = body.walk();
        for member in body.named_children(&mut cursor) {
            match member.kind() {
                "field" => {
                    let kind = if is_class {
                        SymbolKind::Method
                    } else {
                        SymbolKind::Field
                    };
                    self.push_child(&member, "name", kind, parent, &mut out);
                }
                "ctor" => {
                    self.push_child(&member, "name", SymbolKind::Method, parent, &mut out);
                }
                _ => {}
            }
        }
        out
    }

    fn constructors(&self, body: &Node, parent: &str) -> Vec<SymbolEntry> {
        let mut out = Vec::new();
        let mut cursor = body.walk();
        for alt in body
            .named_children(&mut cursor)
            .filter(|c| c.kind() == "ctor_alt")
        {
            self.push_child(&alt, "name", SymbolKind::EnumVariant, parent, &mut out);
        }
        out
    }

    /// Pushes one child per name node found in `field` (a field declaration
    /// may bind several names). A node without a name yields no symbol.
    fn push_child(
        &self,
        node: &Node,
        field: &str,
        kind: SymbolKind,
        parent: &str,
        out: &mut Vec<SymbolEntry>,
    ) {
        let signature = first_line(node, self.source);
        let mut cursor = node.walk();
        let names: Vec<String> = node
            .children_by_field_name(field, &mut cursor)
            .filter(|n| n.is_named() && !n.is_missing())
            .map(|n| node_text(&n, self.source).to_string())
            .collect();
        for name in names {
            let qualified = qualify(parent, &name);
            let mut entry = build_entry(
                node,
                self.source,
                kind,
                name,
                qualified,
                signature.clone(),
                Vec::new(),
            );
            entry.parent = Some(parent.to_string());
            out.push(entry);
        }
    }
}

fn build_entry(
    range_node: &Node,
    source: &str,
    kind: SymbolKind,
    name: String,
    qualified_name: String,
    signature: String,
    children: Vec<SymbolEntry>,
) -> SymbolEntry {
    let text = node_text(range_node, source);
    SymbolEntry {
        kind,
        name,
        qualified_name,
        signature,
        start_line: range_node.start_position().row + 1,
        end_line: range_node.end_position().row + 1,
        start_byte: range_node.start_byte(),
        end_byte: range_node.end_byte(),
        body_start_line: None,
        body_end_line: None,
        parent: None,
        children,
        digest: SymbolDigest::new(text.as_bytes(), text.lines().count()),
    }
}

/// Text of the `type` term of an anonymous instance (e.g. `ToString Point`).
/// The `type` field also labels the `:` token, which is not named.
fn instance_type_text(body: &Node, source: &str) -> Option<String> {
    let mut cursor = body.walk();
    body.children_by_field_name("type", &mut cursor)
        .find(|n| n.is_named() && !n.is_missing())
        .map(|n| {
            node_text(&n, source)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|text| !text.is_empty())
}

/// Declaration header for the outline label: from the first child that is not
/// an attribute to the end of that line, so modifiers stay and `@[...]` goes.
fn header_signature(decl: &Node, source: &str) -> String {
    let mut cursor = decl.walk();
    let start = decl
        .children(&mut cursor)
        .find(|child| {
            !matches!(
                child.kind(),
                "attributes" | "line_comment" | "block_comment" | "doc_comment"
            )
        })
        .map(|child| child.start_byte())
        .unwrap_or_else(|| decl.start_byte());
    let end = decl.end_byte().max(start);
    safe_slice(source, start, end)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Splits a dotted Lean name into its components.
fn split_components(name: &str) -> Vec<String> {
    name.split('.')
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

fn qualify(prefix: &str, name: &str) -> String {
    // `_root_.Foo.bar` ignores the enclosing namespaces.
    if let Some(rest) = name.strip_prefix("_root_.") {
        return rest.to_string();
    }
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}.{name}")
    }
}

fn field_text(node: &Node, field: &str, source: &str) -> Option<String> {
    node.child_by_field_name(field)
        .filter(|n| !n.is_missing())
        .map(|n| node_text(&n, source).trim().to_string())
        .filter(|text| !text.is_empty())
}

fn first_line(node: &Node, source: &str) -> String {
    node_text(node, source)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

fn node_text<'a>(node: &Node, source: &'a str) -> &'a str {
    safe_slice(source, node.start_byte(), node.end_byte())
}

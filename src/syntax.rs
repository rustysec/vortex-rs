//! tree-sitter parsing and highlighting.
//!
//! Parsing happens on the blocking thread pool: the main loop hands over the
//! text, keeps drawing with the previous tree, and adopts the new tree when the
//! job reports back.  Because [`tree_sitter::Tree`] is `Send` but not `Sync`,
//! trees travel through a [`TreeSlot`] (a mutex-guarded hand-off) rather than
//! being shared.

use std::sync::{Arc, Mutex};

use tree_sitter::{Language, Node, Parser, Tree};

use crate::buffer::Buffer;
use crate::event::{AsyncEvent, EventTx};
use crate::selection::Span;

/// Hand-off box for a parsed tree.
#[derive(Clone, Default)]
pub struct TreeSlot(Arc<Mutex<Option<Tree>>>);

impl std::fmt::Debug for TreeSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TreeSlot")
            .field(
                "filled",
                &self.0.lock().map(|g| g.is_some()).unwrap_or(false),
            )
            .finish()
    }
}

impl TreeSlot {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }

    /// Called by the parsing task.
    pub fn store(&self, tree: Tree) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(tree);
        }
    }

    /// Called by the main loop to adopt the tree.
    pub fn take(&self) -> Option<Tree> {
        self.0.lock().ok().and_then(|mut slot| slot.take())
    }
}

/// What the last parse attempt produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParseStatus {
    /// Nothing to do: the tree matches the buffer.
    #[default]
    Current,
    /// A job is in flight.  Only a job may set this, and every job has to end
    /// with a result: nothing else will ever parse the buffer again while it
    /// says `Pending`.
    Pending,
    /// The tree is behind the buffer and no job is running, so the idle timer
    /// owes it a parse.
    Stale,
    /// No grammar for this language.
    Unsupported,
    /// Parsing failed.
    Failed,
}

impl ParseStatus {
    /// True while a parse job is in flight, which is the only reason to leave
    /// a stale buffer alone.
    pub fn is_pending(self) -> bool {
        self == Self::Pending
    }
}

/// Tree-sitter state belonging to one buffer.
#[derive(Debug, Default)]
pub struct SyntaxState {
    pub tree: Option<Tree>,
    /// Buffer version the tree was parsed from.
    pub tree_version: u64,
    pub status: ParseStatus,
    /// Set when the tree contains syntax errors.
    pub has_errors: bool,
}

impl SyntaxState {
    pub fn invalidate(&mut self) {
        self.tree_version = 0;
        self.status = ParseStatus::Stale;
    }

    pub fn root(&self) -> Option<Node<'_>> {
        self.tree.as_ref().map(Tree::root_node)
    }

    /// True when the tree reflects `version`.
    pub fn is_current(&self, version: u64) -> bool {
        self.tree.is_some() && self.tree_version == version
    }

    /// True when the buffer has changed since the tree was built and no parse
    /// is in flight, so the idle timer owes it one.
    ///
    /// The two halves belong together: a buffer whose status says `Pending`
    /// after its job has already reported back is waited on forever, and stops
    /// being highlighted at all.
    pub fn parse_due(&self, version: u64) -> bool {
        self.tree_version != version && !self.status.is_pending()
    }

    /// Clear everything (language changed).
    pub fn clear(&mut self) {
        self.tree = None;
        self.tree_version = 0;
        self.status = ParseStatus::Stale;
        self.has_errors = false;
    }
}

/// Parse a buffer on the current thread and store the result.
///
/// The app parses on the blocking pool; this is for tests and for the odd
/// synchronous query.
pub fn parse_in_place(buffer: &mut Buffer) -> bool {
    let Some(language) = buffer.language.grammar() else {
        buffer.syntax.tree = None;
        buffer.syntax.tree_version = buffer.version;
        buffer.syntax.status = ParseStatus::Unsupported;
        return false;
    };
    let text = buffer.text();
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        // Record the version anyway: the text is known not to parse, so
        // retrying it every idle tick would buy nothing.  The next edit bumps
        // the version and the buffer is interesting again.
        buffer.syntax.tree_version = buffer.version;
        buffer.syntax.status = ParseStatus::Failed;
        return false;
    }
    match parser.parse(&text, None) {
        Some(tree) => {
            let has_errors = tree.root_node().has_error();
            buffer.syntax.tree = Some(tree);
            buffer.syntax.tree_version = buffer.version;
            buffer.syntax.status = ParseStatus::Current;
            buffer.syntax.has_errors = has_errors;
            true
        }
        None => {
            buffer.syntax.tree_version = buffer.version;
            buffer.syntax.status = ParseStatus::Failed;
            false
        }
    }
}

/// Run a parse on the blocking pool and report through `tx`.
///
/// Every job answers exactly once, with a [`AsyncEvent::Parsed`] whose slot is
/// empty when the parse did not produce a tree.  A job that went silent would
/// leave the buffer waiting on a parse that never arrives, and it would never
/// be highlighted again.
pub fn spawn_parse(
    tx: EventTx,
    buffer: crate::buffer::BufferId,
    version: u64,
    language: Language,
    text: String,
    slot: TreeSlot,
) {
    tokio::task::spawn_blocking(move || {
        let mut parser = Parser::new();
        if parser.set_language(&language).is_err() {
            tx.send_async(AsyncEvent::Failed {
                what: format!("parse #{}", buffer.0),
                error: "grammar not available".into(),
            });
            tx.send_async(AsyncEvent::Parsed {
                buffer,
                version,
                slot,
            });
            return;
        }
        match parser.parse(&text, None) {
            Some(tree) => {
                slot.store(tree);
                tx.send_async(AsyncEvent::Parsed {
                    buffer,
                    version,
                    slot,
                });
            }
            None => {
                tx.send_async(AsyncEvent::Failed {
                    what: format!("parse #{}", buffer.0),
                    error: "parser could not parse this text".into(),
                });
                tx.send_async(AsyncEvent::Parsed {
                    buffer,
                    version,
                    slot,
                });
            }
        }
    });
}

/// Syntax highlight groups.  Kept small on purpose: styles come from the theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Hl {
    #[default]
    None,
    Keyword,
    String,
    Comment,
    Number,
    Constant,
    Function,
    Type,
    Property,
    Operator,
    Punctuation,
    Tag,
    Preproc,
    Error,
}

/// A highlighted byte range.
#[derive(Debug, Clone, Copy)]
pub struct Highlight {
    pub span: Span,
    pub hl: Hl,
}

/// Collect highlight runs for `[range]` of a tree that matches the buffer.
///
/// Only leaf tokens are emitted, so runs never overlap and the renderer can
/// paint them in order.
pub fn highlights(buffer: &Buffer, range: Span, lang_id: &str) -> Vec<Highlight> {
    let mut out = Vec::new();
    let Some(root) = buffer.syntax.root() else {
        return out;
    };
    let range = range.clamp(buffer.len_bytes());
    let mut cursor = root.walk();
    descend(buffer, &mut cursor, range, lang_id, &mut out);
    out
}

/// Split a buffer's text into per-line runs of highlight groups.
///
/// The picker file previews need the work done once per file rather than once
/// per frame, and they outlive the parse that produced them, so every run
/// carries owned text.  Tabs are expanded because the preview renderer counts
/// characters, not display cells; alignment across runs inside one line is
/// approximate, which a glance-sized pane can live with.
pub fn line_runs(buffer: &Buffer, lang_id: &str) -> Vec<Vec<(String, Hl)>> {
    let marks = highlights(buffer, Span::new(0, buffer.len_bytes()), lang_id);
    let mut out = Vec::with_capacity(buffer.line_count());
    // `highlights` emits leaf tokens in document order: sorted and disjoint, so
    // one cursor walks the whole file.  A mark that spans several lines (a
    // block comment) stays under the cursor until its last line is drawn.
    let mut mark = 0;
    for line in 0..buffer.line_count() {
        let start = buffer.line_start(line);
        let end = buffer.line_end(line);
        while mark < marks.len() && marks[mark].span.end <= start {
            mark += 1;
        }
        let text = buffer.slice(Span::new(start, end));
        let mut runs: Vec<(String, Hl)> = Vec::new();
        let mut at = start;
        while mark < marks.len() && marks[mark].span.start < end {
            let m = &marks[mark];
            let from = m.span.start.max(start);
            let to = m.span.end.min(end);
            if from > at {
                runs.push((expand_tabs(&text[at - start..from - start]), Hl::None));
            }
            if to > from {
                runs.push((expand_tabs(&text[from - start..to - start]), m.hl));
                at = to;
            }
            if m.span.end > end {
                break; // continues on the next line
            }
            mark += 1;
        }
        if at < end {
            runs.push((expand_tabs(&text[at - start..end - start]), Hl::None));
        }
        out.push(runs);
    }
    out
}

fn expand_tabs(text: &str) -> String {
    if text.contains('\t') {
        text.replace('\t', "    ")
    } else {
        text.to_string()
    }
}

fn descend(
    buffer: &Buffer,
    cursor: &mut tree_sitter::TreeCursor<'_>,
    range: Span,
    lang_id: &str,
    out: &mut Vec<Highlight>,
) {
    let node = cursor.node();
    let node_range = Span::new(node.start_byte(), node.end_byte());
    let field = cursor.field_name();

    // Whole subtree outside the visible window: stop walking.
    if node_range.end <= range.start || node_range.start >= range.end {
        return;
    }

    // A comment is one token even when the grammar exposes its `//` marker as a
    // child, so style the whole node and do not descend into it.  Without this
    // the marker is painted as punctuation and the comment text unstyled.
    //
    // Strings are the same in grammars that build them from parts (TOML's
    // `string` wraps its quotes and content as children): descending would
    // paint the quotes as punctuation and leave the content unstyled, so style
    // the whole node instead.
    //
    // YAML needs the same treatment: its quoted scalars have no content child,
    // a block scalar's body lives outside the tree, and every mapping key is a
    // `flow_node` with a `key` field wrapped around an ordinary scalar.  Styling
    // those nodes whole is the only way to tell a key from its value.
    if node.child_count() == 0
        || node.kind().contains("comment")
        || node.kind() == "string"
        || (lang_id == "yaml" && yaml_atom(node, field))
    {
        let hl = highlight_of(buffer, node, lang_id, field);
        if hl != Hl::None {
            let start = node_range.start.max(range.start);
            let end = node_range.end.min(range.end);
            if start < end {
                out.push(Highlight {
                    span: Span::new(start, end),
                    hl,
                });
            }
        }
        return;
    }

    if cursor.goto_first_child() {
        loop {
            descend(buffer, cursor, range, lang_id, out);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

/// Highlight group implied by the parent's field name for this node.
fn role_highlight(node: Node<'_>, field: Option<&str>) -> Option<Hl> {
    let parent_kind = node.parent().map(|parent| parent.kind().to_lowercase())?;
    match field? {
        // (call_expression function: (identifier)) and friends.
        "function" | "callee" => Some(Hl::Function),
        "name" | "definition" => {
            if parent_kind.contains("function") || parent_kind.contains("method") {
                Some(Hl::Function)
            } else if [
                "struct",
                "class",
                "enum",
                "trait",
                "interface",
                "union",
                "type",
            ]
            .iter()
            .any(|word| parent_kind.contains(word))
            {
                Some(Hl::Type)
            } else {
                None
            }
        }
        "type" | "return_type" | "argument_type" => Some(Hl::Type),
        // `key` is what the data grammars call the left-hand side of a pair:
        // `"name": 1`, `name = 1` and `name: 1` all come back with this field.
        // Without it a JSON key is just another string, which paints a config
        // file in one colour.
        "parameter" | "parameters" | "field" | "property" | "attribute" | "key" => {
            Some(Hl::Property)
        }
        _ => None,
    }
}

/// YAML nodes that must be styled as one token.
///
/// The grammar wraps every scalar in a `plain_scalar`/`*_quote_scalar` node and
/// leaves the interesting text out of the tree entirely: a quoted scalar's body
/// is not a child, and a block scalar's indented lines are not children either.
/// Descending would paint just the quotes and markers.  Keys are wrapped in a
/// `flow_node` carrying the `key` field, so they need the same whole-node
/// treatment to differ from the string that follows the colon.
fn yaml_atom(node: Node<'_>, field: Option<&str>) -> bool {
    if node.kind() == "flow_node" {
        return field == Some("key");
    }
    matches!(
        node.kind(),
        "plain_scalar"
            | "double_quote_scalar"
            | "single_quote_scalar"
            | "block_scalar"
            | "alias"
            | "anchor"
            | "tag"
            | "yaml_directive"
            | "tag_directive"
            | "reserved_directive"
            | "---"
            | "..."
    )
}

/// Highlight group for a whole YAML token.
fn yaml_highlight(node: Node<'_>, field: Option<&str>) -> Option<Hl> {
    if node.kind() == "flow_node" && field == Some("key") {
        return Some(Hl::Property);
    }
    let hl = match node.kind() {
        "plain_scalar" => match node.named_child(0).map(|child| child.kind()) {
            Some("integer_scalar" | "float_scalar") => Hl::Number,
            // A timestamp is a typed value, like `true` and `null`, not a word.
            Some("boolean_scalar" | "null_scalar" | "timestamp_scalar") => Hl::Constant,
            _ => Hl::String,
        },
        "double_quote_scalar" | "single_quote_scalar" | "block_scalar" => Hl::String,
        "anchor" | "alias" => Hl::Constant,
        "tag" => Hl::Type,
        "yaml_directive" | "tag_directive" | "reserved_directive" | "---" | "..." => Hl::Preproc,
        _ => return None,
    };
    Some(hl)
}

/// Decide the highlight group of a leaf token.
fn highlight_of(buffer: &Buffer, node: Node<'_>, lang_id: &str, field: Option<&str>) -> Hl {
    let kind = node.kind();

    if lang_id == "yaml"
        && let Some(hl) = yaml_highlight(node, field)
    {
        return hl;
    }

    // The role the parent gave this token is a better clue than the token's own
    // kind: an `identifier` is a function name, a parameter or a plain variable,
    // and only the parent knows which.
    if let Some(hl) = role_highlight(node, field) {
        return hl;
    }

    // TOML keys are `bare_key` tokens; every one is a property name, whether it
    // sits in a `pair`, a `dotted_key` or a `[table]` header.
    if lang_id == "toml" && kind == "bare_key" {
        return Hl::Property;
    }

    if kind == "ERROR" || kind == "MISSING" || kind.contains("error") || kind.contains("missing") {
        return Hl::Error;
    }
    if kind.contains("comment") {
        return Hl::Comment;
    }
    if kind.contains("preproc") || kind.contains("directive") || kind.contains("include_directive")
    {
        return Hl::Preproc;
    }
    if kind.contains("string")
        || kind.contains("char_literal")
        || (kind == "text" && lang_id == "yaml")
    {
        return Hl::String;
    }
    if kind.contains("number") || kind.contains("integer") || kind.contains("float") {
        return Hl::Number;
    }
    if kind.contains("boolean") || kind.contains("null") || kind.contains("none_literal") {
        return Hl::Constant;
    }
    if kind.contains("tag_name") || kind == "element_name" {
        return Hl::Tag;
    }
    if kind.contains("property_name") || kind.contains("attribute_name") {
        return Hl::Property;
    }
    if kind.contains("type") {
        return Hl::Type;
    }
    if kind.contains("escape_sequence") {
        return Hl::Constant;
    }

    // Keywords, constants and builtin types are single-word token kinds.
    if kind.chars().all(|c| c.is_alphabetic() || c == '_')
        && let Some(group) = keyword_group(lang_id, kind)
    {
        return group;
    }

    if kind.contains("operator") {
        return Hl::Operator;
    }
    if kind.chars().all(|c| !c.is_alphanumeric() && c != '_') && !kind.is_empty() {
        return Hl::Punctuation;
    }

    if is_identifier_kind(kind) {
        return identifier_role(buffer, node);
    }

    Hl::None
}

fn is_identifier_kind(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "name"
            | "variable_name"
            | "field_identifier"
            | "property_identifier"
            | "shouty_constant_identifier"
            | "constant_identifier"
            | "upper_snake_case_string"
            | "attribute"
            | "field"
    )
}

/// Give identifiers a role: constants, called functions, fields.
fn identifier_role(buffer: &Buffer, node: Node<'_>) -> Hl {
    let text = buffer.slice(Span::new(node.start_byte(), node.end_byte()));
    if text.len() > 1
        && text
            .chars()
            .all(|c| c.is_uppercase() || c.is_numeric() || c == '_')
    {
        return Hl::Constant;
    }
    let Some(parent) = node.parent() else {
        return Hl::None;
    };

    // Locate this node among its siblings to learn its field name and position.
    let mut position = None;
    let mut cursor = parent.walk();
    for (index, child) in parent.children(&mut cursor).enumerate() {
        if child.id() == node.id() {
            position = Some((index, parent.field_name_for_child(index as u32)));
            break;
        }
    }
    let (index, field) = position.unwrap_or((usize::MAX, None));
    let parent_kind = parent.kind();

    if (parent_kind.contains("call") || parent_kind.contains("invoke"))
        && (matches!(field, Some("function") | Some("callee")) || index == 0)
    {
        return Hl::Function;
    }
    if parent_kind.contains("assignment") && index == 0 {
        return Hl::Property;
    }
    if matches!(field, Some("property") | Some("field") | Some("attribute")) {
        return Hl::Property;
    }
    if (parent_kind.contains("declaration") || parent_kind.contains("definition"))
        && field == Some("name")
    {
        if parent_kind.contains("function")
            || parent_kind.contains("method")
            || parent_kind.contains("lambda")
            || parent_kind.contains("procedure")
        {
            return Hl::Function;
        }
        if parent_kind.contains("class")
            || parent_kind.contains("struct")
            || parent_kind.contains("enum")
            || parent_kind.contains("trait")
            || parent_kind.contains("impl")
            || parent_kind.contains("interface")
            || parent_kind.contains("type")
        {
            return Hl::Type;
        }
    }
    Hl::None
}

/// Keyword / type / constant tables per language, keyed by token kind.
fn keyword_group(lang_id: &str, token: &str) -> Option<Hl> {
    let set = match lang_id {
        "rust" => RUST,
        "python" => PYTHON,
        "javascript" | "typescript" | "tsx" => JAVASCRIPT,
        "go" => GO,
        "c" => C,
        "bash" => BASH,
        // JSON's `true` and `false` are their own token kinds, so they need the
        // same table the other data formats use; `null` is caught by kind.
        "json" | "toml" | "yaml" => DATA,
        _ => &[],
    };
    set.iter()
        .find(|(word, _)| *word == token)
        .map(|(_, group)| *group)
}

const RUST: &[(&str, Hl)] = &[
    ("as", Hl::Keyword),
    ("async", Hl::Keyword),
    ("await", Hl::Keyword),
    ("break", Hl::Keyword),
    ("const", Hl::Keyword),
    ("continue", Hl::Keyword),
    ("crate", Hl::Keyword),
    ("dyn", Hl::Keyword),
    ("else", Hl::Keyword),
    ("enum", Hl::Keyword),
    ("extern", Hl::Keyword),
    ("false", Hl::Constant),
    ("fn", Hl::Keyword),
    ("for", Hl::Keyword),
    ("if", Hl::Keyword),
    ("impl", Hl::Keyword),
    ("in", Hl::Keyword),
    ("let", Hl::Keyword),
    ("loop", Hl::Keyword),
    ("match", Hl::Keyword),
    ("mod", Hl::Keyword),
    ("move", Hl::Keyword),
    ("mut", Hl::Keyword),
    ("pub", Hl::Keyword),
    ("ref", Hl::Keyword),
    ("return", Hl::Keyword),
    ("self", Hl::Constant),
    ("Self", Hl::Type),
    ("static", Hl::Keyword),
    ("struct", Hl::Keyword),
    ("super", Hl::Constant),
    ("trait", Hl::Keyword),
    ("true", Hl::Constant),
    ("type", Hl::Keyword),
    ("union", Hl::Keyword),
    ("unsafe", Hl::Keyword),
    ("use", Hl::Keyword),
    ("where", Hl::Keyword),
    ("while", Hl::Keyword),
    ("u8", Hl::Type),
    ("u16", Hl::Type),
    ("u32", Hl::Type),
    ("u64", Hl::Type),
    ("u128", Hl::Type),
    ("i8", Hl::Type),
    ("i16", Hl::Type),
    ("i32", Hl::Type),
    ("i64", Hl::Type),
    ("i128", Hl::Type),
    ("f32", Hl::Type),
    ("f64", Hl::Type),
    ("usize", Hl::Type),
    ("isize", Hl::Type),
    ("bool", Hl::Type),
    ("char", Hl::Type),
    ("str", Hl::Type),
    ("String", Hl::Type),
    ("Vec", Hl::Type),
    ("Option", Hl::Type),
    ("Result", Hl::Type),
    ("Box", Hl::Type),
    ("Rc", Hl::Type),
    ("Arc", Hl::Type),
    ("Some", Hl::Constant),
    ("None", Hl::Constant),
    ("Ok", Hl::Constant),
    ("Err", Hl::Constant),
];

const PYTHON: &[(&str, Hl)] = &[
    ("and", Hl::Keyword),
    ("as", Hl::Keyword),
    ("assert", Hl::Keyword),
    ("async", Hl::Keyword),
    ("await", Hl::Keyword),
    ("break", Hl::Keyword),
    ("class", Hl::Keyword),
    ("continue", Hl::Keyword),
    ("def", Hl::Keyword),
    ("del", Hl::Keyword),
    ("elif", Hl::Keyword),
    ("else", Hl::Keyword),
    ("except", Hl::Keyword),
    ("finally", Hl::Keyword),
    ("for", Hl::Keyword),
    ("from", Hl::Keyword),
    ("global", Hl::Keyword),
    ("if", Hl::Keyword),
    ("import", Hl::Keyword),
    ("in", Hl::Keyword),
    ("is", Hl::Keyword),
    ("lambda", Hl::Keyword),
    ("None", Hl::Constant),
    ("nonlocal", Hl::Keyword),
    ("not", Hl::Keyword),
    ("or", Hl::Keyword),
    ("pass", Hl::Keyword),
    ("raise", Hl::Keyword),
    ("return", Hl::Keyword),
    ("self", Hl::Constant),
    ("True", Hl::Constant),
    ("False", Hl::Constant),
    ("try", Hl::Keyword),
    ("while", Hl::Keyword),
    ("with", Hl::Keyword),
    ("yield", Hl::Keyword),
    ("int", Hl::Type),
    ("str", Hl::Type),
    ("float", Hl::Type),
    ("bool", Hl::Type),
    ("bytes", Hl::Type),
    ("list", Hl::Type),
    ("dict", Hl::Type),
    ("set", Hl::Type),
    ("tuple", Hl::Type),
];

const JAVASCRIPT: &[(&str, Hl)] = &[
    ("async", Hl::Keyword),
    ("await", Hl::Keyword),
    ("break", Hl::Keyword),
    ("case", Hl::Keyword),
    ("catch", Hl::Keyword),
    ("class", Hl::Keyword),
    ("const", Hl::Keyword),
    ("continue", Hl::Keyword),
    ("debugger", Hl::Keyword),
    ("default", Hl::Keyword),
    ("delete", Hl::Keyword),
    ("do", Hl::Keyword),
    ("else", Hl::Keyword),
    ("enum", Hl::Keyword),
    ("export", Hl::Keyword),
    ("extends", Hl::Keyword),
    ("false", Hl::Constant),
    ("finally", Hl::Keyword),
    ("for", Hl::Keyword),
    ("function", Hl::Keyword),
    ("if", Hl::Keyword),
    ("implements", Hl::Keyword),
    ("import", Hl::Keyword),
    ("in", Hl::Keyword),
    ("instanceof", Hl::Keyword),
    ("interface", Hl::Keyword),
    ("let", Hl::Keyword),
    ("new", Hl::Keyword),
    ("null", Hl::Constant),
    ("of", Hl::Keyword),
    ("package", Hl::Keyword),
    ("private", Hl::Keyword),
    ("protected", Hl::Keyword),
    ("public", Hl::Keyword),
    ("return", Hl::Keyword),
    ("static", Hl::Keyword),
    ("super", Hl::Constant),
    ("switch", Hl::Keyword),
    ("this", Hl::Constant),
    ("throw", Hl::Keyword),
    ("true", Hl::Constant),
    ("try", Hl::Keyword),
    ("type", Hl::Keyword),
    ("typeof", Hl::Keyword),
    ("undefined", Hl::Constant),
    ("var", Hl::Keyword),
    ("void", Hl::Keyword),
    ("while", Hl::Keyword),
    ("with", Hl::Keyword),
    ("yield", Hl::Keyword),
    ("string", Hl::Type),
    ("number", Hl::Type),
    ("boolean", Hl::Type),
    ("any", Hl::Type),
];

const GO: &[(&str, Hl)] = &[
    ("break", Hl::Keyword),
    ("case", Hl::Keyword),
    ("chan", Hl::Keyword),
    ("const", Hl::Keyword),
    ("continue", Hl::Keyword),
    ("default", Hl::Keyword),
    ("defer", Hl::Keyword),
    ("else", Hl::Keyword),
    ("fallthrough", Hl::Keyword),
    ("for", Hl::Keyword),
    ("func", Hl::Keyword),
    ("go", Hl::Keyword),
    ("if", Hl::Keyword),
    ("import", Hl::Keyword),
    ("interface", Hl::Keyword),
    ("map", Hl::Keyword),
    ("nil", Hl::Constant),
    ("package", Hl::Keyword),
    ("range", Hl::Keyword),
    ("return", Hl::Keyword),
    ("select", Hl::Keyword),
    ("struct", Hl::Keyword),
    ("switch", Hl::Keyword),
    ("type", Hl::Keyword),
    ("var", Hl::Keyword),
    ("true", Hl::Constant),
    ("false", Hl::Constant),
    ("bool", Hl::Type),
    ("byte", Hl::Type),
    ("error", Hl::Type),
    ("float32", Hl::Type),
    ("float64", Hl::Type),
    ("int", Hl::Type),
    ("int8", Hl::Type),
    ("int16", Hl::Type),
    ("int32", Hl::Type),
    ("int64", Hl::Type),
    ("rune", Hl::Type),
    ("string", Hl::Type),
    ("uint", Hl::Type),
    ("uint8", Hl::Type),
    ("uint16", Hl::Type),
    ("uint32", Hl::Type),
    ("uint64", Hl::Type),
    ("uintptr", Hl::Type),
];

const C: &[(&str, Hl)] = &[
    ("auto", Hl::Keyword),
    ("break", Hl::Keyword),
    ("case", Hl::Keyword),
    ("char", Hl::Type),
    ("const", Hl::Keyword),
    ("continue", Hl::Keyword),
    ("default", Hl::Keyword),
    ("do", Hl::Keyword),
    ("double", Hl::Type),
    ("else", Hl::Keyword),
    ("enum", Hl::Keyword),
    ("extern", Hl::Keyword),
    ("float", Hl::Type),
    ("for", Hl::Keyword),
    ("goto", Hl::Keyword),
    ("if", Hl::Keyword),
    ("inline", Hl::Keyword),
    ("int", Hl::Type),
    ("long", Hl::Type),
    ("register", Hl::Keyword),
    ("restrict", Hl::Keyword),
    ("return", Hl::Keyword),
    ("short", Hl::Type),
    ("signed", Hl::Type),
    ("sizeof", Hl::Keyword),
    ("static", Hl::Keyword),
    ("struct", Hl::Keyword),
    ("switch", Hl::Keyword),
    ("typedef", Hl::Keyword),
    ("union", Hl::Keyword),
    ("unsigned", Hl::Type),
    ("void", Hl::Type),
    ("volatile", Hl::Keyword),
    ("while", Hl::Keyword),
    ("NULL", Hl::Constant),
    ("true", Hl::Constant),
    ("false", Hl::Constant),
];

const BASH: &[(&str, Hl)] = &[
    ("if", Hl::Keyword),
    ("then", Hl::Keyword),
    ("else", Hl::Keyword),
    ("elif", Hl::Keyword),
    ("fi", Hl::Keyword),
    ("for", Hl::Keyword),
    ("while", Hl::Keyword),
    ("until", Hl::Keyword),
    ("do", Hl::Keyword),
    ("done", Hl::Keyword),
    ("case", Hl::Keyword),
    ("esac", Hl::Keyword),
    ("function", Hl::Keyword),
    ("in", Hl::Keyword),
    ("return", Hl::Keyword),
    ("exit", Hl::Keyword),
    ("local", Hl::Keyword),
    ("export", Hl::Keyword),
    ("source", Hl::Keyword),
    ("break", Hl::Keyword),
    ("continue", Hl::Keyword),
    ("true", Hl::Constant),
    ("false", Hl::Constant),
];

const DATA: &[(&str, Hl)] = &[
    ("true", Hl::Constant),
    ("false", Hl::Constant),
    ("null", Hl::Constant),
    ("yes", Hl::Constant),
    ("no", Hl::Constant),
    ("on", Hl::Constant),
    ("off", Hl::Constant),
];

// ------------------------------------------------------------- text objects

/// Tree-sitter backed text objects (`if` function, `ic` class, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TsObject {
    Function,
    Class,
    Block,
    Comment,
    StringLiteral,
}

impl TsObject {
    /// Key used after `i`/`a`: `if`, `ic`, `ib`, `iC`, `is`.
    pub fn from_key(key: char) -> Option<Self> {
        match key {
            'f' => Some(TsObject::Function),
            'c' => Some(TsObject::Class),
            'b' => Some(TsObject::Block),
            'C' => Some(TsObject::Comment),
            's' => Some(TsObject::StringLiteral),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TsObject::Function => "function",
            TsObject::Class => "class",
            TsObject::Block => "block",
            TsObject::Comment => "comment",
            TsObject::StringLiteral => "string",
        }
    }

    fn matches(self, kind: &str) -> bool {
        match self {
            TsObject::Function => {
                kind.contains("function")
                    || kind.contains("method")
                    || kind.contains("lambda")
                    || kind.contains("closure")
                    || kind.contains("procedure")
            }
            TsObject::Class => {
                kind.contains("class")
                    || kind.contains("struct")
                    || kind.contains("enum")
                    || kind.contains("trait")
                    || kind.contains("impl")
                    || kind.contains("interface")
                    || kind.contains("module")
                    || kind.contains("type_declaration")
            }
            TsObject::Block => {
                kind.ends_with("block")
                    || kind.contains("statement_block")
                    || kind.ends_with("_body")
            }
            TsObject::Comment => kind.contains("comment"),
            TsObject::StringLiteral => kind.contains("string"),
        }
    }
}

/// Every tree-sitter text object around `byte`, innermost first.
pub fn text_objects(buffer: &Buffer, byte: usize, object: TsObject) -> Vec<(Span, Span)> {
    let mut out: Vec<(Span, Span)> = Vec::new();
    let Some(root) = buffer.syntax.root() else {
        return out;
    };
    let byte = buffer.clamp_byte(byte);
    let Some(node) = node_at_offset(root, byte) else {
        return out;
    };
    let mut current = Some(node);
    while let Some(node) = current {
        if object.matches(node.kind()) {
            let outer = Span::new(node.start_byte(), node.end_byte());
            let inner = inner_span(node).unwrap_or(outer);
            if !outer.is_empty() && out.last().is_none_or(|(last, _)| *last != outer) {
                out.push((outer, inner));
            }
        }
        current = node.parent();
    }
    out
}

/// Does the grammar say a block opens at `at`?
///
/// Used by the indenter for blocks that do not start with a bracket — Python's
/// `def f():`, Ruby's `do`, …  The caret sits at the end of the header line, so
/// the block node that contains it starts *after* the caret.  A stale tree is
/// ignored: guessing from an old parse is worse than the text-only fallback.
pub fn opens_block_at(buffer: &Buffer, at: usize) -> bool {
    if !buffer.syntax.is_current(buffer.version) {
        return false;
    }
    let Some(root) = buffer.syntax.root() else {
        return false;
    };
    let Some(node) = node_at_offset(root, at) else {
        return false;
    };
    let mut current = Some(node);
    while let Some(node) = current {
        if is_block_kind(node.kind()) && node.start_byte() >= at && node.start_byte() > 0 {
            return true;
        }
        current = node.parent();
    }
    false
}

/// The indentation of the line that opens the block containing `at`.
///
/// A closing bracket belongs at the level of the line that opened its block, so
/// this is what the indenter dedents to.  `None` when the tree is stale or the
/// caret is not inside a block.
pub fn block_indent_at(buffer: &Buffer, at: usize) -> Option<String> {
    if !buffer.syntax.is_current(buffer.version) {
        return None;
    }
    let root = buffer.syntax.root()?;
    let node = node_at_offset(root, at)?;
    let mut current = Some(node);
    while let Some(node) = current {
        if is_block_kind(node.kind()) {
            let opener = node.parent().unwrap_or(node);
            let line = buffer.line_of(opener.start_byte());
            return Some(buffer.indent_str(line));
        }
        current = node.parent();
    }
    None
}

/// Node kinds that represent a block body.
fn is_block_kind(kind: &str) -> bool {
    kind.ends_with("block") || kind.contains("statement_block") || kind.ends_with("_body")
}

/// Smallest named node containing `byte`.
pub fn node_at_offset<'t>(root: Node<'t>, byte: usize) -> Option<Node<'t>> {
    let mut cursor = root.walk();
    let mut best = root;
    loop {
        let mut descended = false;
        if cursor.goto_first_child() {
            loop {
                let node = cursor.node();
                if node.start_byte() <= byte
                    && node.end_byte() >= byte
                    && node.end_byte() > node.start_byte()
                {
                    best = node;
                    descended = true;
                    break;
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        if !descended {
            break;
        }
    }
    Some(best)
}

/// The inside of a node: its body, or the text between its delimiters.
fn inner_span(node: Node<'_>) -> Option<Span> {
    for field in ["body", "content", "value", "compound_expression"] {
        if let Some(child) = node.child_by_field_name(field) {
            let span = Span::new(child.start_byte(), child.end_byte());
            if !span.is_empty() {
                return Some(span);
            }
        }
    }
    // Braced bodies: `fn f() { … }` → the `{ … }` child.
    for child in node.children(&mut node.walk()) {
        let kind = child.kind();
        if kind.ends_with("block") || kind.contains("body") || kind.contains("arguments") {
            let span = Span::new(child.start_byte(), child.end_byte());
            if !span.is_empty() {
                return Some(span);
            }
        }
    }
    None
}

/// Comment and string nodes overlapping `[range]` — the spell checker's input.
pub fn comment_and_string_spans(buffer: &Buffer, range: Span) -> Vec<Span> {
    let mut out = Vec::new();
    let Some(root) = buffer.syntax.root() else {
        return out;
    };
    let range = range.clamp(buffer.len_bytes());
    collect_spans(root, range, &mut out);
    out
}

/// Is `at` inside a string literal or a comment?
///
/// Used to keep auto-pairing from inserting a partner quote inside a string
/// that is already open.  A stale tree answers `false`: the text-only fallback
/// is better than a guess from an old parse.
pub fn in_string_or_comment(buffer: &Buffer, at: usize) -> bool {
    if !buffer.syntax.is_current(buffer.version) {
        return false;
    }
    let Some(root) = buffer.syntax.root() else {
        return false;
    };
    let Some(node) = node_at_offset(root, at) else {
        return false;
    };
    let mut current = Some(node);
    while let Some(node) = current {
        let kind = node.kind();
        if kind.contains("comment") || kind.contains("string") {
            return true;
        }
        current = node.parent();
    }
    false
}

fn collect_spans(node: Node<'_>, range: Span, out: &mut Vec<Span>) {
    let node_range = Span::new(node.start_byte(), node.end_byte());
    if node_range.end <= range.start || node_range.start >= range.end {
        return;
    }
    let kind = node.kind();
    if kind.contains("comment") || kind.contains("string") {
        out.push(node_range);
        return; // no need to descend
    }
    for child in node.children(&mut node.walk()) {
        collect_spans(child, range, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{Buffer, BufferId};
    use crate::config::EditorConfig;

    fn rust(text: &str) -> Buffer {
        let mut buffer = Buffer::new(
            BufferId(1),
            Some(std::path::PathBuf::from("sample.rs")),
            text.to_string(),
            &EditorConfig::default(),
        );
        assert!(buffer.reparse_now(), "the Rust grammar should parse");
        buffer
    }

    fn yaml(text: &str) -> Buffer {
        let mut buffer = Buffer::new(
            BufferId(1),
            Some(std::path::PathBuf::from("sample.yaml")),
            text.to_string(),
            &EditorConfig::default(),
        );
        assert!(buffer.reparse_now(), "the YAML grammar should parse");
        buffer
    }

    const YAML_SAMPLE: &str = concat!(
        "%YAML 1.2\n---\n",
        "name: vortex\ncount: 12\nratio: 1.5\n",
        "enabled: true\nnothing: null\nwhen: 2024-01-02\n",
        "note: \"hello world\"\nsingle: 'it''s'\n",
        "block: |\n  line one\n  line two\n",
        "base: &base value\ncopy: *base\ntagged: !!str text\n",
        "flow: {a: 1, b: [2, 3]}\n",
        "# a note\n",
    );

    fn groups(buffer: &Buffer) -> Vec<(Hl, String)> {
        let end = buffer.len_bytes();
        highlights(buffer, Span::new(0, end), buffer.language.id)
            .into_iter()
            .map(|highlight| {
                (
                    highlight.hl,
                    buffer.slice(highlight.span).trim().to_string(),
                )
            })
            .collect()
    }

    #[test]
    fn line_runs_split_the_highlights_per_line() {
        // The picker preview needs one row of runs per file line, and a run
        // that spans several lines (a block comment) must repeat on each.
        let buffer = rust("fn main() {\n    /* a\n       note */\n}\n");
        let runs = line_runs(&buffer, buffer.language.id);
        // Four text lines plus the empty one after the final newline.
        assert_eq!(runs.len(), 5, "{runs:?}");
        // Every line's runs concatenate back to the line itself.
        let joined: Vec<String> = runs
            .iter()
            .map(|line| {
                line.iter()
                    .map(|(text, _)| text.as_str())
                    .collect::<String>()
            })
            .collect();
        assert_eq!(joined[0], "fn main() {");
        assert_eq!(joined[1], "    /* a");
        assert_eq!(joined[3], "}");
        // The keyword on line one and the comment fragment on line two keep
        // their groups.
        assert!(
            runs[0]
                .iter()
                .any(|(t, hl)| t == "fn" && *hl == Hl::Keyword)
        );
        assert!(
            runs[1]
                .iter()
                .any(|(t, hl)| t.contains("/* a") && *hl == Hl::Comment),
            "{:?}",
            runs[1]
        );
        assert!(
            runs[2]
                .iter()
                .any(|(t, hl)| t.contains("note */") && *hl == Hl::Comment),
            "{:?}",
            runs[2]
        );
    }

    #[test]
    fn line_runs_expand_tabs() {
        // Plain text has no grammar, so every byte is one unstyled run — but a
        // tab still must not reach the renderer, which counts characters.
        let buffer = Buffer::new(
            BufferId(1),
            Some(std::path::PathBuf::from("sample.txt")),
            "a\tb".to_string(),
            &EditorConfig::default(),
        );
        let runs = line_runs(&buffer, buffer.language.id);
        assert_eq!(runs[0][0].0, "a    b");
    }

    #[test]
    fn comments_are_highlighted_as_one_run() {
        // The grammar hands back `//` as a child of the comment, which used to
        // leave the comment body unstyled.
        let buffer = rust("fn main() {\n    // a note\n}\n");
        let groups = groups(&buffer);
        assert!(
            groups
                .iter()
                .any(|(hl, text)| *hl == Hl::Comment && text.contains("a note")),
            "no comment highlight in {groups:?}"
        );
    }

    #[test]
    fn strings_numbers_and_keywords_are_distinguished() {
        let buffer = rust(
            "struct Point { x: u32 }\nfn area(p: &Point) -> u32 { p.x * 2 }\nlet s = \"hi\";\n",
        );
        let groups = groups(&buffer);
        assert!(
            groups.iter().any(|(hl, _)| *hl == Hl::Keyword),
            "{groups:?}"
        );
        // A function's own name is an ordinary identifier; only the parent's
        // `name:` field says what it is.
        assert!(
            groups
                .iter()
                .any(|(hl, text)| *hl == Hl::Function && text == "area"),
            "{groups:?}"
        );
        assert!(groups.iter().any(|(hl, _)| *hl == Hl::Type), "{groups:?}");
        assert!(groups.iter().any(|(hl, _)| *hl == Hl::Number), "{groups:?}");
        assert!(groups.iter().any(|(hl, _)| *hl == Hl::String), "{groups:?}");
    }

    #[test]
    fn nothing_is_highlighted_outside_the_window() {
        let buffer = rust("fn a() {}\nfn b() {}\n");
        let end = buffer.len_bytes();
        let second = highlights(&buffer, Span::new(11, end), buffer.language.id);
        assert!(
            second.iter().all(|highlight| highlight.span.start >= 11),
            "{second:?}"
        );
    }

    #[test]
    fn json_keys_and_literals_are_highlighted() {
        // JSON used to paint almost nothing: a key is a `string` token, so it
        // came out the same green as every value, and `true`/`false` are their
        // own token kinds that no keyword table covered.  A config file then
        // read as one colour.
        let mut buffer = Buffer::new(
            BufferId(1),
            Some(std::path::PathBuf::from("sample.json")),
            "{\"name\": \"vortex\", \"count\": 12, \"on\": true, \"off\": false, \"none\": null}\n"
                .to_string(),
            &EditorConfig::default(),
        );
        assert!(buffer.reparse_now(), "the JSON grammar should parse");
        let groups = groups(&buffer);

        for key in ["\"name\"", "\"count\"", "\"on\"", "\"off\"", "\"none\""] {
            assert!(
                groups
                    .iter()
                    .any(|(hl, text)| *hl == Hl::Property && text == key),
                "key {key:?} should be a property in {groups:?}"
            );
        }
        // A value is still a string, so a key and its text differ on screen.
        assert!(
            groups
                .iter()
                .any(|(hl, text)| *hl == Hl::String && text == "\"vortex\""),
            "{groups:?}"
        );
        assert!(groups.iter().any(|(hl, _)| *hl == Hl::Number), "{groups:?}");
        for literal in ["true", "false", "null"] {
            assert!(
                groups
                    .iter()
                    .any(|(hl, text)| *hl == Hl::Constant && text == literal),
                "{literal} should be a constant in {groups:?}"
            );
        }
    }

    #[test]
    fn toml_keys_strings_and_tables_are_highlighted() {
        let mut buffer = Buffer::new(
            BufferId(1),
            Some(std::path::PathBuf::from("sample.toml")),
            "[editor]\ntab-width = 4\nexpand-tab = true\nname = \"hello\"\n# a comment\n"
                .to_string(),
            &EditorConfig::default(),
        );
        assert!(buffer.reparse_now(), "the TOML grammar should parse");
        let groups = groups(&buffer);

        // Keys are properties, whether in a pair or a `[table]` header.
        for key in ["editor", "tab-width", "expand-tab", "name"] {
            assert!(
                groups
                    .iter()
                    .any(|(hl, text)| *hl == Hl::Property && text == key),
                "key {key:?} should be a property in {groups:?}"
            );
        }
        // A string is one run including its quotes, not just punctuation.
        assert!(
            groups
                .iter()
                .any(|(hl, text)| *hl == Hl::String && text == "\"hello\""),
            "{groups:?}"
        );
        assert!(groups.iter().any(|(hl, _)| *hl == Hl::Number), "{groups:?}");
        assert!(
            groups
                .iter()
                .any(|(hl, text)| *hl == Hl::Constant && text == "true"),
            "{groups:?}"
        );
        assert!(
            groups.iter().any(|(hl, _)| *hl == Hl::Comment),
            "{groups:?}"
        );
    }

    #[test]
    fn yaml_keys_are_properties_not_strings() {
        // A YAML key is a `string_scalar` wrapped in a `flow_node` with a `key`
        // field.  Without that field every key came out green like its value,
        // so a whole config file read as one colour.
        let groups = groups(&yaml(YAML_SAMPLE));

        for key in [
            "name", "count", "ratio", "enabled", "nothing", "when", "note", "single", "block",
            "base", "copy", "tagged", "flow", "a", "b",
        ] {
            assert!(
                groups
                    .iter()
                    .any(|(hl, text)| *hl == Hl::Property && text == key),
                "key {key:?} should be a property in {groups:?}"
            );
        }
        assert!(
            groups
                .iter()
                .any(|(hl, text)| *hl == Hl::String && text == "vortex"),
            "a value is still a string in {groups:?}"
        );
        assert!(
            !groups
                .iter()
                .any(|(hl, text)| *hl == Hl::String && text == "name"),
            "a key must not be painted like its value: {groups:?}"
        );
    }

    #[test]
    fn yaml_scalars_and_directives_are_whole_tokens() {
        // Quoted and block scalars have no content child in the grammar, and
        // directives have no token for the `%YAML` prefix.  Styling each leaf
        // left just quotes, escapes and the `|` marker coloured.
        let groups = groups(&yaml(YAML_SAMPLE));

        for text in ["\"hello world\"", "'it''s'"] {
            assert!(
                groups
                    .iter()
                    .any(|(hl, got)| *hl == Hl::String && got == text),
                "quoted scalar {text:?} should be one string run in {groups:?}"
            );
        }
        assert!(
            groups
                .iter()
                .any(|(hl, text)| *hl == Hl::String && text.contains("line one")),
            "the block scalar body should be a string in {groups:?}"
        );
        for number in ["12", "1.5", "1", "2", "3"] {
            assert!(
                groups
                    .iter()
                    .any(|(hl, text)| *hl == Hl::Number && text == number),
                "{number:?} should be a number in {groups:?}"
            );
        }
        for literal in ["true", "null", "&base", "*base"] {
            assert!(
                groups
                    .iter()
                    .any(|(hl, text)| *hl == Hl::Constant && text == literal),
                "{literal:?} should be a constant in {groups:?}"
            );
        }
        for (hl, text) in [
            (Hl::Type, "!!str"),
            (Hl::Preproc, "%YAML 1.2"),
            (Hl::Preproc, "---"),
            (Hl::Comment, "# a note"),
        ] {
            assert!(
                groups
                    .iter()
                    .any(|(got_hl, got)| *got_hl == hl && got == text),
                "{text:?} should be {hl:?} in {groups:?}"
            );
        }
        assert!(
            !groups.iter().any(|(_, text)| text == "\"" || text == "'"),
            "quotes must not be their own punctuation runs: {groups:?}"
        );
    }
}

//! Keel Language Server — speaks LSP over stdio.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use spar::ast::{FuncStmt, SparType, Program, TopLevelItem};
use spar::{SparError, Span};
use spar::evaluator::{EvalResult, Evaluator};
use spar::formatter::{format_program, format_source, FormatConfig};
use spar::lexer::Lexer;
use spar::loader::{ImportLoader, collect_imports, expand_imports, validate_schema_imports};
use spar::parser::Parser;
use spar::resolver::{FunctionEntry, GlobalEntry, Resolver, SectionEntry, SymbolTable};
use spar::typechecker::TypeChecker;

use tokio::sync::Mutex;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

// ── Type display helper ───────────────────────────────────────────────────────

fn format_spar_type(ty: &SparType) -> String {
    match ty {
        SparType::Str         => "str".to_string(),
        SparType::Int         => "int".to_string(),
        SparType::Float       => "float".to_string(),
        SparType::Bool        => "bool".to_string(),
        SparType::List(inner) => format!("[{}]", format_spar_type(inner)),
        SparType::Section     => "section".to_string(),
    }
}

// ── Inferred-type resolution ────────────────────────────────────────────────
//
// A field under a `-> TypeName` binding can omit its own type — that's not
// a gap in what's known, just in what's written. The real type always
// traces back through the binding chain, so hover/completion should show
// it directly instead of a vague "inferred" placeholder.

/// Walk from `path`'s top-level section's own `-> Type` binding down
/// through nested `TypeFieldShape::Section`/`Named` entries matching each
/// remaining path segment, to find the `TypeField` shape governing
/// whatever's declared at `path`. `None` if nothing in the chain is
/// type-bound (an unbound section's fields are always explicitly typed,
/// so this only matters for the `ty: None` case in the first place).
fn resolve_shape_for_path(symbols: &SymbolTable, path: &[String]) -> Option<Vec<spar::ast::TypeField>> {
    let top = symbols.sections.get(&vec![path.first()?.clone()])?;
    let type_name = top.type_binding.as_ref()?;
    let mut fields = symbols.types.get(type_name)?.fields.clone();
    for seg in &path[1..] {
        let tf = fields.iter().find(|f| &f.name == seg)?;
        fields = match &tf.shape {
            spar::ast::TypeFieldShape::Section(nested) => nested.clone(),
            spar::ast::TypeFieldShape::Named(other) => symbols.types.get(other)?.fields.clone(),
            spar::ast::TypeFieldShape::Primitive(_) => return None,
        };
    }
    Some(fields)
}

fn format_type_field_shape(shape: &spar::ast::TypeFieldShape) -> String {
    match shape {
        spar::ast::TypeFieldShape::Primitive(ty) => format_spar_type(ty),
        spar::ast::TypeFieldShape::Section(_)     => "section".to_string(),
        // A Named shape's own type name is more useful than a bare
        // "section" — e.g. "PostgresType" tells the reader where to look.
        spar::ast::TypeFieldShape::Named(name)    => name.clone(),
    }
}

/// The display string for a field whose `FieldEntry.ty` is `None` —
/// resolves the real type from the binding chain. Falls back to
/// "section" only if nothing in the chain can be traced (shouldn't
/// normally happen for valid code, since `ty: None` only parses under a
/// binding in the first place).
fn resolve_field_type_display(symbols: &SymbolTable, path: &[String], field_name: &str) -> String {
    resolve_shape_for_path(symbols, path)
        .and_then(|fields| fields.iter().find(|f| f.name == field_name).map(|tf| format_type_field_shape(&tf.shape)))
        .unwrap_or_else(|| "section".to_string())
}

// ── Error helpers ─────────────────────────────────────────────────────────────

pub fn error_span(e: &SparError) -> &Span {
    match e {
        SparError::LexError { span, .. }     => span,
        SparError::ParseError { span, .. }   => span,
        SparError::ResolveError { span, .. } => span,
        SparError::TypeError { span, .. }    => span,
        SparError::EvalError { span, .. }    => span,
        SparError::SchemaError { span, .. }  => span,
    }
}

pub fn error_message(e: &SparError) -> &str {
    match e {
        SparError::LexError { message, .. }     => message,
        SparError::ParseError { message, .. }   => message,
        SparError::ResolveError { message, .. } => message,
        SparError::TypeError { message, .. }    => message,
        SparError::EvalError { message, .. }    => message,
        SparError::SchemaError { message, .. }  => message,
    }
}

pub fn error_hint(e: &SparError) -> Option<&str> {
    match e {
        SparError::ResolveError { hint, .. } => hint.as_deref(),
        SparError::TypeError { hint, .. }    => hint.as_deref(),
        _ => None,
    }
}

/// Convert one SparError to an LSP Diagnostic.
///
/// Span indexing: spar uses 1-indexed line/col; LSP uses 0-indexed.
/// Conversion: `Position { line: span.line - 1, character: span.col - 1 }`.
pub fn spar_error_to_diagnostic(e: &SparError) -> Diagnostic {
    let span    = error_span(e);
    let message = error_message(e);
    let hint    = error_hint(e);

    let full_message = match hint {
        Some(h) => format!("{}\nHint: {}", message, h),
        None    => message.to_string(),
    };

    let start_line = span.line.saturating_sub(1);
    let start_char = span.col.saturating_sub(1);
    let span_len   = span.end.saturating_sub(span.start) as u32;

    let start = Position { line: start_line, character: start_char };
    let end   = Position { line: start_line, character: start_char + span_len.max(1) };

    // EvalErrors are potential runtime issues rather than definite mistakes;
    // show as warnings so they don't block saving in strict editors.
    let severity = match e {
        SparError::EvalError { .. } => DiagnosticSeverity::WARNING,
        _                         => DiagnosticSeverity::ERROR,
    };

    Diagnostic {
        range:    Range { start, end },
        severity: Some(severity),
        message:  full_message,
        source:   Some("spar".to_string()),
        ..Default::default()
    }
}

// ── Text utilities ────────────────────────────────────────────────────────────

/// Return the identifier (alphanumeric + `_`) at a zero-indexed LSP Position.
pub fn word_at_position(source: &str, pos: Position) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let line_idx = pos.line as usize;
    let char_idx = pos.character as usize;

    if line_idx >= lines.len() {
        return String::new();
    }
    let chars: Vec<char> = lines[line_idx].chars().collect();
    if char_idx >= chars.len() {
        return String::new();
    }

    let is_ident = |c: char| c.is_alphanumeric() || c == '_';

    if !is_ident(chars[char_idx]) {
        return String::new();
    }

    let start = {
        let mut i = char_idx;
        while i > 0 && is_ident(chars[i - 1]) { i -= 1; }
        i
    };
    let end = {
        let mut i = char_idx;
        while i < chars.len() && is_ident(chars[i]) { i += 1; }
        i
    };

    chars[start..end].iter().collect()
}

/// Returns the path segments immediately before the word at `pos`, e.g. for
/// `base::Minor::port` with cursor on `port`, returns `["base", "Minor"]`.
/// Returns `None` if no `::` prefix exists before the word.
pub fn path_prefix_before_word(source: &str, pos: Position) -> Option<Vec<String>> {
    let lines: Vec<&str> = source.lines().collect();
    let line_idx = pos.line as usize;
    let char_idx = pos.character as usize;
    if line_idx >= lines.len() { return None; }
    let chars: Vec<char> = lines[line_idx].chars().collect();
    // Skip back over the current word
    let mut i = char_idx.min(chars.len());
    while i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_') { i -= 1; }
    // Must be preceded by `::`
    if i < 2 || chars[i - 1] != ':' || chars[i - 2] != ':' { return None; }
    // Now parse the path ending at position i-2
    let prefix: String = chars[..i - 2].iter().collect();
    let segments: Vec<String> = prefix
        .split("::")
        .map(|part| {
            part.chars()
                .rev()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        })
        .filter(|s| !s.is_empty())
        .collect();
    if segments.is_empty() { None } else { Some(segments) }
}

/// Returns true when `pos` falls inside a block comment (supports nesting: /* /* */ */).
/// Handles strings and line comments so their `/*` sequences are not counted.
pub fn is_cursor_in_block_comment(source: &str, pos: Position) -> bool {
    let target_line = pos.line as usize;
    let target_char = pos.character as usize;
    let bytes = source.as_bytes();
    let mut depth: i32 = 0;
    let mut i = 0usize;
    let mut cur_line = 0usize;
    let mut cur_col = 0usize;

    macro_rules! past_cursor {
        () => { cur_line > target_line || (cur_line == target_line && cur_col >= target_char) }
    }
    macro_rules! advance_char {
        ($b:expr) => {
            if $b == b'\n' { cur_line += 1; cur_col = 0; } else { cur_col += 1; }
            i += 1;
        }
    }

    while i < bytes.len() && !past_cursor!() {
        // Skip string literals when not inside a block comment.
        if depth == 0 && bytes[i] == b'"' {
            cur_col += 1; i += 1;
            while i < bytes.len() && !past_cursor!() && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < bytes.len() { advance_char!(bytes[i]); }
                if i < bytes.len() && !past_cursor!() { advance_char!(bytes[i]); }
            }
            if i < bytes.len() && !past_cursor!() { cur_col += 1; i += 1; }
            continue;
        }
        // Skip line comments when not inside a block comment.
        if depth == 0 && i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i+1] == b'/' {
            while i < bytes.len() && !past_cursor!() && bytes[i] != b'\n' {
                cur_col += 1; i += 1;
            }
            continue;
        }
        // Block comment open.
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i+1] == b'*' {
            depth += 1;
            cur_col += 2; i += 2;
            continue;
        }
        // Block comment close.
        if depth > 0 && i + 1 < bytes.len() && bytes[i] == b'*' && bytes[i+1] == b'/' {
            depth -= 1;
            cur_col += 2; i += 2;
            continue;
        }
        advance_char!(bytes[i]);
    }

    depth > 0
}

/// If the text before `pos` ends with `seg1::seg2::` (one or more segments followed by `::`)
/// return those segments. For example, `"foo::bar::"` → `Some(vec!["foo", "bar"])`.
/// Returns `None` when text before cursor does not end with `::`.
pub fn path_before_cursor(source: &str, pos: Position) -> Option<Vec<String>> {
    let lines: Vec<&str> = source.lines().collect();
    let line_idx = pos.line as usize;
    let char_idx = pos.character as usize;

    if line_idx >= lines.len() {
        return None;
    }
    let line = lines[line_idx];
    let text_before = &line[..char_idx.min(line.len())];

    if !text_before.ends_with("::") {
        return None;
    }

    let without_suffix = &text_before[..text_before.len() - 2];
    let segments: Vec<String> = without_suffix
        .split("::")
        .map(|part| {
            part.chars()
                .rev()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        })
        .filter(|s| !s.is_empty())
        .collect();

    if segments.is_empty() { None } else { Some(segments) }
}

/// Returns `true` if the cursor is immediately after a bare `:` — a type annotation position.
pub fn is_in_type_position(source: &str, pos: Position) -> bool {
    let lines: Vec<&str> = source.lines().collect();
    let line_idx = pos.line as usize;
    let char_idx = pos.character as usize;

    if line_idx >= lines.len() {
        return false;
    }
    let line = lines[line_idx];
    let text_before = &line[..char_idx.min(line.len())];
    let trimmed = text_before.trim_end();

    trimmed.ends_with(':') && !trimmed.ends_with("::")
}

/// Convert a zero-indexed LSP Position to a byte offset in `source`.
pub fn lsp_pos_to_byte_offset(source: &str, pos: Position) -> usize {
    let mut line = 0u32;
    let mut line_start = 0usize;
    for (i, ch) in source.char_indices() {
        if line == pos.line {
            return line_start + pos.character as usize;
        }
        if ch == '\n' {
            line += 1;
            line_start = i + 1;
        }
    }
    line_start + pos.character as usize
}

// ── AST hover helpers ─────────────────────────────────────────────────────────

/// Walk all function bodies in `program` and return `Some(has_else)` if any
/// `IfStmt` whose span contains `offset` is found.
pub fn find_if_at_offset(program: &Program, offset: usize) -> Option<bool> {
    for item in &program.items {
        if let TopLevelItem::Function(fdecl) = item {
            if let Some(has_else) = search_stmts_for_if(&fdecl.body.stmts, offset) {
                return Some(has_else);
            }
        }
    }
    None
}

fn search_stmts_for_if(stmts: &[FuncStmt], offset: usize) -> Option<bool> {
    for stmt in stmts {
        if let FuncStmt::If(if_stmt) = stmt {
            if if_stmt.span.start <= offset && offset <= if_stmt.span.end {
                return Some(!if_stmt.else_stmts.is_empty());
            }
            if let Some(found) = search_stmts_for_if(&if_stmt.then_stmts, offset) {
                return Some(found);
            }
            if let Some(found) = search_stmts_for_if(&if_stmt.else_stmts, offset) {
                return Some(found);
            }
        }
    }
    None
}

/// Walk top-level var declarations and function bodies to find an `Index`
/// expression whose span contains `offset`. Returns the element type of the
/// indexed list when the source is a known global variable.
pub fn find_index_elem_type_at_offset(
    program: &Program,
    symbols: &SymbolTable,
    offset: usize,
) -> Option<SparType> {
    use spar::ast::Expr;

    fn expr_index_elem(expr: &Expr, symbols: &SymbolTable, offset: usize) -> Option<SparType> {
        match expr {
            Expr::Index { source, index, span } => {
                if span.start <= offset && offset <= span.end {
                    // Resolve source to a List type
                    let elem_ty = match source.as_ref() {
                        Expr::NamespaceRef(nr) if nr.segments.len() == 1 => {
                            match symbols.globals.get(&nr.segments[0]) {
                                Some(spar::resolver::GlobalEntry::Var { ty: SparType::List(elem), .. }) => {
                                    Some(*elem.clone())
                                }
                                _ => None,
                            }
                        }
                        _ => None,
                    };
                    if elem_ty.is_some() { return elem_ty; }
                }
                expr_index_elem(source, symbols, offset)
                    .or_else(|| expr_index_elem(index, symbols, offset))
            }
            Expr::BinaryOp(b) => {
                expr_index_elem(&b.lhs, symbols, offset)
                    .or_else(|| expr_index_elem(&b.rhs, symbols, offset))
            }
            Expr::Unary { operand, .. } => expr_index_elem(operand, symbols, offset),
            Expr::List(items, _) => items.iter().find_map(|e| expr_index_elem(e, symbols, offset)),
            Expr::Grouped(inner, _) => expr_index_elem(inner, symbols, offset),
            Expr::FnCall(fc) => fc.args.iter().find_map(|a| expr_index_elem(a, symbols, offset)),
            Expr::Call { args, .. } => args.iter().find_map(|a| expr_index_elem(&a.value, symbols, offset)),
            Expr::Comprehension { source, body, .. } => {
                expr_index_elem(source, symbols, offset)
                    .or_else(|| expr_index_elem(body, symbols, offset))
            }
            Expr::String(s) => {
                use spar::ast::StringPart;
                s.parts.iter().find_map(|p| {
                    if let StringPart::Expr(e) = p { expr_index_elem(e, symbols, offset) } else { None }
                })
            }
            Expr::Literal(_) | Expr::NamespaceRef(_) => None,
        }
    }

    fn stmts_index_elem(stmts: &[FuncStmt], symbols: &SymbolTable, offset: usize) -> Option<SparType> {
        use spar::ast::{ReturnValue};
        for stmt in stmts {
            match stmt {
                FuncStmt::LocalVar(lv) => {
                    if let Some(t) = expr_index_elem(&lv.value, symbols, offset) { return Some(t); }
                }
                FuncStmt::Return(rv, _) => {
                    match rv {
                        ReturnValue::Expr(e) => {
                            if let Some(t) = expr_index_elem(e, symbols, offset) { return Some(t); }
                        }
                        ReturnValue::SectionBlock(fields) => {
                            for rf in fields {
                                if let Some(t) = expr_index_elem(&rf.value, symbols, offset) { return Some(t); }
                            }
                        }
                    }
                }
                FuncStmt::If(if_stmt) => {
                    if let Some(t) = expr_index_elem(&if_stmt.condition, symbols, offset) { return Some(t); }
                    if let Some(t) = stmts_index_elem(&if_stmt.then_stmts, symbols, offset) { return Some(t); }
                    if let Some(t) = stmts_index_elem(&if_stmt.else_stmts, symbols, offset) { return Some(t); }
                }
                FuncStmt::For { iterable, body, .. } => {
                    if let Some(t) = expr_index_elem(iterable, symbols, offset) { return Some(t); }
                    if let Some(t) = stmts_index_elem(body, symbols, offset) { return Some(t); }
                }
            }
        }
        None
    }

    for item in &program.items {
        match item {
            TopLevelItem::Var(vd) => {
                if let Some(v) = &vd.value {
                    if let Some(t) = expr_index_elem(v, symbols, offset) { return Some(t); }
                }
            }
            TopLevelItem::Function(fd) => {
                if let Some(t) = stmts_index_elem(&fd.body.stmts, symbols, offset) { return Some(t); }
            }
            _ => {}
        }
    }
    None
}

// ── Semantic token infrastructure ────────────────────────────────────────────

const TOKEN_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::VARIABLE,   // 0
    SemanticTokenType::FUNCTION,   // 1
    SemanticTokenType::PARAMETER,  // 2
    SemanticTokenType::PROPERTY,   // 3
    SemanticTokenType::NAMESPACE,  // 4
    SemanticTokenType::TYPE,       // 5
];

const TOKEN_MODIFIERS: &[SemanticTokenModifier] = &[
    SemanticTokenModifier::DECLARATION, // bit 0 = 1
];

const TT_VARIABLE: u32  = 0;
const TT_FUNCTION: u32  = 1;
const TT_PARAMETER: u32 = 2;
const TT_PROPERTY: u32  = 3;
const TT_NAMESPACE: u32 = 4;
const TT_TYPE: u32      = 5;
const MOD_NONE: u32        = 0;
const MOD_DECLARATION: u32 = 1;

struct RawToken {
    line:       u32,
    start_char: u32,
    length:     u32,
    token_type: u32,
    modifiers:  u32,
}

fn byte_to_lsp_pos(source: &str, byte_offset: usize) -> (u32, u32) {
    let off    = byte_offset.min(source.len());
    let before = &source[..off];
    let line   = before.bytes().filter(|&b| b == b'\n').count() as u32;
    let col    = (off - before.rfind('\n').map(|p| p + 1).unwrap_or(0)) as u32;
    (line, col)
}

fn raw_from_span(span: &Span, token_type: u32, modifiers: u32) -> RawToken {
    RawToken {
        line:       span.line.saturating_sub(1),
        start_char: span.col.saturating_sub(1),
        length:     (span.end.saturating_sub(span.start)) as u32,
        token_type,
        modifiers,
    }
}

/// Find `name` as a whole identifier (word-boundary) starting from `from_byte`.
/// Returns the byte offset of the match in `source`, or None.
fn find_ident_byte(source: &str, from_byte: usize, name: &str) -> Option<usize> {
    if from_byte >= source.len() { return None; }
    let haystack = &source[from_byte..];
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut search = 0usize;
    while search < haystack.len() {
        let rel = haystack[search..].find(name)?;
        let abs_rel = search + rel;
        let before_ok = abs_rel == 0
            || !haystack[..abs_rel].chars().last().map(is_ident).unwrap_or(false);
        let after_pos = abs_rel + name.len();
        let after_ok  = after_pos >= haystack.len()
            || !haystack[after_pos..].chars().next().map(is_ident).unwrap_or(false);
        if before_ok && after_ok {
            return Some(from_byte + abs_rel);
        }
        search = abs_rel + 1;
    }
    None
}

fn find_ident_token(
    source: &str, from_byte: usize, name: &str,
    token_type: u32, modifiers: u32,
) -> Option<RawToken> {
    let byte_off = find_ident_byte(source, from_byte, name)?;
    let (line, col) = byte_to_lsp_pos(source, byte_off);
    Some(RawToken { line, start_char: col, length: name.len() as u32, token_type, modifiers })
}

fn collect_expr_tokens(expr: &spar::ast::Expr, source: &str, out: &mut Vec<RawToken>) {
    use spar::ast::{Expr, StringPart};
    match expr {
        Expr::Call { name_span, args, .. } => {
            out.push(raw_from_span(name_span, TT_FUNCTION, MOD_NONE));
            for arg in args { collect_expr_tokens(&arg.value, source, out); }
        }
        Expr::FnCall(fc) => {
            if let Some(tok) = find_ident_token(source, fc.span.start, &fc.name, TT_FUNCTION, MOD_NONE) {
                out.push(tok);
            }
            for arg in &fc.args { collect_expr_tokens(arg, source, out); }
        }
        Expr::NamespaceRef(nr) if nr.segments.len() == 1 => {
            if let Some(tok) = find_ident_token(source, nr.span.start, &nr.segments[0], TT_VARIABLE, MOD_NONE) {
                out.push(tok);
            }
        }
        Expr::NamespaceRef(_) => {}
        Expr::BinaryOp(b) => {
            collect_expr_tokens(&b.lhs, source, out);
            collect_expr_tokens(&b.rhs, source, out);
        }
        Expr::Unary { operand, .. } => collect_expr_tokens(operand, source, out),
        Expr::List(items, _) => {
            for item in items { collect_expr_tokens(item, source, out); }
        }
        Expr::Grouped(inner, _) => collect_expr_tokens(inner, source, out),
        Expr::Comprehension { var_name_span, source: comp_src, body, .. } => {
            out.push(raw_from_span(var_name_span, TT_VARIABLE, MOD_DECLARATION));
            collect_expr_tokens(comp_src, source, out);
            collect_expr_tokens(body, source, out);
        }
        Expr::Index { source: src_expr, index, .. } => {
            collect_expr_tokens(src_expr, source, out);
            collect_expr_tokens(index, source, out);
        }
        Expr::String(s) => {
            for part in &s.parts {
                if let StringPart::Expr(e) = part { collect_expr_tokens(e, source, out); }
            }
        }
        Expr::Literal(_) => {}
    }
}

fn collect_stmts_tokens(stmts: &[FuncStmt], source: &str, out: &mut Vec<RawToken>) {
    use spar::ast::{FuncStmt as FS, ReturnValue};
    for stmt in stmts {
        match stmt {
            FS::LocalVar(lv) => {
                if let Some(tok) = find_ident_token(source, lv.span.start, &lv.name, TT_VARIABLE, MOD_DECLARATION) {
                    out.push(tok);
                }
                collect_expr_tokens(&lv.value, source, out);
            }
            FS::Return(rv, _) => match rv {
                ReturnValue::Expr(e) => collect_expr_tokens(e, source, out),
                ReturnValue::SectionBlock(fields) => {
                    for rf in fields { collect_expr_tokens(&rf.value, source, out); }
                }
            },
            FS::If(if_stmt) => {
                collect_expr_tokens(&if_stmt.condition, source, out);
                collect_stmts_tokens(&if_stmt.then_stmts, source, out);
                collect_stmts_tokens(&if_stmt.else_stmts, source, out);
            }
            FS::For { var_name, iterable, body, span } => {
                if let Some(tok) = find_ident_token(source, span.start, var_name, TT_VARIABLE, MOD_DECLARATION) {
                    out.push(tok);
                }
                collect_expr_tokens(iterable, source, out);
                collect_stmts_tokens(body, source, out);
            }
        }
    }
}

fn collect_section_items_tokens(
    items: &[spar::ast::SectionItem], source: &str, out: &mut Vec<RawToken>,
) {
    use spar::ast::{SectionItem, FieldValue};
    for item in items {
        match item {
            SectionItem::Field(fd) => {
                if let Some(tok) = find_ident_token(source, fd.span.start, &fd.name, TT_PROPERTY, MOD_DECLARATION) {
                    out.push(tok);
                }
                match &fd.value {
                    Some(FieldValue::Expr(e)) => collect_expr_tokens(e, source, out),
                    Some(FieldValue::Nested(nested)) => collect_section_items_tokens(nested, source, out),
                    None => {}
                }
            }
            SectionItem::Spread(ss) => collect_expr_tokens(&ss.expr, source, out),
        }
    }
}

fn collect_tokens_from_program(program: &Program, source: &str, out: &mut Vec<RawToken>) {
    use spar::ast::TopLevelItem as TL;
    for item in &program.items {
        match item {
            TL::Var(vd) => {
                if let Some(tok) = find_ident_token(source, vd.span.start, &vd.name, TT_VARIABLE, MOD_DECLARATION) {
                    out.push(tok);
                }
                if let Some(expr) = &vd.value { collect_expr_tokens(expr, source, out); }
            }
            TL::Dynamic(dd) => {
                if let Some(tok) = find_ident_token(source, dd.span.start, &dd.name, TT_VARIABLE, MOD_DECLARATION) {
                    out.push(tok);
                }
                if let Some(expr) = &dd.value { collect_expr_tokens(expr, source, out); }
            }
            TL::Section(sd) => {
                let mut search_from = sd.span.start;
                for seg in &sd.path {
                    if let Some(byte_off) = find_ident_byte(source, search_from, seg) {
                        let (line, col) = byte_to_lsp_pos(source, byte_off);
                        out.push(RawToken { line, start_char: col, length: seg.len() as u32, token_type: TT_NAMESPACE, modifiers: MOD_DECLARATION });
                        search_from = byte_off + seg.len();
                    }
                }
                // `[Section] -> TypeName { ... }` — TypeName gets its own token.
                if let Some(binding) = &sd.type_binding {
                    out.push(raw_from_span(&binding.span, TT_TYPE, MOD_NONE));
                }
                collect_section_items_tokens(&sd.items, source, out);
            }
            TL::Function(fd) => {
                out.push(raw_from_span(&fd.name_span, TT_FUNCTION, MOD_DECLARATION));
                for param in &fd.params {
                    if let Some(tok) = find_ident_token(source, param.span.start, &param.name, TT_PARAMETER, MOD_DECLARATION) {
                        out.push(tok);
                    }
                }
                collect_stmts_tokens(&fd.body.stmts, source, out);
            }
            TL::Type(td) => {
                out.push(raw_from_span(&td.name_span, TT_TYPE, MOD_DECLARATION));
                collect_type_fields_tokens(&td.fields, source, out);
            }
            TL::SchemaSection(sd) => {
                if let Some(tok) = find_ident_token(source, sd.span.start, &sd.name, TT_TYPE, MOD_DECLARATION) {
                    out.push(tok);
                }
                collect_schema_fields_tokens(&sd.fields, source, out);
            }
            TL::SchemaFrom(sf) => {
                if let Some(tok) = find_ident_token(source, sf.span.start, &sf.name, TT_TYPE, MOD_DECLARATION) {
                    out.push(tok);
                }
                out.push(raw_from_span(&sf.source_type_span, TT_TYPE, MOD_NONE));
            }
            TL::Import(_) => {}
        }
    }
}

/// A `type [Name]{ ... }` declaration's own fields — property names, and a
/// `Named(OtherType)` shape reference gets its own TT_TYPE token (found by
/// text search from the field's span, same "search near a known offset"
/// pattern the rest of this file already uses — TypeField carries no
/// dedicated span for just the type-name portion of `field: OtherType;`).
fn collect_type_fields_tokens(fields: &[spar::ast::TypeField], source: &str, out: &mut Vec<RawToken>) {
    use spar::ast::TypeFieldShape;
    for f in fields {
        if let Some(tok) = find_ident_token(source, f.span.start, &f.name, TT_PROPERTY, MOD_DECLARATION) {
            out.push(tok);
        }
        match &f.shape {
            TypeFieldShape::Primitive(_) => {}
            TypeFieldShape::Named(other) => {
                if let Some(tok) = find_ident_token(source, f.span.start, other, TT_TYPE, MOD_NONE) {
                    out.push(tok);
                }
            }
            TypeFieldShape::Section(nested) => collect_type_fields_tokens(nested, source, out),
        }
    }
}

/// Same idea as `collect_type_fields_tokens`, for `Schema [Name]{ ... }`
/// field bodies (`SchemaFieldShape` has no `Named` variant, so there's no
/// type-reference token to emit — just property names, recursively).
fn collect_schema_fields_tokens(fields: &[spar::ast::SchemaField], source: &str, out: &mut Vec<RawToken>) {
    use spar::ast::SchemaFieldShape;
    for f in fields {
        if let Some(tok) = find_ident_token(source, f.span.start, &f.name, TT_PROPERTY, MOD_DECLARATION) {
            out.push(tok);
        }
        if let SchemaFieldShape::Section(nested) = &f.shape {
            collect_schema_fields_tokens(nested, source, out);
        }
    }
}

// ── Completion builders ───────────────────────────────────────────────────────

fn keyword_items() -> Vec<CompletionItem> {
    [
        // declaration keywords
        "var", "export", "private", "import", "dynamic", "as", "section", "function",
        // control keywords
        "if", "else", "for", "in", "return",
        // literals
        "true", "false",
    ]
    .iter()
    .map(|kw| CompletionItem {
        label: kw.to_string(),
        kind:  Some(CompletionItemKind::KEYWORD),
        ..Default::default()
    })
    .collect()
}

fn type_keyword_items() -> Vec<CompletionItem> {
    ["int", "float", "str", "bool", "section"]
        .iter()
        .map(|kw| CompletionItem {
            label: kw.to_string(),
            kind:  Some(CompletionItemKind::KEYWORD),
            ..Default::default()
        })
        .collect()
}

fn builtin_items() -> Vec<CompletionItem> {
    [
        ("env",   "(name: str) -> str"),
        ("int",   "(value: int | float | str) -> int"),
        ("float", "(value: int | float | str) -> float"),
        ("str",   "(value: int | float | bool | str) -> str"),
        ("bool",  "(value: str | bool) -> bool"),
    ]
    .iter()
    .map(|(name, sig)| CompletionItem {
        label:  name.to_string(),
        kind:   Some(CompletionItemKind::FUNCTION),
        detail: Some(sig.to_string()),
        ..Default::default()
    })
    .collect()
}

fn function_completion_items(functions: &HashMap<String, FunctionEntry>) -> Vec<CompletionItem> {
    functions
        .iter()
        .map(|(name, entry)| {
            let param_list = entry.params.iter()
                .map(|(pname, pty)| format!("{}: {}", pname, format_spar_type(pty)))
                .collect::<Vec<_>>()
                .join(", ");
            CompletionItem {
                label:              name.clone(),
                kind:               Some(CompletionItemKind::FUNCTION),
                detail:             Some(format!("({}) -> {}", param_list, format_spar_type(&entry.ret))),
                insert_text:        Some(format!("{}($1)", name)),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            }
        })
        .collect()
}

fn section_field_completions(symbols: &SymbolTable, path: &[String], section: &SectionEntry) -> Vec<CompletionItem> {
    section.fields.iter()
        .map(|(name, entry)| {
            if entry.ty == Some(SparType::Section) {
                CompletionItem {
                    label:       name.clone(),
                    kind:        Some(CompletionItemKind::MODULE),
                    detail:      Some("section".to_string()),
                    insert_text: Some(format!("{}::", name)),
                    ..Default::default()
                }
            } else {
                CompletionItem {
                    label:  name.clone(),
                    kind:   Some(CompletionItemKind::FIELD),
                    detail: Some(entry.ty.as_ref().map(format_spar_type)
                        .unwrap_or_else(|| resolve_field_type_display(symbols, path, name))),
                    ..Default::default()
                }
            }
        })
        .collect()
}

// ── Hover formatters ──────────────────────────────────────────────────────────

fn format_hover_global(name: &str, entry: &GlobalEntry) -> String {
    let ty_str = match entry {
        GlobalEntry::Var { ty, optional, exported, .. } => {
            let base          = format_spar_type(ty);
            let opt_marker    = if *optional  { "?" } else { "" };
            let export_prefix = if *exported { "export " } else { "" };
            format!("{export_prefix}{}{opt_marker}", base)
        }
        GlobalEntry::Dynamic { optional, .. } => {
            if *optional { "dynamic?".to_string() } else { "dynamic".to_string() }
        }
    };
    format!("```spar\n(var) {}: {}\n```", name, ty_str)
}

fn format_hover_section(symbols: &SymbolTable, path: &[String], section: &SectionEntry) -> String {
    let section_label = path.join(".");
    let field_list: String = section.fields
        .iter()
        .map(|(name, fentry)| {
            if fentry.ty == Some(SparType::Section) {
                format!("  {}: section  // → {}::{}", name, section_label, name)
            } else {
                let ty_str = fentry.ty.as_ref().map(format_spar_type)
                    .unwrap_or_else(|| resolve_field_type_display(symbols, path, name));
                format!("  {}: {}", name, ty_str)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("```spar\n[{}]{{\n{}\n}}\n```", section_label, field_list)
}

fn format_hover_function(name: &str, entry: &FunctionEntry) -> String {
    let params_str = entry.params.iter()
        .map(|(pname, pty)| format!("{}: {}", pname, format_spar_type(pty)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("```spar\nfunction {}({}) -> {}\n```", name, params_str, format_spar_type(&entry.ret))
}

// ── Import hover / completion helpers ────────────────────────────────────────

fn format_import_hover(alias: &str, sym: &SymbolTable) -> String {
    let mut lines: Vec<String> = vec![format!("// import alias: {}", alias)];
    for (name, entry) in &sym.functions {
        if !entry.is_private {
            let params = entry.params.iter()
                .map(|(n, t)| format!("{}: {}", n, format_spar_type(t)))
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(format!("function {}({}) -> {}", name, params, format_spar_type(&entry.ret)));
        }
    }
    for (path, section) in &sym.sections {
        if section.exported && !section.private {
            let field_names: Vec<_> = section.fields.keys().cloned().collect();
            lines.push(format!("[{}] {{ {} }}", path.join("."), field_names.join(", ")));
        }
    }
    for (name, entry) in &sym.globals {
        if let GlobalEntry::Var { exported: true, ty, .. } = entry {
            lines.push(format!("export var {}: {}", name, format_spar_type(ty)));
        }
    }
    format!("```spar\n{}\n```", lines.join("\n"))
}

fn import_completion_items(sym: &SymbolTable) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = vec![];
    for (path, section) in &sym.sections {
        if section.exported && !section.private {
            if let Some(name) = path.first() {
                let fields: Vec<_> = section.fields.keys().cloned().collect();
                items.push(CompletionItem {
                    label:  name.clone(),
                    kind:   Some(CompletionItemKind::MODULE),
                    detail: Some(format!("section ({})", fields.join(", "))),
                    ..Default::default()
                });
            }
        }
    }
    for (name, entry) in &sym.functions {
        if !entry.is_private {
            let params = entry.params.iter()
                .map(|(n, t)| format!("{}: {}", n, format_spar_type(t)))
                .collect::<Vec<_>>()
                .join(", ");
            items.push(CompletionItem {
                label:              name.clone(),
                kind:               Some(CompletionItemKind::FUNCTION),
                detail:             Some(format!("({}) -> {}", params, format_spar_type(&entry.ret))),
                insert_text:        Some(format!("{}($1)", name)),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            });
        }
    }
    for (name, entry) in &sym.globals {
        if let GlobalEntry::Var { exported: true, ty, .. } = entry {
            items.push(CompletionItem {
                label:  name.clone(),
                kind:   Some(CompletionItemKind::VARIABLE),
                detail: Some(format_spar_type(ty)),
                ..Default::default()
            });
        }
    }
    items
}

// ── Document state ────────────────────────────────────────────────────────────

struct DocumentState {
    source:  String,
    ast:     Option<Program>,
    symbols: Option<SymbolTable>,
    import_symbols: HashMap<String, SymbolTable>,
    #[allow(dead_code)]
    result:  Option<EvalResult>,
    errors:  Vec<SparError>,
    last_good_symbols: Option<SymbolTable>,
    last_good_import_symbols: HashMap<String, SymbolTable>,
}

impl DocumentState {
    fn diagnostics(&self) -> Vec<Diagnostic> {
        self.errors.iter().map(spar_error_to_diagnostic).collect()
    }

    fn effective_symbols(&self) -> Option<&SymbolTable> {
        self.symbols.as_ref().or(self.last_good_symbols.as_ref())
    }

    fn effective_import_symbols(&self) -> &HashMap<String, SymbolTable> {
        if !self.import_symbols.is_empty() { &self.import_symbols } else { &self.last_good_import_symbols }
    }
}

// ── LSP helpers ───────────────────────────────────────────────────────────────

fn base_dir_from_uri(uri: &Url) -> std::path::PathBuf {
    uri.to_file_path()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

fn analyze_single_file(source: &str, program: Program, mut errors: Vec<SparError>) -> DocumentState {
    let sym = match Resolver::new().resolve(&program, &[]) {
        Ok(s)  => s,
        Err(e) => {
            errors.extend(e);
            return DocumentState { source: source.to_string(), ast: Some(program), symbols: None, import_symbols: HashMap::new(), result: None, errors, last_good_symbols: None, last_good_import_symbols: HashMap::new() };
        }
    };
    if let Err(e) = TypeChecker::check(&program, &sym) {
        errors.extend(e);
    }
    DocumentState { source: source.to_string(), ast: Some(program), symbols: Some(sym), import_symbols: HashMap::new(), result: None, errors, last_good_symbols: None, last_good_import_symbols: HashMap::new() }
}

/// Returns the canonical absolute paths of all non-schema imports declared in `program`,
/// resolved relative to `base_dir`.
fn extract_imported_paths(program: &spar::ast::Program, base_dir: &std::path::Path) -> Vec<PathBuf> {
    use spar::ast::{TopLevelItem, ImportKind};
    program.items.iter().filter_map(|item| {
        if let TopLevelItem::Import(d) = item {
            if !matches!(d.kind, ImportKind::Schema) {
                let p = base_dir.join(&d.path);
                p.canonicalize().ok()
            } else {
                None
            }
        } else {
            None
        }
    }).collect()
}

// ── LSP Server ────────────────────────────────────────────────────────────────

struct KlLanguageServer {
    client:         Client,
    documents:      Mutex<HashMap<Url, DocumentState>>,
    importers:      Mutex<HashMap<PathBuf, HashSet<PathBuf>>>,
    workspace_root: Mutex<Option<PathBuf>>,
}

impl KlLanguageServer {
    fn analyze(source: &str, base_dir: &std::path::Path) -> DocumentState {
        let mut all_errors: Vec<SparError> = Vec::new();

        let tokens = match Lexer::new(source).tokenize() {
            Ok(t)  => t,
            Err(e) => {
                all_errors.push(e);
                return DocumentState {
                    source:  source.to_string(),
                    ast:     None,
                    symbols: None,
                    import_symbols: HashMap::new(),
                    result:  None,
                    errors:  all_errors,
                    last_good_symbols: None,
                    last_good_import_symbols: HashMap::new(),
                };
            }
        };

        let mut program = match Parser::new(tokens).parse() {
            Ok(p)  => p,
            Err(e) => {
                all_errors.push(e);
                return DocumentState {
                    source:  source.to_string(),
                    ast:     None,
                    symbols: None,
                    import_symbols: HashMap::new(),
                    result:  None,
                    errors:  all_errors,
                    last_good_symbols: None,
                    last_good_import_symbols: HashMap::new(),
                };
            }
        };

        // Splice selective / import type / asPartOf imports into local scope
        // before anything else touches `program` — same ordering as the CLI.
        let mut expand_loader = ImportLoader::new(base_dir);
        if let Err(e) = expand_imports(&mut program, &mut expand_loader) {
            all_errors.extend(e);
            return analyze_single_file(source, program, all_errors);
        }

        let mut loader = ImportLoader::new(base_dir);
        let imports = match collect_imports(&program, &mut loader) {
            Ok(i)  => i,
            Err(e) => {
                all_errors.extend(e);
                return analyze_single_file(source, program, all_errors);
            }
        };

        // Resolve each imported file's full symbols for hover and completion.
        // Done here (before main resolver) so it's populated even if main resolve fails.
        let mut import_symbols: HashMap<String, SymbolTable> = HashMap::new();
        for (alias, loaded) in &imports {
            let full_path = base_dir.join(&loaded.path);
            if let Ok(imp_src) = std::fs::read_to_string(&full_path) {
                if let Ok(tokens) = Lexer::new(&imp_src).tokenize() {
                    if let Ok(prog) = Parser::new(tokens).parse() {
                        if let Ok(isym) = Resolver::new().resolve(&prog, &[]) {
                            import_symbols.insert(alias.clone(), isym);
                        }
                    }
                }
            }
        }

        let sym = match Resolver::resolve_with_imports(&program, &imports) {
            Ok(s)  => s,
            Err(e) => {
                all_errors.extend(e);
                return DocumentState {
                    source:  source.to_string(),
                    ast:     Some(program),
                    symbols: None,
                    import_symbols,
                    result:  None,
                    errors:  all_errors,
                    last_good_symbols: None,
                    last_good_import_symbols: HashMap::new(),
                };
            }
        };

        if let Err(e) = TypeChecker::check_with_imports(&program, &sym, &imports) {
            all_errors.extend(e);
        }

        if let Err(e) = validate_schema_imports(&program, base_dir) {
            all_errors.extend(e);
        }

        let eval_result = if all_errors.is_empty() {
            match Evaluator::evaluate_with_imports_and_base(&program, &sym, &imports, base_dir) {
                Ok(r)  => Some(r),
                Err(e) => { all_errors.extend(e); None }
            }
        } else {
            None
        };

        DocumentState {
            source:  source.to_string(),
            ast:     Some(program),
            symbols: Some(sym),
            import_symbols,
            result:  eval_result,
            errors:  all_errors,
            last_good_symbols: None,
            last_good_import_symbols: HashMap::new(),
        }
    }

    async fn publish_diagnostics(&self, uri: Url, diags: Vec<Diagnostic>) {
        self.client.publish_diagnostics(uri, diags, None).await;
    }

    /// Re-analyse all direct importers of `canon` and push fresh diagnostics.
    /// Called from both did_save and did_change (when parse succeeds).
    async fn propagate_to_importers(&self, canon: &PathBuf) {
        let importers_snapshot = {
            let map = self.importers.lock().await;
            map.get(canon).cloned().unwrap_or_default()
        };
        for importer_path in importers_snapshot {
            let Ok(src2) = std::fs::read_to_string(&importer_path) else { continue };
            let base2 = importer_path.parent().unwrap_or(std::path::Path::new("."));
            let state2 = Self::analyze(&src2, base2);
            let Ok(uri2) = Url::from_file_path(&importer_path) else { continue };
            let diags2 = state2.diagnostics();
            self.publish_diagnostics(uri2, diags2).await;
        }
    }

    /// Updates the importers reverse map when `file_path` is opened/saved.
    /// `file_path` is the canonical absolute path of the file being analyzed.
    /// `imports` is the list of files it imports (from `extract_imported_paths`).
    async fn update_importers(&self, file_path: &PathBuf, imports: &[PathBuf]) {
        let mut map = self.importers.lock().await;
        // Remove this file from all existing importer sets (stale entries).
        for set in map.values_mut() {
            set.remove(file_path);
        }
        // Add fresh entries.
        for imported in imports {
            map.entry(imported.clone()).or_default().insert(file_path.clone());
        }
    }

    async fn scan_workspace(&self, root: &std::path::Path) {
        use std::fs;
        fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
            let Ok(entries) = fs::read_dir(dir) else { return };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().and_then(|e| e.to_str()) == Some("spar") {
                    out.push(path);
                }
            }
        }
        let mut files = Vec::new();
        walk(root, &mut files);
        for path in files {
            let Ok(src) = std::fs::read_to_string(&path) else { continue };
            let base = path.parent().unwrap_or(root);
            let state = Self::analyze(&src, base);
            if let Some(program) = &state.ast {
                if let Ok(canon) = path.canonicalize() {
                    let imports = extract_imported_paths(program, base);
                    self.update_importers(&canon, &imports).await;
                }
            }
            let Ok(uri) = Url::from_file_path(&path) else { continue };
            let diags = state.diagnostics();
            self.publish_diagnostics(uri, diags).await;
        }
    }
}

// ── LanguageServer implementation ─────────────────────────────────────────────

#[tower_lsp::async_trait]
impl LanguageServer for KlLanguageServer {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // Store workspace root for later file scan.
        let root = params.root_uri
            .and_then(|u| u.to_file_path().ok())
            .or_else(|| params.workspace_folders
                .as_deref()
                .and_then(|wf| wf.first())
                .and_then(|f| f.uri.to_file_path().ok()));
        *self.workspace_root.lock().await = root;

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change:     Some(TextDocumentSyncKind::FULL),
                        save:       Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(false),
                        })),
                        ..Default::default()
                    }
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![":".to_string()]),
                    resolve_provider:   Some(false),
                    ..Default::default()
                }),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(SemanticTokensOptions {
                        legend: SemanticTokensLegend {
                            token_types:     TOKEN_TYPES.to_vec(),
                            token_modifiers: TOKEN_MODIFIERS.to_vec(),
                        },
                        full:  Some(SemanticTokensFullOptions::Bool(true)),
                        range: Some(false),
                        ..Default::default()
                    })
                ),
                document_formatting_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name:    "spar-ls".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        self.client.log_message(MessageType::INFO, "spar-ls initialized").await;

        // Register watcher for external file changes.
        let registration = Registration {
            id:     "spar-file-watcher".to_string(),
            method: "workspace/didChangeWatchedFiles".to_string(),
            register_options: Some(serde_json::to_value(DidChangeWatchedFilesRegistrationOptions {
                watchers: vec![FileSystemWatcher {
                    glob_pattern: GlobPattern::String("**/*.spar".to_string()),
                    kind: None,
                }],
            }).unwrap()),
        };
        let _ = self.client.register_capability(vec![registration]).await;

        // Scan workspace and push diagnostics for all .spar files.
        let root = self.workspace_root.lock().await.clone();
        if let Some(root_dir) = root {
            self.scan_workspace(&root_dir).await;
        }
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri  = params.text_document.uri;
        let src  = params.text_document.text;
        let base = base_dir_from_uri(&uri);
        let mut state = Self::analyze(&src, &base);
        // Update reverse import map.
        if let Some(program) = &state.ast {
            if let Ok(file_path) = uri.to_file_path() {
                if let Ok(canon) = file_path.canonicalize() {
                    let imports = extract_imported_paths(program, &base);
                    self.update_importers(&canon, &imports).await;
                }
            }
        }
        self.client.log_message(
            tower_lsp::lsp_types::MessageType::INFO,
            format!(
                "[spar-ls] did_open: symbols={} import_aliases=[{}] errors={}",
                state.symbols.is_some(),
                state.import_symbols.keys().cloned().collect::<Vec<_>>().join(","),
                state.errors.len()
            ),
        ).await;
        if state.symbols.is_some() {
            state.last_good_symbols = state.symbols.clone();
            state.last_good_import_symbols = state.import_symbols.clone();
        }
        let diags = state.diagnostics();
        self.documents.lock().await.insert(uri.clone(), state);
        self.publish_diagnostics(uri, diags).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        if let Some(change) = params.content_changes.into_iter().next() {
            let src  = change.text;
            let base = base_dir_from_uri(&uri);

            // Read previous last-good symbols before releasing lock.
            let (prev_good_sym, prev_good_imports) = {
                let docs = self.documents.lock().await;
                if let Some(prev) = docs.get(&uri) {
                    let sym = prev.last_good_symbols.clone().or_else(|| prev.symbols.clone());
                    let imp = if !prev.last_good_import_symbols.is_empty() {
                        prev.last_good_import_symbols.clone()
                    } else {
                        prev.import_symbols.clone()
                    };
                    (sym, imp)
                } else {
                    (None, HashMap::new())
                }
            };

            let mut state = Self::analyze(&src, &base);
            if state.symbols.is_some() {
                state.last_good_symbols = state.symbols.clone();
                state.last_good_import_symbols = state.import_symbols.clone();
            } else {
                state.last_good_symbols = prev_good_sym;
                state.last_good_import_symbols = prev_good_imports;
            }

            // Update importers map and propagate diagnostics to files that import this one.
            // Only when AST is valid — avoids thrashing importers with every broken keystroke.
            if let Some(program) = &state.ast {
                if let Some(canon) = uri.to_file_path().ok().and_then(|p| p.canonicalize().ok()) {
                    let imports = extract_imported_paths(program, &base);
                    self.update_importers(&canon, &imports).await;
                    if state.symbols.is_some() {
                        self.propagate_to_importers(&canon).await;
                    }
                }
            }

            let diags = state.diagnostics();
            self.documents.lock().await.insert(uri.clone(), state);
            self.publish_diagnostics(uri, diags).await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.documents.lock().await.remove(&uri);
        self.client.publish_diagnostics(uri, vec![], None).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri  = params.text_document.uri;
        let base = base_dir_from_uri(&uri);

        // Re-analyse the saved file from disk.
        let src = match uri.to_file_path().ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
        {
            Some(s) => s,
            None    => return,
        };

        // Read previous last-good state so hover/completion survive a failed save.
        let (prev_good_sym, prev_good_imports) = {
            let docs = self.documents.lock().await;
            if let Some(prev) = docs.get(&uri) {
                let sym = prev.last_good_symbols.clone().or_else(|| prev.symbols.clone());
                let imp = if !prev.last_good_import_symbols.is_empty() {
                    prev.last_good_import_symbols.clone()
                } else {
                    prev.import_symbols.clone()
                };
                (sym, imp)
            } else {
                (None, HashMap::new())
            }
        };

        let mut state = Self::analyze(&src, &base);
        if state.symbols.is_some() {
            state.last_good_symbols        = state.symbols.clone();
            state.last_good_import_symbols = state.import_symbols.clone();
        } else {
            state.last_good_symbols        = prev_good_sym;
            state.last_good_import_symbols = prev_good_imports;
        }

        // Update importers map and re-diagnose direct importers of this file.
        if let Some(program) = &state.ast {
            if let Some(canon) = uri.to_file_path().ok().and_then(|p| p.canonicalize().ok()) {
                let imports = extract_imported_paths(program, &base);
                self.update_importers(&canon, &imports).await;

                // Re-diagnose all direct importers of this file.
                self.propagate_to_importers(&canon).await;
            }
        }

        // Publish diagnostics for the saved file itself.
        let diags = state.diagnostics();
        self.documents.lock().await.insert(uri.clone(), state);
        self.publish_diagnostics(uri, diags).await;
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        for change in params.changes {
            if change.typ == FileChangeType::CREATED || change.typ == FileChangeType::CHANGED {
                // Treat like a save: re-analyse and propagate.
                let params2 = DidSaveTextDocumentParams {
                    text_document: TextDocumentIdentifier { uri: change.uri },
                    text: None,
                };
                self.did_save(params2).await;
            } else if change.typ == FileChangeType::DELETED {
                let uri = change.uri;
                // Remove canonical path from importers map if possible.
                let path_opt = uri.to_file_path().ok()
                    .and_then(|p| p.canonicalize().ok());
                if let Some(path) = path_opt {
                    self.importers.lock().await.remove(&path);
                }
                self.documents.lock().await.remove(&uri);
                self.client.publish_diagnostics(uri, vec![], None).await;
            }
        }
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;

        let docs = self.documents.lock().await;
        let state = match docs.get(&uri) {
            Some(s) => s,
            None    => return Ok(None),
        };
        let symbols = match state.effective_symbols() {
            Some(s) => s,
            None    => return Ok(None),
        };

        let word = word_at_position(&state.source, pos);

        // Case 0: import alias hover
        if !word.is_empty() {
            let imp_sym = state.effective_import_symbols();
            if let Some(imported_sym) = imp_sym.get(word.as_str()) {
                let value = format_import_hover(&word, imported_sym);
                return Ok(Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind:  MarkupKind::Markdown,
                        value,
                    }),
                    range: None,
                }));
            }
        }

        // Case 0b: word is a section/field inside an import path (e.g. hover on `Minor` in `base::Minor::port`)
        if !word.is_empty() {
            if let Some(prefix) = path_prefix_before_word(&state.source, pos) {
                let imp_syms = state.effective_import_symbols();
                if let Some(imported_sym) = imp_syms.get(&prefix[0]) {
                    if prefix.len() == 1 {
                        // hovering on section name: base::[Minor]
                        let sec_path = vec![word.clone()];
                        if let Some(section) = imported_sym.sections.get(&sec_path) {
                            let value = format_hover_section(imported_sym, &sec_path, section);
                            return Ok(Some(Hover {
                                contents: HoverContents::Markup(MarkupContent {
                                    kind: MarkupKind::Markdown, value,
                                }),
                                range: None,
                            }));
                        }
                        // hovering on exported var name: base::[namespace]
                        if let Some(spar::resolver::GlobalEntry::Var { ty, .. }) = imported_sym.globals.get(&word) {
                            let value = format!("```spar\nexport var {}: {}\n```", word, format_spar_type(ty));
                            return Ok(Some(Hover {
                                contents: HoverContents::Markup(MarkupContent {
                                    kind: MarkupKind::Markdown, value,
                                }),
                                range: None,
                            }));
                        }
                        // hovering on function name: base::[main]
                        if let Some(entry) = imported_sym.functions.get(&word) {
                            if !entry.is_private {
                                let params = entry.params.iter()
                                    .map(|(n, t)| format!("{}: {}", n, format_spar_type(t)))
                                    .collect::<Vec<_>>().join(", ");
                                let value = format!("```spar\nfunction {}({}) -> {}\n```", word, params, format_spar_type(&entry.ret));
                                return Ok(Some(Hover {
                                    contents: HoverContents::Markup(MarkupContent {
                                        kind: MarkupKind::Markdown, value,
                                    }),
                                    range: None,
                                }));
                            }
                        }
                    } else {
                        // hovering on field: base::SectionName::[fieldName]
                        let sec_path = prefix[1..].to_vec();
                        if let Some(section) = imported_sym.sections.get(&sec_path) {
                            if let Some(field) = section.fields.get(&word) {
                                let ty_str = field.ty.as_ref().map(format_spar_type)
                                    .unwrap_or_else(|| resolve_field_type_display(imported_sym, &sec_path, &word));
                                let value = format!("```spar\n{}: {}\n```", word, ty_str);
                                return Ok(Some(Hover {
                                    contents: HoverContents::Markup(MarkupContent {
                                        kind: MarkupKind::Markdown, value,
                                    }),
                                    range: None,
                                }));
                            }
                        }
                    }
                }
            }
        }

        // Case 0c: word is a field in a same-file path like Server::rateLimit::[enabled]
        if !word.is_empty() {
            if let Some(prefix) = path_prefix_before_word(&state.source, pos) {
                let imp_syms = state.effective_import_symbols();
                if !imp_syms.contains_key(&prefix[0]) {
                    let prefix_owned: Vec<String> = prefix.iter().map(|s| s.clone()).collect();
                    if let Some(section) = symbols.sections.get(&prefix_owned) {
                        if let Some(field) = section.fields.get(&word) {
                            let ty_str = field.ty.as_ref().map(format_spar_type)
                                .unwrap_or_else(|| resolve_field_type_display(symbols, &prefix_owned, &word));
                            let value = format!(
                                "```spar\n(field) {}: {} in `[{}]`\n```",
                                word,
                                ty_str,
                                prefix_owned.join(".")
                            );
                            return Ok(Some(Hover {
                                contents: HoverContents::Markup(MarkupContent {
                                    kind: MarkupKind::Markdown, value,
                                }),
                                range: None,
                            }));
                        }
                    }
                }
            }
        }

        // Case 1: global variable hover
        if !word.is_empty() {
            if let Some(entry) = symbols.globals.get(&word) {
                let value = format_hover_global(&word, entry);
                return Ok(Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind:  MarkupKind::Markdown,
                        value,
                    }),
                    range: None,
                }));
            }
        }

        // Case 2: top-level section name (path == [word])
        if !word.is_empty() {
            let top_key = vec![word.clone()];
            if let Some(section) = symbols.sections.get(&top_key) {
                let value = format_hover_section(symbols, &top_key, section);
                return Ok(Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind:  MarkupKind::Markdown,
                        value,
                    }),
                    range: None,
                }));
            }
        }

        // Case 3: nested section — word matches a non-first segment of any path
        if !word.is_empty() {
            for (path, section) in &symbols.sections {
                if path.len() > 1 && path.last().map(|s| s.as_str()) == Some(word.as_str()) {
                    let value = format_hover_section(symbols, path, section);
                    return Ok(Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind:  MarkupKind::Markdown,
                            value,
                        }),
                        range: None,
                    }));
                }
            }
        }

        // Case 4: section field — word matches a field name in a section.
        // Use AST to find which section the cursor is actually inside, to avoid
        // returning the wrong section when multiple sections share a field name.
        if !word.is_empty() {
            let offset = lsp_pos_to_byte_offset(&state.source, pos);
            // Find the innermost section whose span contains the cursor.
            let containing_path: Option<Vec<String>> = state.ast.as_ref().and_then(|prog| {
                use spar::ast::TopLevelItem;
                prog.items.iter().filter_map(|item| {
                    if let TopLevelItem::Section(sd) = item {
                        if sd.span.start <= offset && offset <= sd.span.end {
                            Some(sd.path.clone())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }).last() // innermost (last) enclosing section
            });

            // First try the specific containing section, then fall back to any match.
            let field_hover = if let Some(ref path) = containing_path {
                symbols.sections.get(path).and_then(|sec| {
                    sec.fields.get(&word).map(|field| (path.clone(), field))
                })
            } else {
                None
            };
            let field_hover = field_hover.or_else(|| {
                symbols.sections.iter().find_map(|(path, sec)| {
                    sec.fields.get(&word).map(|field| (path.clone(), field))
                })
            });

            if let Some((path, field)) = field_hover {
                let ty_str = field.ty.as_ref().map(format_spar_type)
                    .unwrap_or_else(|| resolve_field_type_display(symbols, &path, &word));
                let value = format!(
                    "```spar\n(field) {}: {} in `[{}]`\n```",
                    word,
                    ty_str,
                    path.join(".")
                );
                return Ok(Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind:  MarkupKind::Markdown,
                        value,
                    }),
                    range: None,
                }));
            }
        }

        // Case 5: user-defined function
        if !word.is_empty() {
            if let Some(entry) = symbols.functions.get(&word) {
                let value = format_hover_function(&word, entry);
                return Ok(Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind:  MarkupKind::Markdown,
                        value,
                    }),
                    range: None,
                }));
            }
        }

        // Case 6: `if` keyword — report branch presence from AST
        if word == "if" {
            if let Some(ast) = &state.ast {
                let offset = lsp_pos_to_byte_offset(&state.source, pos);
                let has_else = find_if_at_offset(ast, offset).unwrap_or(false);
                let desc = if has_else { "then + else branches" } else { "then only (no else)" };
                let value = format!("```spar\nif/else — {}\n```", desc);
                return Ok(Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind:  MarkupKind::Markdown,
                        value,
                    }),
                    range: None,
                }));
            }
        }

        // Case 7: `for` keyword — list comprehension
        if word == "for" {
            let value = "```spar\nfor x in list { expr } — list comprehension\n```".to_string();
            return Ok(Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind:  MarkupKind::Markdown,
                    value,
                }),
                range: None,
            }));
        }

        // Case 8: index expression — find `[...]` at cursor and return element type
        if let Some(ast) = &state.ast {
            let offset = lsp_pos_to_byte_offset(&state.source, pos);
            if let Some(elem_ty) = find_index_elem_type_at_offset(ast, symbols, offset) {
                let value = format!("```spar\n: {}\n```", format_spar_type(&elem_ty));
                return Ok(Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind:  MarkupKind::Markdown,
                        value,
                    }),
                    range: None,
                }));
            }
        }

        Ok(None)
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;

        let docs = self.documents.lock().await;
        let state = match docs.get(&uri) {
            Some(s) => s,
            None    => return Ok(Some(CompletionResponse::Array(keyword_items()))),
        };

        if is_cursor_in_block_comment(&state.source, pos) {
            return Ok(None);
        }

        let symbols = match state.effective_symbols() {
            Some(s) => s,
            None    => return Ok(Some(CompletionResponse::Array(keyword_items()))),
        };

        // Case 1: after `path::` — enumerate section fields or imported symbols
        if let Some(path) = path_before_cursor(&state.source, pos) {
            if let Some(section) = symbols.sections.get(&path) {
                return Ok(Some(CompletionResponse::Array(
                    section_field_completions(symbols, &path, section),
                )));
            }

            if symbols.imports.contains_key(&path[0]) {
                if let Some(imported_sym) = state.effective_import_symbols().get(&path[0]) {
                    if path.len() == 1 {
                        // base:: → top-level exported symbols of imported file
                        return Ok(Some(CompletionResponse::Array(
                            import_completion_items(imported_sym),
                        )));
                    } else {
                        // base::SectionName:: → fields of that section in imported file
                        let section_path = path[1..].to_vec();
                        if let Some(section) = imported_sym.sections.get(&section_path) {
                            return Ok(Some(CompletionResponse::Array(
                                section_field_completions(imported_sym, &section_path, section),
                            )));
                        }
                    }
                }
                return Ok(Some(CompletionResponse::Array(vec![])));
            }

            return Ok(Some(CompletionResponse::Array(vec![])));
        }

        // Case 2: after bare `:` — type annotation position → offer type keywords
        if is_in_type_position(&state.source, pos) {
            return Ok(Some(CompletionResponse::Array(type_keyword_items())));
        }

        // Case 3: default — keywords + builtins + user functions + globals + sections + imports
        let mut items = keyword_items();
        items.extend(builtin_items());
        items.extend(function_completion_items(&symbols.functions));

        for (name, entry) in &symbols.globals {
            let detail = match entry {
                spar::resolver::GlobalEntry::Var { ty, .. } => Some(format_spar_type(ty)),
                spar::resolver::GlobalEntry::Dynamic { .. } => Some("dynamic".to_string()),
            };
            items.push(CompletionItem {
                label:  name.clone(),
                kind:   Some(CompletionItemKind::VARIABLE),
                detail,
                ..Default::default()
            });
        }

        for path in symbols.sections.keys() {
            if let Some(first) = path.first() {
                items.push(CompletionItem {
                    label: first.clone(),
                    kind:  Some(CompletionItemKind::MODULE),
                    ..Default::default()
                });
            }
        }

        for alias in symbols.imports.keys() {
            items.push(CompletionItem {
                label: alias.clone(),
                kind:  Some(CompletionItemKind::MODULE),
                ..Default::default()
            });
        }

        Ok(Some(CompletionResponse::Array(items)))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri;
        let docs = self.documents.lock().await;
        let state = match docs.get(&uri) {
            Some(s) => s,
            None    => return Ok(None),
        };
        let program = match &state.ast {
            Some(p) => p,
            None    => return Ok(None),
        };

        let mut raw: Vec<RawToken> = Vec::new();
        collect_tokens_from_program(program, &state.source, &mut raw);
        raw.sort_by_key(|t| (t.line, t.start_char));

        let mut data: Vec<SemanticToken> = Vec::with_capacity(raw.len());
        let mut prev_line = 0u32;
        let mut prev_char = 0u32;
        for tok in &raw {
            let delta_line = tok.line - prev_line;
            let delta_char = if delta_line == 0 { tok.start_char - prev_char } else { tok.start_char };
            data.push(SemanticToken {
                delta_line,
                delta_start:            delta_char,
                length:                 tok.length,
                token_type:             tok.token_type,
                token_modifiers_bitset: tok.modifiers,
            });
            prev_line = tok.line;
            prev_char = tok.start_char;
        }

        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens { result_id: None, data })))
    }

    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let uri  = params.text_document.uri;
        let docs = self.documents.lock().await;
        let state = match docs.get(&uri) {
            Some(s) => s,
            None    => return Ok(None),
        };
        let program = match &state.ast {
            Some(p) => p,
            None    => return Ok(None), // parse failed — fail soft, not as LSP error
        };

        let formatted = match format_source(&state.source) {
            Ok(s) => s,
            Err(_) => format_program(program, &FormatConfig::default()),
        };

        // Compute end-of-document position covering the full source including any trailing newline.
        // str::lines() absorbs a trailing '\n', so we must account for it explicitly.
        let source = &state.source;
        let line_count = source.lines().count();
        let (end_line, end_char) = if source.ends_with('\n') {
            // Place the end marker at the start of the (virtual) line after the last '\n',
            // so the TextEdit range covers the trailing newline itself.
            (line_count as u32, 0)
        } else {
            let last_line = source.lines().last().unwrap_or("");
            ((line_count.saturating_sub(1)) as u32, last_line.len() as u32)
        };

        let full_range = Range {
            start: Position { line: 0, character: 0 },
            end:   Position { line: end_line, character: end_char },
        };

        Ok(Some(vec![TextEdit { range: full_range, new_text: formatted }]))
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let stdin  = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| KlLanguageServer {
        client,
        documents:      Mutex::new(HashMap::new()),
        importers:      Mutex::new(HashMap::new()),
        workspace_root: Mutex::new(None),
    });

    Server::new(stdin, stdout, socket).serve(service).await;
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    #[test]
    fn block_comment_detection_outside_comment() {
        let src = "var x = 1;";
        assert!(!is_cursor_in_block_comment(src, pos(0, 5)));
    }

    #[test]
    fn block_comment_detection_inside_closed_comment() {
        let src = "/* hello */ var x = 1;";
        assert!(!is_cursor_in_block_comment(src, pos(0, 15)));
    }

    #[test]
    fn block_comment_detection_inside_open_comment() {
        let src = "/* hello\nworld";
        assert!(is_cursor_in_block_comment(src, pos(1, 2)));
    }

    #[test]
    fn block_comment_detection_nested() {
        let src = "/* outer /* inner */";
        // cursor after inner closes — still in outer
        assert!(is_cursor_in_block_comment(src, pos(0, 20)));
    }

    #[test]
    fn block_comment_detection_nested_fully_closed() {
        let src = "/* outer /* inner */ */ var";
        assert!(!is_cursor_in_block_comment(src, pos(0, 24)));
    }

    #[test]
    fn block_comment_detection_slash_star_in_string_ignored() {
        let src = "var s = \"/* not a comment\"; var";
        assert!(!is_cursor_in_block_comment(src, pos(0, 28)));
    }

    #[test]
    fn block_comment_detection_slash_star_in_line_comment_ignored() {
        let src = "// /* not block\nvar x";
        assert!(!is_cursor_in_block_comment(src, pos(1, 3)));
    }

    fn span(line: u32, col: u32, start: usize, end: usize) -> Span {
        Span { line, col, start, end }
    }

    #[test]
    fn lex_error_produces_correct_range_and_message() {
        let err = SparError::LexError {
            message: "unexpected character '@'".to_string(),
            span:    span(3, 5, 42, 43),
        };
        let diag = spar_error_to_diagnostic(&err);
        assert_eq!(diag.range.start.line,      2, "line must be span.line - 1");
        assert_eq!(diag.range.start.character, 4, "char must be span.col - 1");
        assert_eq!(diag.message, "unexpected character '@'");
        assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diag.source.as_deref(), Some("spar"));
    }

    #[test]
    fn first_line_first_column_maps_to_position_zero() {
        let err = SparError::ParseError {
            message: "unexpected EOF".to_string(),
            span:    span(1, 1, 0, 1),
        };
        let diag = spar_error_to_diagnostic(&err);
        assert_eq!(diag.range.start.line,      0);
        assert_eq!(diag.range.start.character, 0);
    }

    #[test]
    fn resolve_error_with_hint_appends_hint_to_message() {
        let err = SparError::ResolveError {
            message: "undefined variable 'por'".to_string(),
            hint:    Some("did you mean 'port'?".to_string()),
            span:    span(2, 5, 20, 23),
        };
        let diag = spar_error_to_diagnostic(&err);
        assert!(diag.message.contains("undefined variable 'por'"));
        assert!(diag.message.contains("did you mean 'port'?"));
    }

    #[test]
    fn resolve_error_without_hint_has_no_hint_text() {
        let err = SparError::ResolveError {
            message: "duplicate name 'port'".to_string(),
            hint:    None,
            span:    span(1, 1, 0, 4),
        };
        let diag = spar_error_to_diagnostic(&err);
        assert!(!diag.message.contains("Hint:"));
    }

    #[test]
    fn type_error_with_hint_maps_range_correctly() {
        let err = SparError::TypeError {
            message: "expected int, got str".to_string(),
            hint:    Some("consider using str()".to_string()),
            span:    span(5, 10, 60, 65),
        };
        let diag = spar_error_to_diagnostic(&err);
        assert_eq!(diag.range.start.line,      4);
        assert_eq!(diag.range.start.character, 9);
        assert!(diag.message.contains("consider using str()"));
    }

    #[test]
    fn eval_error_maps_as_warning() {
        let err = SparError::EvalError {
            message: "division by zero".to_string(),
            span:    span(10, 20, 200, 201),
        };
        let diag = spar_error_to_diagnostic(&err);
        assert_eq!(diag.range.start.line,      9);
        assert_eq!(diag.range.start.character, 19);
        assert_eq!(diag.severity, Some(DiagnosticSeverity::WARNING));
    }

    #[test]
    fn word_at_middle_of_identifier() {
        let src = "var port: int = 8080;";
        assert_eq!(word_at_position(src, Position { line: 0, character: 5 }), "port");
    }

    #[test]
    fn word_at_start_of_identifier() {
        let src = "var port: int = 8080;";
        assert_eq!(word_at_position(src, Position { line: 0, character: 4 }), "port");
    }

    #[test]
    fn word_at_operator_returns_empty() {
        let src = "var port: int = 8080;";
        assert_eq!(word_at_position(src, Position { line: 0, character: 8 }), "");
    }

    #[test]
    fn word_on_nonexistent_line_returns_empty() {
        let src = "var port: int = 8080;";
        assert_eq!(word_at_position(src, Position { line: 99, character: 0 }), "");
    }

    #[test]
    fn word_at_character_beyond_line_end_returns_empty() {
        let src = "var x;";
        assert_eq!(word_at_position(src, Position { line: 0, character: 100 }), "");
    }

    // ── path_before_cursor ────────────────────────────────────────────────────

    #[test]
    fn path_detected_when_cursor_follows_double_colon() {
        let src = "var x = database::";
        assert_eq!(
            path_before_cursor(src, Position { line: 0, character: 18 }),
            Some(vec!["database".to_string()])
        );
    }

    #[test]
    fn path_multi_segment_detected() {
        let src = "var x = foo::bar::";
        assert_eq!(
            path_before_cursor(src, Position { line: 0, character: 18 }),
            Some(vec!["foo".to_string(), "bar".to_string()])
        );
    }

    #[test]
    fn path_none_when_no_double_colon_present() {
        let src = "var x = something";
        assert_eq!(
            path_before_cursor(src, Position { line: 0, character: 17 }),
            None
        );
    }

    #[test]
    fn path_none_when_single_colon_only() {
        let src = "var x:";
        assert_eq!(
            path_before_cursor(src, Position { line: 0, character: 6 }),
            None
        );
    }

    #[test]
    fn path_on_nonexistent_line_returns_none() {
        let src = "var x = database::";
        assert_eq!(
            path_before_cursor(src, Position { line: 5, character: 0 }),
            None
        );
    }

    // ── is_in_type_position ───────────────────────────────────────────────────

    #[test]
    fn type_position_detected_after_colon() {
        let src = "var x:";
        assert!(is_in_type_position(src, Position { line: 0, character: 6 }));
    }

    #[test]
    fn type_position_detected_after_colon_with_space() {
        let src = "var x: ";
        assert!(is_in_type_position(src, Position { line: 0, character: 7 }));
    }

    #[test]
    fn type_position_false_after_double_colon() {
        let src = "var x = Database::";
        assert!(!is_in_type_position(src, Position { line: 0, character: 18 }));
    }

    #[test]
    fn type_position_false_with_no_colon() {
        let src = "var x";
        assert!(!is_in_type_position(src, Position { line: 0, character: 5 }));
    }

    #[test]
    fn type_position_false_on_nonexistent_line() {
        let src = "var x:";
        assert!(!is_in_type_position(src, Position { line: 99, character: 0 }));
    }

    // ── lsp_pos_to_byte_offset ────────────────────────────────────────────────

    #[test]
    fn byte_offset_first_line() {
        let src = "hello\nworld";
        assert_eq!(lsp_pos_to_byte_offset(src, Position { line: 0, character: 3 }), 3);
    }

    #[test]
    fn byte_offset_second_line() {
        let src = "hello\nworld";
        // "hello\n" = 6 bytes, so line 1 starts at 6
        assert_eq!(lsp_pos_to_byte_offset(src, Position { line: 1, character: 2 }), 8);
    }

    // ── format_spar_type ────────────────────────────────────────────────────────

    #[test]
    fn format_type_handles_all_kltype_variants() {
        assert_eq!(format_spar_type(&SparType::Int), "int");
        assert_eq!(format_spar_type(&SparType::Float), "float");
        assert_eq!(format_spar_type(&SparType::Str), "str");
        assert_eq!(format_spar_type(&SparType::Bool), "bool");
        assert_eq!(format_spar_type(&SparType::Section), "section");
        assert_eq!(format_spar_type(&SparType::List(Box::new(SparType::Int))), "[int]");
        assert_eq!(
            format_spar_type(&SparType::List(Box::new(SparType::List(Box::new(SparType::Str))))),
            "[[str]]"
        );
    }

    // ── spar_error_to_diagnostic hint inclusion ────────────────────────────────

    #[test]
    fn klerror_to_diagnostic_includes_hint_when_present() {
        let err = SparError::TypeError {
            message: "type mismatch".into(),
            hint:    Some("try converting explicitly".into()),
            span:    span(1, 1, 0, 1),
        };
        let diag = spar_error_to_diagnostic(&err);
        assert!(diag.message.contains("try converting explicitly"));
        assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
    }

    // ── keyword_items completeness ────────────────────────────────────────────

    #[test]
    fn keyword_items_includes_all_control_and_decl_keywords() {
        let items = keyword_items();
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        for kw in &["if", "else", "for", "in", "return", "function", "var", "export", "private"] {
            assert!(labels.contains(kw), "missing keyword: {kw}");
        }
    }

    // ── builtin_items ─────────────────────────────────────────────────────────

    #[test]
    fn builtin_items_has_all_five_builtins_with_signatures() {
        let items = builtin_items();
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        for name in &["env", "int", "float", "str", "bool"] {
            assert!(labels.contains(name), "missing builtin: {name}");
        }
        for item in &items {
            assert!(item.detail.is_some(), "builtin {} missing detail", item.label);
        }
    }

    // ── find_if_at_offset ─────────────────────────────────────────────────────

    #[test]
    fn find_if_detects_then_else_branch_presence() {
        let src = "function f(x: bool) -> int {\n    if x {\n        return 1;\n    } else {\n        return 0;\n    }\n}";
        let tokens = spar::lexer::Lexer::new(src).tokenize().unwrap();
        let program = spar::parser::Parser::new(tokens).parse().unwrap();
        // "if" keyword is at byte offset 29 (line 1, col 4)
        let if_offset = src.find("if x").unwrap();
        let result = find_if_at_offset(&program, if_offset);
        assert_eq!(result, Some(true), "should detect else branch");
    }

    #[test]
    fn find_if_detects_then_only_no_else() {
        let src = "function f(x: bool) -> int {\n    if x {\n        return 1;\n    }\n    return 0;\n}";
        let tokens = spar::lexer::Lexer::new(src).tokenize().unwrap();
        let program = spar::parser::Parser::new(tokens).parse().unwrap();
        let if_offset = src.find("if x").unwrap();
        let result = find_if_at_offset(&program, if_offset);
        assert_eq!(result, Some(false), "should detect no else branch");
    }

    // ── four-segment path produces zero diagnostics ───────────────────────────

    #[test]
    fn four_segment_namespace_path_produces_no_diagnostics() {
        let src = "[A]{\n    b: section = {\n        c: section = {\n            d: section = {\n                e: int = 1;\n            };\n        };\n    };\n};\nvar x: int = A::b::c::d::e;";
        let state = KlLanguageServer::analyze(src, std::path::Path::new("."));
        let diags = state.diagnostics();
        assert!(
            diags.is_empty(),
            "expected no diagnostics for 4-segment path, got: {:?}",
            diags.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    // ── formatting helpers ────────────────────────────────────────────────────

    #[test]
    fn formatting_returns_none_for_unparseable_source() {
        let src = "this is not valid keel {{{";
        // Simulate what formatting() does internally: try parse, expect None on failure.
        let tokens_result = spar::lexer::Lexer::new(src).tokenize();
        let parse_ok = tokens_result.ok()
            .and_then(|t| spar::parser::Parser::new(t).parse().ok())
            .is_some();
        assert!(!parse_ok, "unparseable source must not produce a program");
    }

    #[test]
    fn end_of_document_position_single_line() {
        // Source WITHOUT trailing newline: end is at last char of content
        let src = "var x: int = 1;";
        let lines: Vec<&str> = src.lines().collect();
        let last_line = lines.last().copied().unwrap_or("");
        let end_pos = Position {
            line: (lines.len() as u32).saturating_sub(1),
            character: last_line.len() as u32,
        };
        assert_eq!(end_pos.line, 0);
        assert_eq!(end_pos.character, 15);
    }

    #[test]
    fn end_of_document_position_with_trailing_newline() {
        // Source WITH trailing newline: end must be (line_count, 0) to cover the '\n'
        let src = "var x: int = 1;\n";
        let line_count = src.lines().count(); // 1 (str::lines absorbs trailing '\n')
        let (end_line, end_char) = if src.ends_with('\n') {
            (line_count as u32, 0)
        } else {
            let last = src.lines().last().unwrap_or("");
            ((line_count.saturating_sub(1)) as u32, last.len() as u32)
        };
        assert_eq!(end_line, 1, "end line must be 1 (past content line) to cover trailing \\n");
        assert_eq!(end_char, 0);
    }

    #[test]
    fn formatting_produces_expected_text_edit() {
        let messy = "var   x:int=1;";
        let tokens = spar::lexer::Lexer::new(messy).tokenize().unwrap();
        let program = spar::parser::Parser::new(tokens).parse().unwrap();
        let formatted = spar::formatter::format_program(&program, &spar::formatter::FormatConfig::default());
        // format_program always appends a trailing '\n'
        assert_eq!(formatted, "var x: int = 1;\n", "format_program output must end with exactly one newline");
    }

    // ── semantic token helpers ────────────────────────────────────────────────

    struct DecodedToken {
        line:       u32,
        start_char: u32,
        length:     u32,
        token_type: u32,
    }

    fn decode_semantic_tokens(src: &str) -> Vec<DecodedToken> {
        let tokens = spar::lexer::Lexer::new(src).tokenize().unwrap();
        let program = spar::parser::Parser::new(tokens).parse().unwrap();
        let mut raw: Vec<RawToken> = Vec::new();
        collect_tokens_from_program(&program, src, &mut raw);
        raw.sort_by_key(|t| (t.line, t.start_char));
        // RawToken has absolute positions — no delta decoding needed here.
        raw.iter().map(|t| DecodedToken {
            line:       t.line,
            start_char: t.start_char,
            length:     t.length,
            token_type: t.token_type,
        }).collect()
    }

    fn find_tok<'a>(tokens: &'a [DecodedToken], text: &str, src: &str) -> Option<&'a DecodedToken> {
        let lines: Vec<&str> = src.lines().collect();
        tokens.iter().find(|t| {
            let ln = t.line as usize;
            if ln >= lines.len() { return false; }
            let line  = lines[ln];
            let start = t.start_char as usize;
            let end   = start + t.length as usize;
            if start > line.len() || end > line.len() { return false; }
            &line[start..end] == text
        })
    }

    #[test]
    fn semantic_tokens_var_decl_classified_as_variable_with_declaration_modifier() {
        let src = "var myVar: int = 5;";
        let tokens = decode_semantic_tokens(src);
        let tok = find_tok(&tokens, "myVar", src).expect("myVar not found");
        assert_eq!(tok.token_type, TT_VARIABLE);
        // modifiers are not in DecodedToken above; just verify position/type
    }

    #[test]
    fn semantic_tokens_function_decl_classified_as_function() {
        let src = "function pickPort(debug: bool) -> int { return 9000; }";
        let tokens = decode_semantic_tokens(src);
        let tok = find_tok(&tokens, "pickPort", src).expect("pickPort not found");
        assert_eq!(tok.token_type, TT_FUNCTION);
    }

    #[test]
    fn semantic_tokens_function_call_site_classified_as_function() {
        let src = "function f(x: int) -> int { return x; }\nvar y: int = f(x: 1);";
        let tokens = decode_semantic_tokens(src);
        let fn_toks: Vec<_> = tokens.iter().filter(|t| t.token_type == TT_FUNCTION).collect();
        assert!(fn_toks.len() >= 2, "expected decl + call site, got {}", fn_toks.len());
    }

    #[test]
    fn semantic_tokens_section_name_classified_as_namespace() {
        let src = "[Server]{ host: str = \"x\"; };";
        let tokens = decode_semantic_tokens(src);
        let tok = find_tok(&tokens, "Server", src).expect("Server not found");
        assert_eq!(tok.token_type, TT_NAMESPACE);
    }

    #[test]
    fn semantic_tokens_section_field_classified_as_property() {
        let src = "[Server]{ host: str = \"x\"; };";
        let tokens = decode_semantic_tokens(src);
        let tok = find_tok(&tokens, "host", src).expect("host not found");
        assert_eq!(tok.token_type, TT_PROPERTY);
    }

    #[test]
    fn semantic_tokens_param_classified_as_parameter() {
        let src = "function f(myParam: int) -> int { return myParam; }";
        let tokens = decode_semantic_tokens(src);
        let tok = find_tok(&tokens, "myParam", src).expect("myParam not found");
        assert_eq!(tok.token_type, TT_PARAMETER);
    }

    #[test]
    fn semantic_tokens_variable_reference_in_body_classified_as_variable() {
        let src = "var x: int = 1;\nvar y: int = x;";
        let tokens = decode_semantic_tokens(src);
        let var_toks: Vec<_> = tokens.iter()
            .filter(|t| t.token_type == TT_VARIABLE)
            .collect();
        assert!(var_toks.len() >= 2, "expected x decl + x ref, got {}", var_toks.len());
    }

    #[test]
    fn semantic_tokens_type_decl_name_classified_as_type() {
        let src = "type [PostgresType]{ image: str; }";
        let tokens = decode_semantic_tokens(src);
        let tok = find_tok(&tokens, "PostgresType", src).expect("PostgresType not found");
        assert_eq!(tok.token_type, TT_TYPE);
    }

    #[test]
    fn semantic_tokens_named_type_field_reference_classified_as_type() {
        let src = "type [Border]{ width: int; }\ntype [Decoration]{ border: Border; }";
        let tokens = decode_semantic_tokens(src);
        let lines: Vec<&str> = src.lines().collect();
        let border_type_toks = tokens.iter().filter(|t| {
            let ln = t.line as usize;
            if ln >= lines.len() { return false; }
            let line = lines[ln];
            let start = t.start_char as usize;
            let end = start + t.length as usize;
            end <= line.len() && &line[start..end] == "Border" && t.token_type == TT_TYPE
        }).count();
        // One for the `[Border]` declaration, one for the `border: Border;` reference.
        assert_eq!(border_type_toks, 2, "expected both the Border declaration and its reference to be TT_TYPE");
    }

    #[test]
    fn semantic_tokens_section_type_binding_classified_as_type() {
        let src = "type [Human]{ name: str; }\n[Man] -> Human {\n    name: \"John\";\n};";
        let tokens = decode_semantic_tokens(src);
        let tok = find_tok(&tokens, "Human", src).filter(|t| t.token_type == TT_TYPE);
        assert!(tok.is_some(), "expected the `-> Human` binding to be classified as a type");
    }

    #[test]
    fn semantic_tokens_selectively_imported_names_classified_by_real_kind() {
        // The user's actual ask: "even in the import system" — a
        // selectively-imported name should get its real semantic color,
        // not fall through as plain text. After expand_imports splices
        // the requested names in, they're ordinary Type/Section
        // declarations positioned at their own spot in the import braces
        // (see spar's retag_top_level_span), so they get colored exactly
        // like a locally-declared type/section would.
        use std::fs;
        let dir = std::env::temp_dir().join(format!("spar_ls_import_tok_test_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("shared.spar"),
            "export type [PostgresType]{ image: str; }\nexport [Colors]{ red: str = \"#f00\"; };\n",
        ).unwrap();
        let src = "import { PostgresType, Colors } from \"shared.spar\";\n";
        let tokens = spar::lexer::Lexer::new(src).tokenize().unwrap();
        let mut program = spar::parser::Parser::new(tokens).parse().unwrap();
        let mut loader = spar::loader::ImportLoader::new(&dir);
        spar::loader::expand_imports(&mut program, &mut loader).expect("expand must succeed");

        let mut raw: Vec<RawToken> = Vec::new();
        collect_tokens_from_program(&program, src, &mut raw);
        raw.sort_by_key(|t| (t.line, t.start_char));

        assert!(raw.iter().any(|t| t.token_type == TT_TYPE),
            "expected the imported PostgresType to get a TT_TYPE token");
        assert!(raw.iter().any(|t| t.token_type == TT_NAMESPACE),
            "expected the imported Colors section to get a TT_NAMESPACE token");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn semantic_tokens_schema_section_name_classified_as_type() {
        let src = "@SchemaFile\nSchema [Container]{ x?: str; }\n";
        let tokens = decode_semantic_tokens(src);
        let tok = find_tok(&tokens, "Container", src).expect("Container not found");
        assert_eq!(tok.token_type, TT_TYPE);
    }

    fn resolve_src(src: &str) -> SymbolTable {
        let tokens = Lexer::new(src).tokenize().expect("lex");
        let prog = Parser::new(tokens).parse().expect("parse");
        Resolver::new().resolve(&prog, &[]).expect("resolve")
    }

    #[test]
    fn resolve_field_type_display_resolves_top_level_inferred_field() {
        let src = concat!(
            "type [Human]{ name: str; age: int; }\n",
            "[Man] -> Human {\n",
            "    name: \"John\";\n",
            "    age: 29;\n",
            "};\n",
        );
        let symbols = resolve_src(src);
        let path = vec!["Man".to_string()];
        assert_eq!(resolve_field_type_display(&symbols, &path, "name"), "str");
        assert_eq!(resolve_field_type_display(&symbols, &path, "age"), "int");
    }

    #[test]
    fn resolve_field_type_display_resolves_nested_inferred_field() {
        let src = concat!(
            "type [EnvironmentType]{ nodeEnv: str; port: str; }\n",
            "type [ServiceType]{ image: str; environment: EnvironmentType; }\n",
            "[Api] -> ServiceType {\n",
            "    image: \"my-api\";\n",
            "    environment: {\n",
            "        nodeEnv: \"production\";\n",
            "        port: \"3000\";\n",
            "    };\n",
            "};\n",
        );
        let symbols = resolve_src(src);
        let path = vec!["Api".to_string(), "environment".to_string()];
        assert_eq!(resolve_field_type_display(&symbols, &path, "nodeEnv"), "str");
        assert_eq!(resolve_field_type_display(&symbols, &path, "port"), "str");
    }

    #[test]
    fn resolve_field_type_display_named_shape_shows_type_name() {
        let src = concat!(
            "type [Border]{ width: int; }\n",
            "type [Decoration]{ border: Border; }\n",
            "[Style] -> Decoration {\n",
            "    border: { width: 2; };\n",
            "};\n",
        );
        let symbols = resolve_src(src);
        let path = vec!["Style".to_string()];
        assert_eq!(resolve_field_type_display(&symbols, &path, "border"), "Border");
    }
}

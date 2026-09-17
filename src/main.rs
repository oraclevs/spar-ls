//! Spar Language Server — speaks LSP over stdio.

mod diagnostics;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use diagnostics::spar_error_to_diagnostic;
use spar::ast::{FuncStmt, Program, SparType, TopLevelItem};
use spar::evaluator::{EvalResult, Evaluator};
use spar::formatter::{format_program, format_source, FormatConfig};
use spar::lexer::Lexer;
use spar::loader::{collect_imports, expand_imports, validate_schema_imports, ImportLoader};
use spar::parser::Parser;
use spar::resolver::{
    EnumEntry, FunctionEntry, FunctionGroupEntry, GlobalEntry, Resolver, SectionEntry, SymbolTable,
    TypeEntry,
};
use spar::typechecker::TypeChecker;
use spar::{Compilation, CompileOptions, Compiler, Span, SparError};

use tokio::sync::Mutex;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

// ── Type display helper ───────────────────────────────────────────────────────

fn format_spar_type(ty: &SparType) -> String {
    match ty {
        SparType::Str => "str".to_string(),
        SparType::Int => "int".to_string(),
        SparType::Float => "float".to_string(),
        SparType::Bool => "bool".to_string(),
        SparType::List(inner) => format!("List<{}>", format_spar_type(inner)),
        SparType::Section => "section".to_string(),
        SparType::Void => "void".to_string(),
        SparType::Shell => "shell".to_string(),
        SparType::Error => "error".to_string(),
        SparType::Named(name) => name.clone(),
        SparType::TypeParameter(name) => name.clone(),
        SparType::Applied { name, arguments } => format!(
            "{}<{}>",
            name,
            arguments
                .iter()
                .map(format_spar_type)
                .collect::<Vec<_>>()
                .join(", ")
        ),
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
fn resolve_shape_for_path(
    symbols: &SymbolTable,
    path: &[String],
) -> Option<Vec<spar::ast::TypeField>> {
    let top = symbols.sections.get(&vec![path.first()?.clone()])?;
    let type_name = top.type_binding.as_ref()?;
    let type_name = match type_name {
        SparType::Named(name) => name,
        SparType::Applied { name, .. } => name,
        _ => return None,
    };
    let mut fields = symbols.types.get(type_name)?.fields.clone();
    for seg in &path[1..] {
        let tf = fields.iter().find(|f| &f.name == seg)?;
        fields = match &tf.shape {
            spar::ast::TypeFieldShape::Section(nested) => nested.clone(),
            spar::ast::TypeFieldShape::Named(other) => symbols.types.get(other)?.fields.clone(),
            spar::ast::TypeFieldShape::TypeParameter(_) => return None,
            spar::ast::TypeFieldShape::Applied { name, .. } => {
                symbols.types.get(name)?.fields.clone()
            }
            spar::ast::TypeFieldShape::Primitive(_) => return None,
        };
    }
    Some(fields)
}

fn format_type_field_shape(shape: &spar::ast::TypeFieldShape) -> String {
    match shape {
        spar::ast::TypeFieldShape::Primitive(ty) => format_spar_type(ty),
        spar::ast::TypeFieldShape::Section(_) => "section".to_string(),
        // A Named shape's own type name is more useful than a bare
        // "section" — e.g. "PostgresType" tells the reader where to look.
        spar::ast::TypeFieldShape::Named(name) => name.clone(),
        spar::ast::TypeFieldShape::TypeParameter(name) => name.clone(),
        spar::ast::TypeFieldShape::Applied { name, arguments } => format!(
            "{}<{}>",
            name,
            arguments
                .iter()
                .map(format_spar_type)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The display string for a field whose `FieldEntry.ty` is `None` —
/// resolves the real type from the binding chain. Falls back to
/// "section" only if nothing in the chain can be traced (shouldn't
/// normally happen for valid code, since `ty: None` only parses under a
/// binding in the first place).
fn resolve_field_type_display(symbols: &SymbolTable, path: &[String], field_name: &str) -> String {
    resolve_shape_for_path(symbols, path)
        .and_then(|fields| {
            fields
                .iter()
                .find(|f| f.name == field_name)
                .map(|tf| format_type_field_shape(&tf.shape))
        })
        .unwrap_or_else(|| "section".to_string())
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
        while i > 0 && is_ident(chars[i - 1]) {
            i -= 1;
        }
        i
    };
    let end = {
        let mut i = char_idx;
        while i < chars.len() && is_ident(chars[i]) {
            i += 1;
        }
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
    if line_idx >= lines.len() {
        return None;
    }
    let chars: Vec<char> = lines[line_idx].chars().collect();
    // Skip back over the current word
    let mut i = char_idx.min(chars.len());
    while i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_') {
        i -= 1;
    }
    // Must be preceded by `::`
    if i < 2 || chars[i - 1] != ':' || chars[i - 2] != ':' {
        return None;
    }
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
    if segments.is_empty() {
        None
    } else {
        Some(segments)
    }
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
        () => {
            cur_line > target_line || (cur_line == target_line && cur_col >= target_char)
        };
    }
    macro_rules! advance_char {
        ($b:expr) => {
            if $b == b'\n' {
                cur_line += 1;
                cur_col = 0;
            } else {
                cur_col += 1;
            }
            i += 1;
        };
    }

    while i < bytes.len() && !past_cursor!() {
        // Skip string literals when not inside a block comment.
        if depth == 0 && bytes[i] == b'"' {
            cur_col += 1;
            i += 1;
            while i < bytes.len() && !past_cursor!() && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    advance_char!(bytes[i]);
                }
                if i < bytes.len() && !past_cursor!() {
                    advance_char!(bytes[i]);
                }
            }
            if i < bytes.len() && !past_cursor!() {
                cur_col += 1;
                i += 1;
            }
            continue;
        }
        // Skip line comments when not inside a block comment.
        if depth == 0 && i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < bytes.len() && !past_cursor!() && bytes[i] != b'\n' {
                cur_col += 1;
                i += 1;
            }
            continue;
        }
        // Block comment open.
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            depth += 1;
            cur_col += 2;
            i += 2;
            continue;
        }
        // Block comment close.
        if depth > 0 && i + 1 < bytes.len() && bytes[i] == b'*' && bytes[i + 1] == b'/' {
            depth -= 1;
            cur_col += 2;
            i += 2;
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

    if segments.is_empty() {
        None
    } else {
        Some(segments)
    }
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

include!("hover.rs");
include!("completion.rs");
include!("definition.rs");
include!("references.rs");
include!("semantic_tokens.rs");
// ── Import hover / completion helpers ────────────────────────────────────────

fn format_import_hover(alias: &str, sym: &SymbolTable) -> String {
    let mut lines: Vec<String> = vec![format!("// import alias: {}", alias)];
    for (name, entry) in &sym.functions {
        if !entry.is_private {
            let params = entry
                .params
                .iter()
                .map(|(n, t)| format!("{}: {}", n, format_spar_type(t)))
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(format!(
                "function {}({}) -> {}",
                name,
                params,
                format_spar_type(&entry.ret)
            ));
        }
    }
    for (path, section) in &sym.sections {
        if section.exported && !section.private {
            let field_names: Vec<_> = section.fields.keys().cloned().collect();
            lines.push(format!(
                "[{}] {{ {} }}",
                path.join("."),
                field_names.join(", ")
            ));
        }
    }
    for (name, entry) in &sym.globals {
        if let GlobalEntry::Var {
            exported: true, ty, ..
        } = entry
        {
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
                    label: name.clone(),
                    kind: Some(CompletionItemKind::MODULE),
                    detail: Some(format!("section ({})", fields.join(", "))),
                    ..Default::default()
                });
            }
        }
    }
    for (name, entry) in &sym.functions {
        if !entry.is_private {
            let params = entry
                .params
                .iter()
                .map(|(n, t)| format!("{}: {}", n, format_spar_type(t)))
                .collect::<Vec<_>>()
                .join(", ");
            items.push(CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some(format!("({}) -> {}", params, format_spar_type(&entry.ret))),
                insert_text: Some(format!("{}($1)", name)),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            });
        }
    }
    for (name, entry) in &sym.globals {
        if let GlobalEntry::Var {
            exported: true, ty, ..
        } = entry
        {
            items.push(CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::VARIABLE),
                detail: Some(format_spar_type(ty)),
                ..Default::default()
            });
        }
    }
    items
}

include!("document.rs");

// ── LSP helpers ───────────────────────────────────────────────────────────────

fn base_dir_from_uri(uri: &Url) -> std::path::PathBuf {
    uri.to_file_path()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

fn analyze_single_file(
    source: &str,
    program: Program,
    mut errors: Vec<SparError>,
) -> DocumentState {
    let sym = match Resolver::new().resolve(&program, &[]) {
        Ok(s) => s,
        Err(e) => {
            errors.extend(e);
            return DocumentState {
                source: source.to_string(),
                ast: Some(program),
                symbols: None,
                import_symbols: HashMap::new(),
                spliced_import_decls: Vec::new(),
                spliced_import_symbols: HashMap::new(),
                result: None,
                errors,
                last_good_symbols: None,
                last_good_import_symbols: HashMap::new(),
            };
        }
    };
    if let Err(e) = TypeChecker::check(&program, &sym) {
        errors.extend(e);
    }
    DocumentState {
        source: source.to_string(),
        ast: Some(program),
        symbols: Some(sym),
        import_symbols: HashMap::new(),
        spliced_import_decls: Vec::new(),
        spliced_import_symbols: HashMap::new(),
        result: None,
        errors,
        last_good_symbols: None,
        last_good_import_symbols: HashMap::new(),
    }
}

/// Returns the canonical absolute paths of all non-schema imports declared in `program`,
/// resolved relative to `base_dir`.
fn extract_imported_paths(
    program: &spar::ast::Program,
    base_dir: &std::path::Path,
) -> Vec<PathBuf> {
    use spar::ast::{ImportKind, TopLevelItem};
    program
        .items
        .iter()
        .filter_map(|item| {
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
        })
        .collect()
}

include!("backend.rs");
// ── LanguageServer implementation ─────────────────────────────────────────────

#[tower_lsp::async_trait]
impl LanguageServer for SparLanguageServer {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // Store workspace root for later file scan.
        let root = params
            .root_uri
            .and_then(|u| u.to_file_path().ok())
            .or_else(|| {
                params
                    .workspace_folders
                    .as_deref()
                    .and_then(|wf| wf.first())
                    .and_then(|f| f.uri.to_file_path().ok())
            });
        *self.workspace_root.lock().await = root;

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(false),
                        })),
                        ..Default::default()
                    },
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![
                        ":".to_string(),
                        "{".to_string(),
                        ".".to_string(),
                    ]),
                    resolve_provider: Some(false),
                    ..Default::default()
                }),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend: SemanticTokensLegend {
                                token_types: TOKEN_TYPES.to_vec(),
                                token_modifiers: TOKEN_MODIFIERS.to_vec(),
                            },
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                            range: Some(false),
                            ..Default::default()
                        },
                    ),
                ),
                document_formatting_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "spar-ls".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "spar-ls initialized")
            .await;

        // Register watcher for external file changes.
        let registration = Registration {
            id: "spar-file-watcher".to_string(),
            method: "workspace/didChangeWatchedFiles".to_string(),
            register_options: Some(
                serde_json::to_value(DidChangeWatchedFilesRegistrationOptions {
                    watchers: vec![FileSystemWatcher {
                        glob_pattern: GlobPattern::String("**/*.spar".to_string()),
                        kind: None,
                    }],
                })
                .unwrap(),
            ),
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
        let uri = params.text_document.uri;
        let src = params.text_document.text;
        let base = base_dir_from_uri(&uri);
        let mut state = uri
            .to_file_path()
            .ok()
            .map(|path| Self::analyze_path(&src, &path))
            .unwrap_or_else(|| Self::analyze(&src, &base));
        // Update reverse import map.
        if let Some(program) = &state.ast {
            if let Ok(file_path) = uri.to_file_path() {
                if let Ok(canon) = file_path.canonicalize() {
                    let imports = extract_imported_paths(program, &base);
                    self.update_importers(&canon, &imports).await;
                }
            }
        }
        self.client
            .log_message(
                tower_lsp::lsp_types::MessageType::INFO,
                format!(
                    "[spar-ls] did_open: symbols={} import_aliases=[{}] errors={}",
                    state.symbols.is_some(),
                    state
                        .import_symbols
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(","),
                    state.errors.len()
                ),
            )
            .await;
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
            let src = change.text;
            let base = base_dir_from_uri(&uri);

            // Read previous last-good symbols before releasing lock.
            let (prev_good_sym, prev_good_imports) = {
                let docs = self.documents.lock().await;
                if let Some(prev) = docs.get(&uri) {
                    let sym = prev
                        .last_good_symbols
                        .clone()
                        .or_else(|| prev.symbols.clone());
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

            let mut state = uri
                .to_file_path()
                .ok()
                .map(|path| Self::analyze_path(&src, &path))
                .unwrap_or_else(|| Self::analyze(&src, &base));
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
        let uri = params.text_document.uri;
        let base = base_dir_from_uri(&uri);

        // Re-analyse the saved file from disk.
        let src = match uri
            .to_file_path()
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
        {
            Some(s) => s,
            None => return,
        };

        // Read previous last-good state so hover/completion survive a failed save.
        let (prev_good_sym, prev_good_imports) = {
            let docs = self.documents.lock().await;
            if let Some(prev) = docs.get(&uri) {
                let sym = prev
                    .last_good_symbols
                    .clone()
                    .or_else(|| prev.symbols.clone());
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

        let mut state = uri
            .to_file_path()
            .ok()
            .map(|path| Self::analyze_path(&src, &path))
            .unwrap_or_else(|| Self::analyze(&src, &base));
        if state.symbols.is_some() {
            state.last_good_symbols = state.symbols.clone();
            state.last_good_import_symbols = state.import_symbols.clone();
        } else {
            state.last_good_symbols = prev_good_sym;
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
                let path_opt = uri.to_file_path().ok().and_then(|p| p.canonicalize().ok());
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
            None => return Ok(None),
        };
        let symbols = match state.effective_symbols() {
            Some(s) => s,
            None => return Ok(None),
        };

        let word = word_at_position(&state.source, pos);

        if !word.is_empty() {
            if let Some(program) = &state.ast {
                let offset = lsp_pos_to_byte_offset(&state.source, pos);
                if let Some(value) = task_hover_at_offset(program, &state.source, offset, &word) {
                    return Ok(Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value,
                        }),
                        range: None,
                    }));
                }
            }
        }

        // Case 0: import alias hover
        if !word.is_empty() {
            let imp_sym = state.effective_import_symbols();
            if let Some(imported_sym) = imp_sym.get(word.as_str()) {
                let value = format_import_hover(&word, imported_sym);
                return Ok(Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
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
                                    kind: MarkupKind::Markdown,
                                    value,
                                }),
                                range: None,
                            }));
                        }
                        // hovering on exported var name: base::[namespace]
                        if let Some(spar::resolver::GlobalEntry::Var { ty, .. }) =
                            imported_sym.globals.get(&word)
                        {
                            let value = format!(
                                "```spar\nexport var {}: {}\n```",
                                word,
                                format_spar_type(ty)
                            );
                            return Ok(Some(Hover {
                                contents: HoverContents::Markup(MarkupContent {
                                    kind: MarkupKind::Markdown,
                                    value,
                                }),
                                range: None,
                            }));
                        }
                        // hovering on function name: base::[main]
                        if let Some(entry) = imported_sym.functions.get(&word) {
                            if !entry.is_private {
                                let params = entry
                                    .params
                                    .iter()
                                    .map(|(n, t)| format!("{}: {}", n, format_spar_type(t)))
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                let value = format!(
                                    "```spar\nfunction {}({}) -> {}\n```",
                                    word,
                                    params,
                                    format_spar_type(&entry.ret)
                                );
                                return Ok(Some(Hover {
                                    contents: HoverContents::Markup(MarkupContent {
                                        kind: MarkupKind::Markdown,
                                        value,
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
                                let ty_str =
                                    field.ty.as_ref().map(format_spar_type).unwrap_or_else(|| {
                                        resolve_field_type_display(imported_sym, &sec_path, &word)
                                    });
                                let value = format!("```spar\n{}: {}\n```", word, ty_str);
                                return Ok(Some(Hover {
                                    contents: HoverContents::Markup(MarkupContent {
                                        kind: MarkupKind::Markdown,
                                        value,
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
                    let prefix_owned: Vec<String> = prefix.to_vec();
                    if let Some(section) = symbols.sections.get(&prefix_owned) {
                        if let Some(field) = section.fields.get(&word) {
                            let ty_str =
                                field.ty.as_ref().map(format_spar_type).unwrap_or_else(|| {
                                    resolve_field_type_display(symbols, &prefix_owned, &word)
                                });
                            let value = format!(
                                "```spar\n(field) {}: {} in `[{}]`\n```",
                                word,
                                ty_str,
                                prefix_owned.join(".")
                            );
                            return Ok(Some(Hover {
                                contents: HoverContents::Markup(MarkupContent {
                                    kind: MarkupKind::Markdown,
                                    value,
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
                        kind: MarkupKind::Markdown,
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
                        kind: MarkupKind::Markdown,
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
                            kind: MarkupKind::Markdown,
                            value,
                        }),
                        range: None,
                    }));
                }
            }
        }

        // Case 4: section field — word matches a field name in the section the
        // cursor is textually inside. Deliberately does NOT fall back to an
        // ambiguous scan across every section sharing that field name here —
        // more precise mechanisms (function/type/enum names, then type-checker
        // expression inference) get a chance first; seeing the wrong section's
        // field type is worse than briefly falling through. The ambiguous
        // fallback still runs, as an actual last resort, right before `Ok(None)`.
        if !word.is_empty() {
            let offset = lsp_pos_to_byte_offset(&state.source, pos);
            // Find the innermost section whose span contains the cursor.
            let containing_path: Option<Vec<String>> = state.ast.as_ref().and_then(|prog| {
                use spar::ast::TopLevelItem;
                prog.items
                    .iter()
                    .filter_map(|item| {
                        if let TopLevelItem::Section(sd) = item {
                            if sd.span.start <= offset && offset <= sd.span.end {
                                Some(sd.path.clone())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                    .next_back() // innermost (last) enclosing section
            });

            let field_hover = containing_path.as_ref().and_then(|path| {
                symbols
                    .sections
                    .get(path)
                    .and_then(|sec| sec.fields.get(&word).map(|field| (path.clone(), field)))
            });

            if let Some((path, field)) = field_hover {
                let ty_str = field
                    .ty
                    .as_ref()
                    .map(format_spar_type)
                    .unwrap_or_else(|| resolve_field_type_display(symbols, &path, &word));
                let value = format!(
                    "```spar\n(field) {}: {} in `[{}]`\n```",
                    word,
                    ty_str,
                    path.join(".")
                );
                return Ok(Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
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
                        kind: MarkupKind::Markdown,
                        value,
                    }),
                    range: None,
                }));
            }
        }

        // Case 5b/5c/5d: `type [Name] { ... }`, `enum Name { ... }` (bare or
        // `Name::Variant`), and `functionGroup Name { ... }` (bare or
        // `Name::member`) declarations/references.
        if !word.is_empty() {
            if let Some(value) = hover_type_enum_group(symbols, &state.source, pos, &word) {
                return Ok(Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
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
                let desc = if has_else {
                    "then + else branches"
                } else {
                    "then only (no else)"
                };
                let value = format!("```spar\nif/else — {}\n```", desc);
                return Ok(Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
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
                    kind: MarkupKind::Markdown,
                    value,
                }),
                range: None,
            }));
        }

        // Case 8: any expression whose type the typechecker can infer.
        if let Some(ast) = &state.ast {
            let offset = lsp_pos_to_byte_offset(&state.source, pos);
            if let Some(expr) = find_expression_at_offset(ast, offset) {
                if let Some(ty) = TypeChecker::infer_expression(expr, symbols) {
                    let value = format!("```spar\n(expression): {}\n```", format_spar_type(&ty));
                    return Ok(Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value,
                        }),
                        range: None,
                    }));
                }
            }
        }

        // Case 9: index expression — find `[...]` at cursor and return element type
        if let Some(ast) = &state.ast {
            let offset = lsp_pos_to_byte_offset(&state.source, pos);
            if let Some(elem_ty) = find_index_elem_type_at_offset(ast, symbols, offset) {
                let value = format!("```spar\n: {}\n```", format_spar_type(&elem_ty));
                return Ok(Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value,
                    }),
                    range: None,
                }));
            }
        }

        // Case 10 (last resort): a field name matching some section, anywhere
        // in the program, when nothing more precise recognized the cursor.
        // Ambiguous when multiple sections share a field name — kept as the
        // lowest-priority fallback rather than deleted outright, since a
        // possibly-wrong section beats no hover at all.
        if !word.is_empty() {
            if let Some((path, field)) = symbols
                .sections
                .iter()
                .find_map(|(path, sec)| sec.fields.get(&word).map(|field| (path.clone(), field)))
            {
                let ty_str = field
                    .ty
                    .as_ref()
                    .map(format_spar_type)
                    .unwrap_or_else(|| resolve_field_type_display(symbols, &path, &word));
                let value = format!(
                    "```spar\n(field) {}: {} in `[{}]`\n```",
                    word,
                    ty_str,
                    path.join(".")
                );
                return Ok(Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
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
            None => return Ok(Some(CompletionResponse::Array(keyword_items()))),
        };

        if is_cursor_in_block_comment(&state.source, pos) {
            return Ok(None);
        }

        let offset = lsp_pos_to_byte_offset(&state.source, pos);
        if let Ok(path) = uri.to_file_path() {
            if let Some(items) = package_metadata_completion_items(&path, &state.source, offset) {
                return Ok(Some(CompletionResponse::Array(items)));
            }
        }

        let symbols = match state.effective_symbols() {
            Some(s) => s,
            None => return Ok(Some(CompletionResponse::Array(keyword_items()))),
        };

        if let Some(items) = member_completion_items(&state.source, offset, symbols) {
            return Ok(Some(CompletionResponse::Array(items)));
        }
        if let Some(items) =
            task_completion_items(state.ast.as_ref(), &state.source, symbols, offset)
        {
            return Ok(Some(CompletionResponse::Array(items)));
        }

        // Case 1: after `path::` — enumerate section fields, enum variants,
        // function-group members, or imported symbols
        if let Some(path) = path_before_cursor(&state.source, pos) {
            if let Some(section) = symbols.sections.get(&path) {
                return Ok(Some(CompletionResponse::Array(section_field_completions(
                    symbols, &path, section,
                ))));
            }

            if path.len() == 1 {
                if let Some(items) = enum_or_group_path_completions(symbols, &path[0]) {
                    return Ok(Some(CompletionResponse::Array(items)));
                }
            }

            if symbols.imports.contains_key(&path[0]) {
                if let Some(imported_sym) = state.effective_import_symbols().get(&path[0]) {
                    if path.len() == 1 {
                        // base:: → top-level exported symbols of imported file
                        return Ok(Some(CompletionResponse::Array(import_completion_items(
                            imported_sym,
                        ))));
                    } else {
                        // base::SectionName:: → fields of that section in imported file
                        let section_path = path[1..].to_vec();
                        if let Some(section) = imported_sym.sections.get(&section_path) {
                            return Ok(Some(CompletionResponse::Array(section_field_completions(
                                imported_sym,
                                &section_path,
                                section,
                            ))));
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
                label: name.clone(),
                kind: Some(CompletionItemKind::VARIABLE),
                detail,
                ..Default::default()
            });
        }

        for path in symbols.sections.keys() {
            if let Some(first) = path.first() {
                items.push(CompletionItem {
                    label: first.clone(),
                    kind: Some(CompletionItemKind::MODULE),
                    ..Default::default()
                });
            }
        }

        for alias in symbols.imports.keys() {
            items.push(CompletionItem {
                label: alias.clone(),
                kind: Some(CompletionItemKind::MODULE),
                ..Default::default()
            });
        }

        Ok(Some(CompletionResponse::Array(items)))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let docs = self.documents.lock().await;
        let Some(state) = docs.get(&uri) else {
            return Ok(None);
        };
        Ok(definition_at(&uri, state, pos).map(GotoDefinitionResponse::Scalar))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let include_declaration = params.context.include_declaration;
        let locations = self.references_at(&uri, pos, include_declaration).await;
        Ok(if locations.is_empty() {
            None
        } else {
            Some(locations)
        })
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri;
        let docs = self.documents.lock().await;
        let state = match docs.get(&uri) {
            Some(s) => s,
            None => return Ok(None),
        };
        let program = match &state.ast {
            Some(p) => p,
            None => return Ok(None),
        };

        let mut raw: Vec<RawToken> = Vec::new();
        collect_tokens_from_program(program, &state.source, &mut raw);
        raw.sort_by_key(|t| (t.line, t.start_char));

        let mut data: Vec<SemanticToken> = Vec::with_capacity(raw.len());
        let mut prev_line = 0u32;
        let mut prev_char = 0u32;
        for tok in &raw {
            let delta_line = tok.line - prev_line;
            let delta_char = if delta_line == 0 {
                tok.start_char - prev_char
            } else {
                tok.start_char
            };
            data.push(SemanticToken {
                delta_line,
                delta_start: delta_char,
                length: tok.length,
                token_type: tok.token_type,
                token_modifiers_bitset: tok.modifiers,
            });
            prev_line = tok.line;
            prev_char = tok.start_char;
        }

        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data,
        })))
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri;
        let docs = self.documents.lock().await;
        let state = match docs.get(&uri) {
            Some(s) => s,
            None => return Ok(None),
        };
        let program = match &state.ast {
            Some(p) => p,
            None => return Ok(None), // parse failed — fail soft, not as LSP error
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
            (
                (line_count.saturating_sub(1)) as u32,
                last_line.len() as u32,
            )
        };

        let full_range = Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: end_line,
                character: end_char,
            },
        };

        Ok(Some(vec![TextEdit {
            range: full_range,
            new_text: formatted,
        }]))
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| SparLanguageServer {
        client,
        documents: Mutex::new(HashMap::new()),
        importers: Mutex::new(HashMap::new()),
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
    fn edit_sequence_clears_diagnostics_and_restores_symbols() {
        let invalid = SparLanguageServer::analyze(
            "export var port: int = \"wrong\";",
            std::path::Path::new("."),
        );
        assert_eq!(invalid.diagnostics().len(), 1);

        let corrected =
            SparLanguageServer::analyze("export var port: int = 8080;", std::path::Path::new("."));
        assert!(corrected.diagnostics().is_empty());
        assert!(corrected.symbols.unwrap().globals.contains_key("port"));
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

    fn task_completion_labels(src: &str, marker: &str) -> Vec<String> {
        let offset = src.find(marker).expect("completion marker");
        let tokens = Lexer::new(src).tokenize().expect("lex");
        let program = Parser::new(tokens).parse().expect("parse");
        let symbols = Resolver::new().resolve(&program, &[]).expect("resolve");
        task_completion_items(Some(&program), src, &symbols, offset)
            .expect("task completion context")
            .into_iter()
            .map(|item| item.label)
            .collect()
    }

    #[test]
    fn completion_suggests_missing_task_metadata_fields() {
        let src = concat!(
            "task [Deploy] {\n",
            "    description: \"Deploy the app\";\n",
            "    quiet: true;\n",
            "    /* complete here */\n",
            "    run { echo deploy; };\n",
            "};\n",
        );
        let labels = task_completion_labels(src, "/* complete here */");
        assert!(labels.contains(&"dependsOn".to_string()));
        assert!(labels.contains(&"env".to_string()));
        assert!(!labels.contains(&"description".to_string()));
        assert!(!labels.contains(&"quiet".to_string()));
        assert!(!labels.contains(&"run".to_string()));
    }

    #[test]
    fn completion_still_offers_run_when_only_labeled_blocks_exist() {
        let src = concat!(
            "task [Deploy] {\n",
            "    description: \"Deploy the app\";\n",
            "    /* complete here */\n",
            "    run windows { echo win; };\n",
            "};\n",
        );
        let labels = task_completion_labels(src, "/* complete here */");
        assert!(
            labels.contains(&"run".to_string()),
            "a task with only a labeled run block and no bare default must still offer 'run'"
        );
    }

    #[test]
    fn completion_suggests_other_tasks_inside_depends_on() {
        let src = concat!(
            "task [Build] { run { cargo build; }; };\n",
            "task [Test] { run { cargo test; }; };\n",
            "task [Deploy] { dependsOn: [Build]; run { echo deploy; }; };\n",
        );
        let offset = src.find("Build]; run").expect("dependency") + "Build".len();
        let tokens = Lexer::new(src).tokenize().expect("lex");
        let program = Parser::new(tokens).parse().expect("parse");
        let symbols = Resolver::new().resolve(&program, &[]).expect("resolve");
        let labels = task_completion_items(Some(&program), src, &symbols, offset)
            .expect("dependsOn completion")
            .into_iter()
            .map(|item| item.label)
            .collect::<Vec<_>>();
        assert!(labels.contains(&"Build".to_string()));
        assert!(labels.contains(&"Test".to_string()));
        assert!(!labels.contains(&"Deploy".to_string()));
    }

    #[test]
    fn completion_only_suggests_task_params_in_run_interpolation() {
        let src = concat!(
            "var environment: str = \"global\";\n",
            "var region: str = \"west\";\n",
            "task [Deploy](environment: str) { run { echo ${environment}; }; };\n",
        );
        let offset = src.rfind("environment}").expect("interpolation") + 2;
        let tokens = Lexer::new(src).tokenize().expect("lex");
        let program = Parser::new(tokens).parse().expect("parse");
        let symbols = Resolver::new().resolve(&program, &[]).expect("resolve");
        let items = task_completion_items(Some(&program), src, &symbols, offset)
            .expect("interpolation completion");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "environment");
        assert!(!items.iter().any(|item| item.label == "region"));
    }

    #[test]
    fn completion_recovers_task_params_immediately_after_interpolation_start() {
        let valid = "task [Deploy](environment: str, *extra: str) { run { echo ok; }; };\n";
        let symbols = resolve_src(valid);
        let src = "task [Deploy](environment: str, *extra: str) { run { echo ${";
        let items = task_completion_items(None, src, &symbols, src.len())
            .expect("incomplete interpolation completion");
        assert_eq!(
            items
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>(),
            ["environment", "extra"]
        );
    }

    #[test]
    fn completion_after_dot_suggests_fields_of_named_type() {
        let symbols = resolve_src(concat!(
            "type [Human]{ name: str; age: int; };\n",
            "var person: Human = { name: \"Ada\"; age: 36; };\n",
        ));
        let items = member_completion_items("person.", "person.".len(), &symbols)
            .expect("member completion context");

        assert_eq!(
            items
                .iter()
                .map(|item| (item.label.as_str(), item.kind))
                .collect::<Vec<_>>(),
            [
                ("name", Some(CompletionItemKind::FIELD)),
                ("age", Some(CompletionItemKind::FIELD)),
            ]
        );
    }

    #[test]
    fn completion_after_dot_suggests_fields_of_a_section() {
        // Regression: a section reference (as opposed to a var typed with a
        // named type) fell through member_completion_items entirely — it
        // never checked symbols.sections, so `Environment.` (a very common
        // real-world pattern: `[Environment] -> SomeType { ... }` then
        // `Environment.field` elsewhere) silently returned an empty
        // completion list instead of the section's own fields.
        let symbols = resolve_src(concat!(
            "export type [HyprlandEnvironmentType]{ terminal: str; launcher: str; };\n",
            "[Environment] -> HyprlandEnvironmentType {\n",
            "    terminal: \"kitty\";\n",
            "    launcher: \"wofi\";\n",
            "};\n",
        ));
        let items = member_completion_items("Environment.", "Environment.".len(), &symbols)
            .expect("member completion context");

        let mut labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        labels.sort();
        assert_eq!(labels, ["launcher", "terminal"]);
    }

    #[test]
    fn completion_after_dot_suggests_variants_of_enum_typed_value() {
        let symbols = resolve_src(concat!(
            "enum Device { Ios, Android };\n",
            "var device: Device = Device::Ios;\n",
        ));
        let items = member_completion_items("device.", "device.".len(), &symbols)
            .expect("member completion context");

        assert_eq!(
            items
                .iter()
                .map(|item| (item.label.as_str(), item.kind))
                .collect::<Vec<_>>(),
            [
                ("Ios", Some(CompletionItemKind::ENUM_MEMBER)),
                ("Android", Some(CompletionItemKind::ENUM_MEMBER)),
            ]
        );
    }

    #[test]
    fn completion_after_dot_suggests_function_group_members() {
        let symbols = resolve_src(concat!(
            "functionGroup Convert {\n",
            "    function toText(value: int) -> str { return str(value); }\n",
            "    function toBool(value: str) -> bool { return bool(value); }\n",
            "};\n",
        ));
        let items = member_completion_items("Convert.", "Convert.".len(), &symbols)
            .expect("member completion context");

        assert_eq!(items.len(), 2);
        assert!(items.iter().any(|item| {
            item.label == "toText" && item.kind == Some(CompletionItemKind::FUNCTION)
        }));
        assert!(items.iter().any(|item| {
            item.label == "toBool" && item.kind == Some(CompletionItemKind::FUNCTION)
        }));
    }

    #[test]
    fn completion_after_double_colon_suggests_enum_variants() {
        let symbols = resolve_src(concat!(
            "export enum Devices {\n",
            "    Ios,\n",
            "    Android,\n",
            "};\n",
        ));
        let items = enum_or_group_path_completions(&symbols, "Devices")
            .expect("enum path completion context");

        assert_eq!(
            items
                .iter()
                .map(|item| (item.label.as_str(), item.kind))
                .collect::<Vec<_>>(),
            [
                ("Ios", Some(CompletionItemKind::ENUM_MEMBER)),
                ("Android", Some(CompletionItemKind::ENUM_MEMBER)),
            ]
        );
    }

    #[test]
    fn completion_after_double_colon_suggests_function_group_members() {
        let symbols = resolve_src(concat!(
            "functionGroup EdgeInsect {\n",
            "    function only() -> [int] { return [1]; }\n",
            "    function semantic(hor: float) -> [int] { return [1]; }\n",
            "};\n",
        ));
        let items = enum_or_group_path_completions(&symbols, "EdgeInsect")
            .expect("function group path completion context");

        assert_eq!(items.len(), 2);
        assert!(items.iter().any(|item| {
            item.label == "only" && item.kind == Some(CompletionItemKind::FUNCTION)
        }));
        assert!(items.iter().any(|item| {
            item.label == "semantic" && item.kind == Some(CompletionItemKind::FUNCTION)
        }));
    }

    #[test]
    fn completion_after_double_colon_on_unknown_name_is_none() {
        let symbols = resolve_src("export var port: int = 8080;\n");
        assert!(enum_or_group_path_completions(&symbols, "NotAThing").is_none());
    }

    fn task_hover(src: &str, needle: &str, occurrence: usize) -> String {
        let offset = src
            .match_indices(needle)
            .nth(occurrence)
            .map(|(offset, _)| offset + 1)
            .expect("hover target");
        let tokens = Lexer::new(src).tokenize().expect("lex");
        let program = Parser::new(tokens).parse().expect("parse");
        task_hover_at_offset(&program, src, offset, needle).expect("task hover")
    }

    #[test]
    fn hover_on_task_name_shows_signature_and_description() {
        let src = concat!(
            "task [Build] { run { cargo build; }; };\n",
            "task [Deploy](environment: str = \"staging\", *extra: str) {\n",
            "    description: \"Deploy to an environment\";\n",
            "    dependsOn: [Build];\n",
            "    run { ./deploy.sh ${environment} ${extra}; };\n",
            "};\n",
        );
        let hover = task_hover(src, "Deploy", 0);
        assert!(hover.contains("task [Deploy](environment: str = \"staging\", *extra: str)"));
        assert!(hover.contains("Deploy to an environment"));
        assert!(hover.contains("Depends on: `Build`"));
    }

    #[test]
    fn hover_on_task_param_declaration_shows_default_and_variadic_marker() {
        let src = concat!(
            "task [Deploy](environment: str = \"staging\", *extra: str) {\n",
            "    run { ./deploy.sh ${environment} ${extra}; };\n",
            "};\n",
        );
        let environment = task_hover(src, "environment", 0);
        assert!(environment.contains("environment: str = \"staging\""));
        let extra = task_hover(src, "extra", 0);
        assert!(extra.contains("*extra: str"));
    }

    #[test]
    fn hover_on_task_param_interpolation_matches_declaration_hover() {
        let src = concat!(
            "task [Deploy](environment: str = \"staging\") {\n",
            "    run { ./deploy.sh ${environment}; };\n",
            "};\n",
        );
        assert_eq!(
            task_hover(src, "environment", 0),
            task_hover(src, "environment", 1)
        );
    }

    #[test]
    fn hover_on_task_dependson_shows_referenced_task_signature() {
        let src = concat!(
            "task [Build] { description: \"Compile\"; run { cargo build; }; };\n",
            "task [Test] {\n",
            "    dependsOn: [Build];\n",
            "    run { cargo test; };\n",
            "};\n",
        );
        // `Build` inside `dependsOn: [Build];` — the second occurrence of "Build".
        let hover = task_hover(src, "Build", 1);
        assert!(hover.contains("task [Build]"), "{hover}");
        assert!(hover.contains("Compile"), "{hover}");
    }

    fn pos_at(src: &str, byte_offset: usize) -> Position {
        let (line, col) = byte_to_lsp_pos(src, byte_offset);
        Position::new(line, col)
    }

    fn word_pos(src: &str, needle: &str, occurrence: usize) -> Position {
        let offset = src
            .match_indices(needle)
            .nth(occurrence)
            .map(|(offset, _)| offset + 1)
            .expect("hover target");
        pos_at(src, offset)
    }

    #[test]
    fn hover_type_enum_group_shows_type_declaration() {
        let src = "type [Border]{ width: int; };\n";
        let symbols = resolve_src(src);
        let pos = word_pos(src, "Border", 0);
        let value = hover_type_enum_group(&symbols, src, pos, "Border").expect("type hover");
        assert!(value.contains("type [Border]"), "{value}");
        assert!(value.contains("width: int"), "{value}");
    }

    #[test]
    fn hover_type_enum_group_shows_enum_declaration() {
        let src = "enum Color { Red, Green, Blue };\n";
        let symbols = resolve_src(src);
        let pos = word_pos(src, "Color", 0);
        let value = hover_type_enum_group(&symbols, src, pos, "Color").expect("enum hover");
        assert!(value.contains("enum Color"), "{value}");
        assert!(value.contains("Red, Green, Blue"), "{value}");
    }

    #[test]
    fn hover_type_enum_group_shows_enum_variant() {
        let src = "enum Color { Red, Green, Blue };\nvar c: Color = Color::Red;\n";
        let symbols = resolve_src(src);
        let pos = word_pos(src, "Red", 1);
        let value = hover_type_enum_group(&symbols, src, pos, "Red").expect("variant hover");
        assert!(value.contains("Color::Red"), "{value}");
    }

    #[test]
    fn hover_type_enum_group_shows_function_group_declaration() {
        let src = "functionGroup Handlers { function onStart() -> int { return 1; } };\n";
        let symbols = resolve_src(src);
        let pos = word_pos(src, "Handlers", 0);
        let value = hover_type_enum_group(&symbols, src, pos, "Handlers").expect("group hover");
        assert!(value.contains("functionGroup Handlers"), "{value}");
        assert!(value.contains("onStart"), "{value}");
    }

    #[test]
    fn hover_type_enum_group_shows_function_group_member() {
        let src = concat!(
            "functionGroup Handlers { function onStart() -> int { return 1; } };\n",
            "var x: int = Handlers::onStart();\n",
        );
        let symbols = resolve_src(src);
        let pos = word_pos(src, "onStart", 1);
        let value = hover_type_enum_group(&symbols, src, pos, "onStart").expect("member hover");
        assert!(value.contains("function onStart() -> int"), "{value}");
    }

    #[test]
    fn hover_type_enum_group_returns_none_for_unrelated_word() {
        let src = "var x: int = 1;\n";
        let symbols = resolve_src(src);
        let pos = word_pos(src, "x", 0);
        assert!(hover_type_enum_group(&symbols, src, pos, "x").is_none());
    }

    #[test]
    fn definition_resolves_function_group_member() {
        let src = concat!(
            "functionGroup Handlers { function onStart() -> int { return 1; } };\n",
            "var x: int = Handlers::onStart();\n",
        );
        let dir = std::env::temp_dir();
        let uri = Url::from_file_path(dir.join("group_member.spar")).unwrap();
        let state = SparLanguageServer::analyze(src, &dir);
        let pos = word_pos(src, "onStart", 1);
        let location = definition_at(&uri, &state, pos).expect("definition");
        assert_eq!(location.range.start.line, 0);
    }

    #[test]
    fn definition_resolves_function_group_name() {
        let src = "functionGroup Handlers { function onStart() -> int { return 1; } };\n";
        let dir = std::env::temp_dir();
        let uri = Url::from_file_path(dir.join("group_name.spar")).unwrap();
        let state = SparLanguageServer::analyze(src, &dir);
        let pos = word_pos(src, "Handlers", 0);
        let location = definition_at(&uri, &state, pos).expect("definition");
        assert_eq!(location.range.start.line, 0);
    }

    #[test]
    fn definition_resolves_enum_variant_by_jumping_to_the_enum_declaration() {
        let src = "enum Color { Red, Green, Blue };\nvar c: Color = Color::Red;\n";
        let dir = std::env::temp_dir();
        let uri = Url::from_file_path(dir.join("enum_variant.spar")).unwrap();
        let state = SparLanguageServer::analyze(src, &dir);
        let pos = word_pos(src, "Red", 1);
        let location = definition_at(&uri, &state, pos).expect("definition");
        // No per-variant span exists in the AST — this lands on the enum
        // declaration itself (line 0), not a variant-specific location.
        assert_eq!(location.range.start.line, 0);
    }

    #[test]
    fn references_finds_function_group_member_same_file() {
        let src = concat!(
            "functionGroup Handlers {\n",
            "    function onStart() -> int { return 1; }\n",
            "};\n",
            "var a: int = Handlers::onStart();\n",
            "var b: int = Handlers::onStart();\n",
        );
        let dir = std::env::temp_dir();
        let uri = Url::from_file_path(dir.join("group_refs.spar")).unwrap();
        let state = SparLanguageServer::analyze(src, &dir);
        let pos = word_pos(src, "onStart", 1);
        let location = definition_at(&uri, &state, pos).expect("definition");

        let refs = compute_references("onStart", location, src.to_string(), &HashMap::new(), false);
        assert_eq!(refs.len(), 2, "both call sites: {refs:?}");
    }

    #[test]
    fn definition_resolves_selective_import_to_its_true_source_file_and_line() {
        let dir =
            std::env::temp_dir().join(format!("spar_ls_selective_def_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let base_path = dir.join("base.spar");
        std::fs::write(
            &base_path,
            "export var unrelated: int = 0;\nfunction make() -> int { return 1; };\n",
        )
        .unwrap();
        let main_src = "import { make } from \"base.spar\";\nvar value: int = make();\n";
        let main_path = dir.join("main.spar");
        std::fs::write(&main_path, main_src).unwrap();
        let main_uri = Url::from_file_path(&main_path).unwrap();

        let state = SparLanguageServer::analyze(main_src, &dir);
        let pos = word_pos(main_src, "make", 1);
        let location = definition_at(&main_uri, &state, pos).expect("definition");

        // Must land in base.spar (where `make` is really declared), on its
        // real declaration line (1) — not on the local `import { make }`
        // statement in main.spar (line 0).
        assert_eq!(location.uri, Url::from_file_path(&base_path).unwrap());
        assert_eq!(location.range.start.line, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn definition_resolves_as_part_of_import_to_its_true_source_file_and_line() {
        let dir = std::env::temp_dir().join(format!("spar_ls_aspartof_def_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let base_path = dir.join("base.spar");
        std::fs::write(
            &base_path,
            "export var unrelated: int = 0;\nfunction make() -> int { return 1; };\n",
        )
        .unwrap();
        let main_src = "import asPartOf \"base.spar\";\nvar value: int = make();\n";
        let main_path = dir.join("main.spar");
        std::fs::write(&main_path, main_src).unwrap();
        let main_uri = Url::from_file_path(&main_path).unwrap();

        let state = SparLanguageServer::analyze(main_src, &dir);
        let pos = word_pos(main_src, "make", 0);
        let location = definition_at(&main_uri, &state, pos).expect("definition");

        assert_eq!(location.uri, Url::from_file_path(&base_path).unwrap());
        assert_eq!(location.range.start.line, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn references_does_not_search_a_file_for_a_name_its_selective_import_never_requested() {
        let dir =
            std::env::temp_dir().join(format!("spar_ls_selective_refs_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let base_path = dir.join("base.spar");
        std::fs::write(
            &base_path,
            "function make() -> int { return 1; };\nfunction helper() -> int { return 2; };\n",
        )
        .unwrap();

        // `other.spar` imports `make` specifically — should be searched.
        let other_src = "import { make } from \"base.spar\";\nvar a: int = make();\n";
        let other_path = dir.join("other.spar");
        std::fs::write(&other_path, other_src).unwrap();

        // `unrelated.spar` imports `helper` (not `make`) from the same
        // base.spar, and separately declares its own unrelated local
        // `make` — its Selective import brought `helper` into scope, not
        // `make`, so its local `make` must NOT be reported as a reference
        // to base.spar's `make`.
        let unrelated_src = concat!(
            "import { helper } from \"base.spar\";\n",
            "function make() -> int { return 99; };\n",
            "var b: int = make();\n",
        );
        let unrelated_path = dir.join("unrelated.spar");
        std::fs::write(&unrelated_path, unrelated_src).unwrap();

        let state = SparLanguageServer::analyze(other_src, &dir);
        let other_uri = Url::from_file_path(&other_path).unwrap();
        let pos = word_pos(other_src, "make", 1);
        let location = definition_at(&other_uri, &state, pos).expect("definition");
        assert_eq!(location.uri, Url::from_file_path(&base_path).unwrap());

        let mut importers = HashMap::new();
        importers.insert(
            base_path.canonicalize().unwrap(),
            HashSet::from([
                other_path.canonicalize().unwrap(),
                unrelated_path.canonicalize().unwrap(),
            ]),
        );

        let defining_source = std::fs::read_to_string(&base_path).unwrap();
        let refs = compute_references("make", location, defining_source, &importers, false);
        assert_eq!(
            refs.len(),
            1,
            "only other.spar's real use, not unrelated.spar's own local make: {refs:?}"
        );
        assert_eq!(refs[0].uri, other_uri);

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn span(line: u32, col: u32, start: usize, end: usize) -> Span {
        Span {
            line,
            col,
            start,
            end,
        }
    }

    #[test]
    fn lex_error_produces_correct_range_and_message() {
        let err = SparError::LexError {
            message: "unexpected character '@'".to_string(),
            span: span(3, 5, 42, 43),
        };
        let diag = spar_error_to_diagnostic(&err);
        assert_eq!(diag.range.start.line, 2, "line must be span.line - 1");
        assert_eq!(diag.range.start.character, 4, "char must be span.col - 1");
        assert_eq!(diag.message, "unexpected character '@'");
        assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diag.source.as_deref(), Some("spar"));
    }

    #[test]
    fn first_line_first_column_maps_to_position_zero() {
        let err = SparError::ParseError {
            message: "unexpected EOF".to_string(),
            span: span(1, 1, 0, 1),
        };
        let diag = spar_error_to_diagnostic(&err);
        assert_eq!(diag.range.start.line, 0);
        assert_eq!(diag.range.start.character, 0);
    }

    #[test]
    fn resolve_error_with_hint_appends_hint_to_message() {
        let err = SparError::ResolveError {
            message: "undefined variable 'por'".to_string(),
            hint: Some("did you mean 'port'?".to_string()),
            span: span(2, 5, 20, 23),
        };
        let diag = spar_error_to_diagnostic(&err);
        assert!(diag.message.contains("undefined variable 'por'"));
        assert!(diag.message.contains("did you mean 'port'?"));
    }

    #[test]
    fn resolve_error_without_hint_has_no_hint_text() {
        let err = SparError::ResolveError {
            message: "duplicate name 'port'".to_string(),
            hint: None,
            span: span(1, 1, 0, 4),
        };
        let diag = spar_error_to_diagnostic(&err);
        assert!(!diag.message.contains("Hint:"));
    }

    #[test]
    fn type_error_with_hint_maps_range_correctly() {
        let err = SparError::TypeError {
            message: "expected int, got str".to_string(),
            hint: Some("consider using str()".to_string()),
            span: span(5, 10, 60, 65),
        };
        let diag = spar_error_to_diagnostic(&err);
        assert_eq!(diag.range.start.line, 4);
        assert_eq!(diag.range.start.character, 9);
        assert!(diag.message.contains("consider using str()"));
    }

    #[test]
    fn eval_error_maps_as_warning() {
        let err = SparError::EvalError {
            message: "division by zero".to_string(),
            span: span(10, 20, 200, 201),
        };
        let diag = spar_error_to_diagnostic(&err);
        assert_eq!(diag.range.start.line, 9);
        assert_eq!(diag.range.start.character, 19);
        assert_eq!(diag.severity, Some(DiagnosticSeverity::WARNING));
    }

    #[test]
    fn word_at_middle_of_identifier() {
        let src = "var port: int = 8080;";
        assert_eq!(
            word_at_position(
                src,
                Position {
                    line: 0,
                    character: 5
                }
            ),
            "port"
        );
    }

    #[test]
    fn word_at_start_of_identifier() {
        let src = "var port: int = 8080;";
        assert_eq!(
            word_at_position(
                src,
                Position {
                    line: 0,
                    character: 4
                }
            ),
            "port"
        );
    }

    #[test]
    fn word_at_operator_returns_empty() {
        let src = "var port: int = 8080;";
        assert_eq!(
            word_at_position(
                src,
                Position {
                    line: 0,
                    character: 8
                }
            ),
            ""
        );
    }

    #[test]
    fn word_on_nonexistent_line_returns_empty() {
        let src = "var port: int = 8080;";
        assert_eq!(
            word_at_position(
                src,
                Position {
                    line: 99,
                    character: 0
                }
            ),
            ""
        );
    }

    #[test]
    fn word_at_character_beyond_line_end_returns_empty() {
        let src = "var x;";
        assert_eq!(
            word_at_position(
                src,
                Position {
                    line: 0,
                    character: 100
                }
            ),
            ""
        );
    }

    // ── path_before_cursor ────────────────────────────────────────────────────

    #[test]
    fn path_detected_when_cursor_follows_double_colon() {
        let src = "var x = database::";
        assert_eq!(
            path_before_cursor(
                src,
                Position {
                    line: 0,
                    character: 18
                }
            ),
            Some(vec!["database".to_string()])
        );
    }

    #[test]
    fn path_multi_segment_detected() {
        let src = "var x = foo::bar::";
        assert_eq!(
            path_before_cursor(
                src,
                Position {
                    line: 0,
                    character: 18
                }
            ),
            Some(vec!["foo".to_string(), "bar".to_string()])
        );
    }

    #[test]
    fn path_none_when_no_double_colon_present() {
        let src = "var x = something";
        assert_eq!(
            path_before_cursor(
                src,
                Position {
                    line: 0,
                    character: 17
                }
            ),
            None
        );
    }

    #[test]
    fn path_none_when_single_colon_only() {
        let src = "var x:";
        assert_eq!(
            path_before_cursor(
                src,
                Position {
                    line: 0,
                    character: 6
                }
            ),
            None
        );
    }

    #[test]
    fn path_on_nonexistent_line_returns_none() {
        let src = "var x = database::";
        assert_eq!(
            path_before_cursor(
                src,
                Position {
                    line: 5,
                    character: 0
                }
            ),
            None
        );
    }

    // ── is_in_type_position ───────────────────────────────────────────────────

    #[test]
    fn type_position_detected_after_colon() {
        let src = "var x:";
        assert!(is_in_type_position(
            src,
            Position {
                line: 0,
                character: 6
            }
        ));
    }

    #[test]
    fn type_position_detected_after_colon_with_space() {
        let src = "var x: ";
        assert!(is_in_type_position(
            src,
            Position {
                line: 0,
                character: 7
            }
        ));
    }

    #[test]
    fn type_position_false_after_double_colon() {
        let src = "var x = Database::";
        assert!(!is_in_type_position(
            src,
            Position {
                line: 0,
                character: 18
            }
        ));
    }

    #[test]
    fn type_position_false_with_no_colon() {
        let src = "var x";
        assert!(!is_in_type_position(
            src,
            Position {
                line: 0,
                character: 5
            }
        ));
    }

    #[test]
    fn type_position_false_on_nonexistent_line() {
        let src = "var x:";
        assert!(!is_in_type_position(
            src,
            Position {
                line: 99,
                character: 0
            }
        ));
    }

    // ── lsp_pos_to_byte_offset ────────────────────────────────────────────────

    #[test]
    fn byte_offset_first_line() {
        let src = "hello\nworld";
        assert_eq!(
            lsp_pos_to_byte_offset(
                src,
                Position {
                    line: 0,
                    character: 3
                }
            ),
            3
        );
    }

    #[test]
    fn byte_offset_second_line() {
        let src = "hello\nworld";
        // "hello\n" = 6 bytes, so line 1 starts at 6
        assert_eq!(
            lsp_pos_to_byte_offset(
                src,
                Position {
                    line: 1,
                    character: 2
                }
            ),
            8
        );
    }

    // ── format_spar_type ────────────────────────────────────────────────────────

    #[test]
    fn format_type_handles_all_kltype_variants() {
        assert_eq!(format_spar_type(&SparType::Int), "int");
        assert_eq!(format_spar_type(&SparType::Float), "float");
        assert_eq!(format_spar_type(&SparType::Str), "str");
        assert_eq!(format_spar_type(&SparType::Bool), "bool");
        assert_eq!(format_spar_type(&SparType::Section), "section");
        assert_eq!(format_spar_type(&SparType::Shell), "shell");
        assert_eq!(
            format_spar_type(&SparType::List(Box::new(SparType::Int))),
            "List<int>"
        );
        assert_eq!(
            format_spar_type(&SparType::List(Box::new(SparType::List(Box::new(
                SparType::Str
            ))))),
            "List<List<str>>"
        );
    }

    // ── spar_error_to_diagnostic hint inclusion ────────────────────────────────

    #[test]
    fn klerror_to_diagnostic_includes_hint_when_present() {
        let err = SparError::TypeError {
            message: "type mismatch".into(),
            hint: Some("try converting explicitly".into()),
            span: span(1, 1, 0, 1),
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
        for kw in &[
            "if", "else", "for", "in", "return", "function", "var", "export", "private", "struct",
            "type", "try", "catch",
        ] {
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
            assert!(
                item.detail.is_some(),
                "builtin {} missing detail",
                item.label
            );
        }
    }

    // ── find_if_at_offset ─────────────────────────────────────────────────────

    #[test]
    fn find_if_detects_then_else_branch_presence() {
        let src = "function f(x: bool) -> int {\n    if x {\n        return 1;\n    } else {\n        return 0;\n    }\n};";
        let tokens = spar::lexer::Lexer::new(src).tokenize().unwrap();
        let program = spar::parser::Parser::new(tokens).parse().unwrap();
        // "if" keyword is at byte offset 29 (line 1, col 4)
        let if_offset = src.find("if x").unwrap();
        let result = find_if_at_offset(&program, if_offset);
        assert_eq!(result, Some(true), "should detect else branch");
    }

    #[test]
    fn find_if_detects_then_only_no_else() {
        let src =
            "function f(x: bool) -> int {\n    if x {\n        return 1;\n    }\n    return 0;\n};";
        let tokens = spar::lexer::Lexer::new(src).tokenize().unwrap();
        let program = spar::parser::Parser::new(tokens).parse().unwrap();
        let if_offset = src.find("if x").unwrap();
        let result = find_if_at_offset(&program, if_offset);
        assert_eq!(result, Some(false), "should detect no else branch");
    }

    // ── four-segment path produces zero diagnostics ───────────────────────────

    #[test]
    fn four_segment_namespace_path_produces_no_diagnostics() {
        let src = "[A]{\n    b: section = {\n        c: section = {\n            d: section = {\n                e: int = 1;\n            };\n        };\n    };\n};\nvar x: int = A.b.c.d.e;";
        let state = SparLanguageServer::analyze(src, std::path::Path::new("."));
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
        let src = "this is not valid spar {{{";
        // Simulate what formatting() does internally: try parse, expect None on failure.
        let tokens_result = spar::lexer::Lexer::new(src).tokenize();
        let parse_ok = tokens_result
            .ok()
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
        assert_eq!(
            end_line, 1,
            "end line must be 1 (past content line) to cover trailing \\n"
        );
        assert_eq!(end_char, 0);
    }

    #[test]
    fn formatting_produces_expected_text_edit() {
        let messy = "var   x:int=1;";
        let tokens = spar::lexer::Lexer::new(messy).tokenize().unwrap();
        let program = spar::parser::Parser::new(tokens).parse().unwrap();
        let formatted =
            spar::formatter::format_program(&program, &spar::formatter::FormatConfig::default());
        // format_program always appends a trailing '\n'
        assert_eq!(
            formatted, "var x: int = 1;\n",
            "format_program output must end with exactly one newline"
        );
    }

    // ── semantic token helpers ────────────────────────────────────────────────

    struct DecodedToken {
        line: u32,
        start_char: u32,
        length: u32,
        token_type: u32,
    }

    fn decode_semantic_tokens(src: &str) -> Vec<DecodedToken> {
        let tokens = spar::lexer::Lexer::new(src).tokenize().unwrap();
        let program = spar::parser::Parser::new(tokens).parse().unwrap();
        let mut raw: Vec<RawToken> = Vec::new();
        collect_tokens_from_program(&program, src, &mut raw);
        raw.sort_by_key(|t| (t.line, t.start_char));
        // RawToken has absolute positions — no delta decoding needed here.
        raw.iter()
            .map(|t| DecodedToken {
                line: t.line,
                start_char: t.start_char,
                length: t.length,
                token_type: t.token_type,
            })
            .collect()
    }

    fn find_tok<'a>(tokens: &'a [DecodedToken], text: &str, src: &str) -> Option<&'a DecodedToken> {
        let lines: Vec<&str> = src.lines().collect();
        tokens.iter().find(|t| {
            let ln = t.line as usize;
            if ln >= lines.len() {
                return false;
            }
            let line = lines[ln];
            let start = t.start_char as usize;
            let end = start + t.length as usize;
            if start > line.len() || end > line.len() {
                return false;
            }
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
        let src = "function pickPort(debug: bool) -> int { return 9000; };";
        let tokens = decode_semantic_tokens(src);
        let tok = find_tok(&tokens, "pickPort", src).expect("pickPort not found");
        assert_eq!(tok.token_type, TT_FUNCTION);
    }

    #[test]
    fn semantic_tokens_task_name_uses_registered_task_type() {
        let src = "task [Deploy] { run { echo deploy; }; };";
        let tokens = decode_semantic_tokens(src);
        let tok = find_tok(&tokens, "Deploy", src).expect("Deploy not found");
        assert_eq!(tok.token_type, TT_TASK);
        assert_eq!(
            TOKEN_TYPES[TT_TASK as usize],
            SemanticTokenType::new("task")
        );
        assert_eq!(TOKEN_TYPES[TT_KEYWORD as usize], SemanticTokenType::KEYWORD);
    }

    #[test]
    fn semantic_tokens_task_fields_and_interpolations_are_classified() {
        let src = concat!(
            "task [Deploy](environment: str) {\n",
            "    description: \"Deploy\";\n",
            "    dependsOn: [];\n",
            "    env: { TARGET: \"prod\"; };\n",
            "    run { echo ${environment}; };\n",
            "};\n",
        );
        let tokens = decode_semantic_tokens(src);
        assert_eq!(
            find_tok(&tokens, "description", src).unwrap().token_type,
            TT_TASK_FIELD
        );
        assert_eq!(
            find_tok(&tokens, "dependsOn", src).unwrap().token_type,
            TT_TASK_FIELD
        );
        assert_eq!(
            find_tok(&tokens, "env", src).unwrap().token_type,
            TT_TASK_FIELD
        );
        assert_eq!(
            find_tok(&tokens, "TARGET", src).unwrap().token_type,
            TT_PROPERTY
        );
        assert!(tokens.iter().any(|token| {
            token.token_type == TT_PARAMETER
                && src.lines().nth(token.line as usize).is_some_and(|line| {
                    let start = token.start_char as usize;
                    let end = start + token.length as usize;
                    line.get(start..end) == Some("environment")
                })
        }));
        assert_eq!(
            find_tok(&tokens, "run", src).unwrap().token_type,
            TT_TASK_FIELD
        );
        assert!(find_tok(&tokens, "echo", src).is_none());
    }

    #[test]
    fn semantic_tokens_function_call_site_classified_as_function() {
        let src = "function f(x: int) -> int { return x; };\nvar y: int = f(x: 1);";
        let tokens = decode_semantic_tokens(src);
        let fn_toks: Vec<_> = tokens
            .iter()
            .filter(|t| t.token_type == TT_FUNCTION)
            .collect();
        assert!(
            fn_toks.len() >= 2,
            "expected decl + call site, got {}",
            fn_toks.len()
        );
    }

    #[test]
    fn semantic_tokens_section_name_classified_as_section() {
        let src = "[Server]{ host: str = \"x\"; };";
        let tokens = decode_semantic_tokens(src);
        let tok = find_tok(&tokens, "Server", src).expect("Server not found");
        assert_eq!(tok.token_type, TT_SECTION);
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
        let src = "function f(myParam: int) -> int { return myParam; };";
        let tokens = decode_semantic_tokens(src);
        let tok = find_tok(&tokens, "myParam", src).expect("myParam not found");
        assert_eq!(tok.token_type, TT_PARAMETER);
    }

    #[test]
    fn semantic_tokens_variable_reference_in_body_classified_as_variable() {
        let src = "var x: int = 1;\nvar y: int = x;";
        let tokens = decode_semantic_tokens(src);
        let var_toks: Vec<_> = tokens
            .iter()
            .filter(|t| t.token_type == TT_VARIABLE)
            .collect();
        assert!(
            var_toks.len() >= 2,
            "expected x decl + x ref, got {}",
            var_toks.len()
        );
    }

    #[test]
    fn semantic_tokens_type_decl_name_classified_as_type() {
        let src = "type [PostgresType]{ image: str; };";
        let tokens = decode_semantic_tokens(src);
        let tok = find_tok(&tokens, "PostgresType", src).expect("PostgresType not found");
        assert_eq!(tok.token_type, TT_TYPE);
    }

    #[test]
    fn semantic_tokens_named_type_field_reference_classified_as_type() {
        let src = "type [Border]{ width: int; };\ntype [Decoration]{ border: Border; };";
        let tokens = decode_semantic_tokens(src);
        let lines: Vec<&str> = src.lines().collect();
        let border_type_toks = tokens
            .iter()
            .filter(|t| {
                let ln = t.line as usize;
                if ln >= lines.len() {
                    return false;
                }
                let line = lines[ln];
                let start = t.start_char as usize;
                let end = start + t.length as usize;
                end <= line.len() && &line[start..end] == "Border" && t.token_type == TT_TYPE
            })
            .count();
        // One for the `[Border]` declaration, one for the `border: Border;` reference.
        assert_eq!(
            border_type_toks, 2,
            "expected both the Border declaration and its reference to be TT_TYPE"
        );
    }

    #[test]
    fn semantic_tokens_section_type_binding_classified_as_type() {
        let src = "type [Human]{ name: str; };\n[Man] -> Human {\n    name: \"John\";\n};";
        let tokens = decode_semantic_tokens(src);
        let tok = find_tok(&tokens, "Human", src).filter(|t| t.token_type == TT_TYPE);
        assert!(
            tok.is_some(),
            "expected the `-> Human` binding to be classified as a type"
        );
    }

    #[test]
    fn semantic_tokens_named_var_and_section_field_types_classified_as_type() {
        let src = concat!(
            "type [HyprlandEnvironmentType]{ name: str; };\n",
            "var environment: HyprlandEnvironmentType;\n",
            "[Config]{ environment: HyprlandEnvironmentType = {}; };\n",
        );
        let tokens = decode_semantic_tokens(src);
        let lines: Vec<&str> = src.lines().collect();
        let references = tokens
            .iter()
            .filter(|token| {
                let line = lines[token.line as usize];
                let start = token.start_char as usize;
                let end = start + token.length as usize;
                line.get(start..end) == Some("HyprlandEnvironmentType")
                    && token.token_type == TT_TYPE
            })
            .count();

        assert_eq!(
            references, 3,
            "expected declaration plus both type references"
        );
    }

    #[test]
    fn semantic_tokens_enum_and_function_group_value_qualifiers_keep_their_kinds() {
        let src = concat!(
            "enum LbAlgorithm { LeastConn };\n",
            "type [Balancer]{ algorithm: LbAlgorithm; };\n",
            "functionGroup EdgeInsect {\n",
            "    function only() -> [int] { return [1]; }\n",
            "};\n",
            "var algorithm: LbAlgorithm = LbAlgorithm::LeastConn;\n",
            "var padding: [int] = EdgeInsect::only();\n",
        );
        let tokens = decode_semantic_tokens(src);
        let lines: Vec<&str> = src.lines().collect();
        let count = |name: &str, token_type: u32| {
            tokens
                .iter()
                .filter(|token| {
                    let line = lines[token.line as usize];
                    let start = token.start_char as usize;
                    let end = start + token.length as usize;
                    line.get(start..end) == Some(name) && token.token_type == token_type
                })
                .count()
        };

        assert_eq!(count("LbAlgorithm", TT_ENUM), 4);
        assert_eq!(count("EdgeInsect", TT_FUNCTION_GROUP), 2);
        assert_ne!(TT_ENUM, TT_TYPE);
        assert_ne!(TT_FUNCTION_GROUP, TT_ENUM);
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
        let dir =
            std::env::temp_dir().join(format!("spar_ls_import_tok_test_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("shared.spar"),
            "export type [PostgresType]{ image: str; };\nexport [Colors]{ red: str = \"#f00\"; };\n",
        )
        .unwrap();
        let src = "import { PostgresType, Colors } from \"shared.spar\";\n";
        let tokens = spar::lexer::Lexer::new(src).tokenize().unwrap();
        let mut program = spar::parser::Parser::new(tokens).parse().unwrap();
        let mut loader = spar::loader::ImportLoader::new(&dir);
        spar::loader::expand_imports(&mut program, &mut loader).expect("expand must succeed");

        let mut raw: Vec<RawToken> = Vec::new();
        collect_tokens_from_program(&program, src, &mut raw);
        raw.sort_by_key(|t| (t.line, t.start_char));

        assert!(
            raw.iter().any(|t| t.token_type == TT_TYPE),
            "expected the imported PostgresType to get a TT_TYPE token"
        );
        assert!(
            raw.iter().any(|t| t.token_type == TT_SECTION),
            "expected the imported Colors section to get a TT_SECTION token"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn semantic_tokens_selective_import_type_enum_and_function_group_are_distinct() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!(
            "spar_ls_import_kind_tok_test_{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("other.spar"),
            concat!(
                "export type [ImportedType]{ value: str; };\n",
                "export enum ImportedEnum { First };\n",
                "functionGroup ImportedGroup {\n",
                "    function make() -> int { return 1; }\n",
                "};\n",
            ),
        )
        .unwrap();
        let src = concat!(
            "import { ImportedType, ImportedEnum, ImportedGroup } ",
            "from \"other.spar\";\n",
        );
        let state = SparLanguageServer::analyze(src, &dir);
        assert!(
            state.errors.is_empty(),
            "analysis errors: {:?}",
            state.errors
        );
        let program = state.ast.as_ref().expect("expanded program");
        let mut raw = Vec::new();
        collect_tokens_from_program(program, src, &mut raw);
        raw.sort_by_key(|token| (token.line, token.start_char));
        let decoded = raw
            .iter()
            .map(|token| DecodedToken {
                line: token.line,
                start_char: token.start_char,
                length: token.length,
                token_type: token.token_type,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            find_tok(&decoded, "ImportedType", src).unwrap().token_type,
            TT_TYPE
        );
        assert_eq!(
            find_tok(&decoded, "ImportedEnum", src).unwrap().token_type,
            TT_ENUM
        );
        assert_eq!(
            find_tok(&decoded, "ImportedGroup", src).unwrap().token_type,
            TT_FUNCTION_GROUP
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn semantic_tokens_keywords_and_builtin_types_are_distinct() {
        let src = concat!(
            "export struct App { port: int = 8080; };\n",
            "function main() -> void { try {} catch err {} };",
        );
        let tokens = decode_semantic_tokens(src);
        assert_eq!(
            find_tok(&tokens, "export", src).unwrap().token_type,
            TT_KEYWORD
        );
        assert_eq!(
            find_tok(&tokens, "struct", src).unwrap().token_type,
            TT_KEYWORD
        );
        assert_eq!(
            find_tok(&tokens, "try", src).unwrap().token_type,
            TT_KEYWORD
        );
        assert_eq!(
            find_tok(&tokens, "catch", src).unwrap().token_type,
            TT_KEYWORD
        );
        assert_eq!(find_tok(&tokens, "int", src).unwrap().token_type, TT_TYPE);
    }

    #[test]
    fn semantic_tokens_and_formatter_support_async_await() {
        let src = "async function value() -> int { return 1; }; async function main() -> int { return await value(); };";
        let tokens = decode_semantic_tokens(src);
        assert_eq!(
            find_tok(&tokens, "async", src).unwrap().token_type,
            TT_KEYWORD
        );
        assert_eq!(
            find_tok(&tokens, "await", src).unwrap().token_type,
            TT_KEYWORD
        );

        let parsed = spar::parser::Parser::new(spar::lexer::Lexer::new(src).tokenize().unwrap())
            .parse()
            .unwrap();
        let formatted =
            spar::formatter::format_program(&parsed, &spar::formatter::FormatConfig::default());
        assert!(formatted.contains("async function main() -> int"));
        assert!(formatted.contains("return await value();"));
    }

    #[test]
    fn diagnostics_reuse_core_await_context_rule() {
        let source = "async function value() -> int { return 1; }; function main() -> int { return await value(); };";
        let tokens = spar::lexer::Lexer::new(source).tokenize().unwrap();
        let program = spar::parser::Parser::new(tokens).parse().unwrap();
        let state = analyze_single_file(source, program, Vec::new());
        assert!(state.diagnostics().iter().any(|diagnostic| diagnostic
            .message
            .contains("only valid inside an async function")));
    }

    #[test]
    fn semantic_tokens_classify_shell_syntax_and_type() {
        let src = concat!(
            "function plan() -> shell { return command echo hi; };\n",
            "function run() -> int { var r: ExecResult = exec shell { true; }; return r.exitCode; };\n",
        );
        let tokens = decode_semantic_tokens(src);
        assert_eq!(find_tok(&tokens, "shell", src).unwrap().token_type, TT_TYPE);
        assert_eq!(
            find_tok(&tokens, "command", src).unwrap().token_type,
            TT_KEYWORD
        );
        assert_eq!(
            find_tok(&tokens, "exec", src).unwrap().token_type,
            TT_KEYWORD
        );
        let block_shell = tokens
            .iter()
            .find(|token| {
                token.line == 1 && {
                    let line = src.lines().nth(1).unwrap();
                    let start = token.start_char as usize;
                    let end = start + token.length as usize;
                    line.get(start..end) == Some("shell")
                }
            })
            .expect("shell block keyword not found");
        assert_eq!(block_shell.token_type, TT_KEYWORD);
    }

    #[test]
    fn definition_resolves_local_global_reference() {
        let src = "var answer: int = 42;\nvar copy: int = answer;\n";
        let dir = std::env::temp_dir();
        let uri = Url::from_file_path(dir.join("definition_local.spar")).unwrap();
        let state = SparLanguageServer::analyze(src, &dir);
        let location = definition_at(&uri, &state, Position::new(1, 18)).expect("definition");
        assert_eq!(location.uri, uri);
        assert_eq!(location.range.start.line, 0);
    }

    #[test]
    fn definition_resolves_imported_function_cross_file() {
        let dir = std::env::temp_dir().join(format!("spar_ls_definition_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let imported_path = dir.join("base.spar");
        std::fs::write(&imported_path, "function make() -> int { return 1; };\n").unwrap();
        let src = "import \"base.spar\";\nvar value: int = base::make();\n";
        let uri = Url::from_file_path(dir.join("main.spar")).unwrap();
        let state = SparLanguageServer::analyze(src, &dir);
        let location = definition_at(&uri, &state, Position::new(1, 24)).expect("definition");
        assert_eq!(location.uri, Url::from_file_path(&imported_path).unwrap());
        assert_eq!(location.range.start.line, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn references_finds_same_file_uses_and_optionally_the_declaration() {
        let src = "var answer: int = 42;\nvar copy: int = answer;\nvar other: int = answer;\n";
        let dir = std::env::temp_dir();
        let uri = Url::from_file_path(dir.join("references_local.spar")).unwrap();
        let state = SparLanguageServer::analyze(src, &dir);
        let location = definition_at(&uri, &state, Position::new(1, 18)).expect("definition");

        let without_decl = compute_references(
            "answer",
            location.clone(),
            src.to_string(),
            &HashMap::new(),
            false,
        );
        assert_eq!(without_decl.len(), 2, "the two uses, not the declaration");
        assert!(without_decl.iter().all(|l| l.range.start.line != 0));

        let with_decl =
            compute_references("answer", location, src.to_string(), &HashMap::new(), true);
        assert_eq!(with_decl.len(), 3, "two uses plus the declaration");
        assert!(with_decl.iter().any(|l| l.range.start.line == 0));
    }

    #[test]
    fn references_finds_cross_file_uses_through_an_aliased_import() {
        let dir = std::env::temp_dir().join(format!("spar_ls_references_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let base_path = dir.join("base.spar");
        std::fs::write(&base_path, "function make() -> int { return 1; };\n").unwrap();
        let main_src = "import \"base.spar\";\nvar value: int = base::make();\n";
        let main_path = dir.join("main.spar");
        std::fs::write(&main_path, main_src).unwrap();
        let main_uri = Url::from_file_path(&main_path).unwrap();

        let state = SparLanguageServer::analyze(main_src, &dir);
        let location = definition_at(&main_uri, &state, Position::new(1, 24)).expect("definition");
        assert_eq!(location.uri, Url::from_file_path(&base_path).unwrap());

        let mut importers = HashMap::new();
        importers.insert(
            base_path.canonicalize().unwrap(),
            HashSet::from([main_path.canonicalize().unwrap()]),
        );

        let refs = compute_references(
            "make",
            location,
            "function make() -> int { return 1; };\n".to_string(),
            &importers,
            false,
        );
        assert_eq!(refs.len(), 1, "the one cross-file use in main.spar");
        assert_eq!(refs[0].uri, main_uri);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn references_finds_task_dependson_reference() {
        let src = concat!(
            "task [Build] {\n",
            "    run { echo build; };\n",
            "};\n",
            "task [Test] {\n",
            "    dependsOn: [Build];\n",
            "    run { echo test; };\n",
            "};\n",
        );
        let dir = std::env::temp_dir();
        let uri = Url::from_file_path(dir.join("references_task.spar")).unwrap();
        let state = SparLanguageServer::analyze(src, &dir);
        // Position on `Build` in `task [Build] {`.
        let location = definition_at(&uri, &state, Position::new(0, 7)).expect("definition");

        let refs = compute_references("Build", location, src.to_string(), &HashMap::new(), false);
        assert_eq!(refs.len(), 1, "the dependsOn reference");
        assert_eq!(refs[0].range.start.line, 4);
    }

    #[test]
    fn semantic_tokens_schema_section_name_classified_as_type() {
        let src = "@SchemaFile\nSchema [Container]{ x?: str; };\n";
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
    fn section_double_colon_completion_resolves_registered_fields() {
        let src = "[MainCont]{\n    padding: int = 1;\n    margin: int = 2;\n};\n";
        let symbols = resolve_src(src);
        let path = vec!["MainCont".to_string()];
        let section = symbols
            .sections
            .get(&path)
            .expect("MainCont section must be registered");
        let items = section_field_completions(&symbols, &path, section);
        assert_eq!(items.len(), 2);
        assert!(items.iter().any(|item| item.label == "padding"));
        assert!(items.iter().any(|item| item.label == "margin"));
    }

    #[test]
    fn resolve_field_type_display_resolves_top_level_inferred_field() {
        let src = concat!(
            "type [Human]{ name: str; age: int; };\n",
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
            "type [EnvironmentType]{ nodeEnv: str; port: str; };\n",
            "type [ServiceType]{ image: str; environment: EnvironmentType; };\n",
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
        assert_eq!(
            resolve_field_type_display(&symbols, &path, "nodeEnv"),
            "str"
        );
        assert_eq!(resolve_field_type_display(&symbols, &path, "port"), "str");
    }

    #[test]
    fn resolve_field_type_display_named_shape_shows_type_name() {
        let src = concat!(
            "type [Border]{ width: int; };\n",
            "type [Decoration]{ border: Border; };\n",
            "[Style] -> Decoration {\n",
            "    border: { width: 2; };\n",
            "};\n",
        );
        let symbols = resolve_src(src);
        let path = vec!["Style".to_string()];
        assert_eq!(
            resolve_field_type_display(&symbols, &path, "border"),
            "Border"
        );
    }

    #[test]
    fn phase0_keywords_are_completed() {
        let labels: Vec<String> = keyword_items().into_iter().map(|item| item.label).collect();
        for keyword in ["mut", "break", "continue"] {
            assert!(
                labels.iter().any(|label| label == keyword),
                "missing {keyword}"
            );
        }
    }

    #[test]
    fn phase0_scripting_words_receive_semantic_tokens() {
        let src = concat!(
            "function main() -> void {\n",
            "    var mut count: int = 0;\n",
            "    for (index, value) in [1] { count = index + value; break; }\n",
            "    return;\n",
            "};\n",
        );
        let tokens = decode_semantic_tokens(src);
        assert_eq!(find_tok(&tokens, "void", src).unwrap().token_type, TT_TYPE);
        assert_eq!(
            find_tok(&tokens, "mut", src).unwrap().token_type,
            TT_KEYWORD
        );
        assert_eq!(
            find_tok(&tokens, "index", src).unwrap().token_type,
            TT_VARIABLE
        );
    }

    #[test]
    fn lsp_discovers_package_lock_for_project_sources() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(temp.path().join("spar.package.spar"), "[Package]{};").unwrap();
        spar::package::Lockfile::default()
            .write_atomically(&temp.path().join("spar.package.lock.spar"))
            .unwrap();
        let options = SparLanguageServer::compile_options(&src);
        assert!(options.locator.is_some());
    }

    #[test]
    fn lsp_analysis_preloads_manifest_schema_from_document_path() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("spar.package.spar");
        let state = SparLanguageServer::analyze_path(
            concat!(
                "[Package] -> SparPackage {\n",
                "    version: \"1.0.0\";\n",
                "    kind: \"application\";\n",
                "};\n",
            ),
            &path,
        );
        let errors = state
            .errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(errors.contains("missing required field 'name'"), "{errors}");
        assert!(
            !errors.contains("undefined type: `SparPackage`"),
            "{errors}"
        );
    }

    #[test]
    fn manifest_completion_offers_fields_and_kind_values() {
        let path = std::path::Path::new("spar.package.spar");
        let source = "[Package] -> SparPackage {\n    \n};\n";
        let fields =
            package_metadata_completion_items(path, source, source.find("    ").unwrap() + 4)
                .expect("package field completion");
        for label in ["name", "version", "kind", "entry"] {
            assert!(
                fields.iter().any(|item| item.label == label),
                "missing {label}"
            );
        }

        let source = "[Package] -> SparPackage {\n    kind: \"\";\n};\n";
        let offset = source.find("\"\"").unwrap() + 1;
        let values = package_metadata_completion_items(path, source, offset)
            .expect("package kind completion");
        assert_eq!(
            values
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>(),
            ["application", "library", "config"]
        );
    }
}

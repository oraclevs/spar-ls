// ── Compiler-derived workspace/export index ─────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SymbolId(String);

impl SymbolId {
    fn new(uri: &Url, kind: IndexedSymbolKind, semantic_path: &str, span: &Span) -> Self {
        Self(format!(
            "{}#{}:{}:{}:{}",
            uri,
            kind.as_key(),
            semantic_path,
            span.start,
            span.end
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum IndexedSymbolKind {
    Variable,
    Callable,
    Function,
    Struct,
    Constructor,
    Method,
    Type,
    Enum,
    EnumVariant,
    Section,
    Field,
    FunctionGroup,
    FunctionGroupMember,
    Task,
}

impl IndexedSymbolKind {
    fn as_key(self) -> &'static str {
        match self {
            Self::Variable => "variable",
            Self::Callable => "callable",
            Self::Function => "function",
            Self::Struct => "struct",
            Self::Constructor => "constructor",
            Self::Method => "method",
            Self::Type => "type",
            Self::Enum => "enum",
            Self::EnumVariant => "enum-variant",
            Self::Section => "section",
            Self::Field => "field",
            Self::FunctionGroup => "function-group",
            Self::FunctionGroupMember => "function-group-member",
            Self::Task => "task",
        }
    }

    fn symbol_kind(self) -> SymbolKind {
        match self {
            Self::Variable => SymbolKind::VARIABLE,
            Self::Callable | Self::Function | Self::FunctionGroupMember => SymbolKind::FUNCTION,
            Self::Struct => SymbolKind::STRUCT,
            Self::Constructor => SymbolKind::CONSTRUCTOR,
            Self::Method => SymbolKind::METHOD,
            Self::Type => SymbolKind::CLASS,
            Self::Enum => SymbolKind::ENUM,
            Self::EnumVariant => SymbolKind::ENUM_MEMBER,
            Self::Section | Self::FunctionGroup => SymbolKind::MODULE,
            Self::Field => SymbolKind::FIELD,
            Self::Task => SymbolKind::FUNCTION,
        }
    }

    fn is_type(self) -> bool {
        matches!(self, Self::Struct | Self::Type | Self::Enum)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CallableParam {
    name: String,
    ty: String,
    has_default: bool,
    default_repr: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallableArgumentStyle {
    Named,
    Positional,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CallableSignature {
    name: String,
    params: Vec<CallableParam>,
    return_type: String,
    is_async: bool,
    origin: Option<String>,
    argument_style: CallableArgumentStyle,
}

impl CallableSignature {
    fn label(&self) -> String {
        let params = self
            .params
            .iter()
            .map(|param| {
                let mut rendered = format!("{}: {}", param.name, param.ty);
                if param.has_default {
                    rendered.push_str(" = ");
                    rendered.push_str(param.default_repr.as_deref().unwrap_or("…"));
                }
                rendered
            })
            .collect::<Vec<_>>()
            .join(", ");
        let prefix = if self.is_async { "async " } else { "" };
        format!("{prefix}{}({params}) -> {}", self.name, self.return_type)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexedMethodReceiver {
    Shared,
    Mutable,
    Static,
}

#[derive(Debug, Clone)]
struct IndexedSymbol {
    id: SymbolId,
    name: String,
    semantic_path: String,
    kind: IndexedSymbolKind,
    uri: Url,
    range: Range,
    selection_range: Range,
    container_name: Option<String>,
    exported: bool,
    private: bool,
    signature: Option<CallableSignature>,
    method_receiver: Option<IndexedMethodReceiver>,
    detail: Option<String>,
    documentation: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct IndexedModule {
    symbols: Vec<IndexedSymbol>,
    exports: Vec<IndexedSymbol>,
}

#[derive(Debug, Clone, Default)]
struct WorkspaceIndex {
    modules: HashMap<Url, IndexedModule>,
}

fn byte_offset_to_lsp_position(source: &str, offset: usize) -> Position {
    let clamped = offset.min(source.len());
    let mut line = 0u32;
    let mut line_start = 0usize;
    for (index, ch) in source.char_indices() {
        if index >= clamped {
            break;
        }
        if ch == '\n' {
            line += 1;
            line_start = index + ch.len_utf8();
        }
    }
    let character = source[line_start..clamped].encode_utf16().count() as u32;
    Position { line, character }
}

fn span_to_lsp_range(source: &str, span: &Span) -> Range {
    Range {
        start: byte_offset_to_lsp_position(source, span.start),
        end: byte_offset_to_lsp_position(source, span.end.max(span.start + 1)),
    }
}

fn expr_source(expr: &spar::ast::Expr) -> Option<String> {
    Some(spar::formatter::format_expression(expr))
}

fn signature_from_entry(
    name: &str,
    entry: &FunctionEntry,
    _source: &str,
    raw_decl: Option<&spar::ast::FunctionDecl>,
    origin: Option<String>,
) -> CallableSignature {
    let raw_defaults = raw_decl
        .map(|decl| {
            decl.params
                .iter()
                .map(|param| {
                    (
                        param.name.clone(),
                        param
                            .default
                            .as_ref()
                            .and_then(expr_source),
                    )
                })
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();

    CallableSignature {
        name: name.to_string(),
        params: entry
            .params
            .iter()
            .map(|(param_name, ty)| CallableParam {
                name: param_name.clone(),
                ty: format_spar_type(ty),
                has_default: entry.default_params.contains(param_name),
                default_repr: raw_defaults.get(param_name).cloned().flatten(),
            })
            .collect(),
        return_type: format_spar_type(&entry.ret),
        is_async: entry.is_async,
        origin,
        argument_style: CallableArgumentStyle::Named,
    }
}

fn signature_from_callable_type(
    name: &str,
    ty: &spar::ast::SparType,
    raw_decl: Option<&spar::ast::VarDecl>,
    origin: Option<String>,
) -> Option<CallableSignature> {
    let spar::ast::SparType::Function { params, return_type } = ty else {
        return None;
    };
    let closure_names = raw_decl
        .and_then(|decl| decl.value.as_ref())
        .and_then(|value| match value {
            spar::ast::Expr::Closure { params: closure_params, .. }
                if closure_params.len() == params.len() => {
                Some(closure_params.iter().map(|param| param.name.clone()).collect::<Vec<_>>())
            }
            _ => None,
        });
    Some(CallableSignature {
        name: name.to_string(),
        params: params
            .iter()
            .enumerate()
            .map(|(index, ty)| CallableParam {
                name: closure_names
                    .as_ref()
                    .and_then(|names| names.get(index))
                    .cloned()
                    .unwrap_or_else(|| format!("arg{}", index + 1)),
                ty: format_spar_type(ty),
                has_default: false,
                default_repr: None,
            })
            .collect(),
        return_type: format_spar_type(return_type),
        is_async: false,
        origin,
        argument_style: CallableArgumentStyle::Positional,
    })
}

fn impl_owner_name(ty: &spar::ast::SparType) -> Option<String> {
    match ty {
        spar::ast::SparType::Named(name) => Some(name.clone()),
        spar::ast::SparType::Applied { name, .. } => Some(name.clone()),
        spar::ast::SparType::Str => Some("str".to_string()),
        spar::ast::SparType::List(_) => Some("List".to_string()),
        _ => None,
    }
}

fn constructor_signature(
    name: &str,
    decl: &spar::ast::SectionDecl,
    origin: Option<String>,
) -> CallableSignature {
    let params = decl
        .items
        .iter()
        .filter_map(|item| match item {
            spar::ast::SectionItem::Field(field) => Some(CallableParam {
                name: field.name.clone(),
                ty: field
                    .ty
                    .as_ref()
                    .map(format_spar_type)
                    .unwrap_or_else(|| "dynamic".to_string()),
                has_default: field.value.is_some(),
                default_repr: field.value.as_ref().map(|value| match value {
                    spar::ast::FieldValue::Expr(expr) => spar::formatter::format_expression(expr),
                    spar::ast::FieldValue::Nested(_) => "{ … }".to_string(),
                }),
            }),
            spar::ast::SectionItem::Spread(_) => None,
        })
        .collect();
    CallableSignature {
        name: name.to_string(),
        params,
        return_type: name.to_string(),
        is_async: false,
        origin,
        argument_style: CallableArgumentStyle::Named,
    }
}

fn raw_program_for_source(source: &str) -> Option<Program> {
    // Tolerates a half-typed statement (see `parse_with_statement_repair`).
    parse_with_statement_repair(source)
}

fn normalize_documentation_comment(text: &str) -> String {
    let trimmed = text.trim();
    if let Some(line) = trimmed.strip_prefix("//") {
        return line.trim_start().to_string();
    }
    if let Some(block) = trimmed.strip_prefix("/*").and_then(|value| value.strip_suffix("*/")) {
        return block
            .lines()
            .map(|line| line.trim().trim_start_matches('*').trim_start())
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string();
    }
    trimmed.to_string()
}

fn documentation_cursor(source: &str, declaration_start: usize) -> usize {
    let declaration_start = declaration_start.min(source.len());
    let line_start = source[..declaration_start]
        .rfind('\n')
        .map_or(0, |index| index + 1);
    let prefix = source[line_start..declaration_start].trim();
    if prefix.is_empty() {
        return line_start;
    }
    let allowed = [
        "export", "private", "async", "struct", "function", "var", "dynamic",
        "type", "enum", "section", "schema", "task",
    ];
    if prefix
        .split_whitespace()
        .all(|word| allowed.contains(&word.trim_matches(|ch: char| !ch.is_ascii_alphabetic())))
    {
        line_start
    } else {
        declaration_start
    }
}

fn leading_documentation(source: &str, declaration_start: usize) -> Option<String> {
    let (_, comments) = spar::lexer::Lexer::new(source).tokenize_with_comments().ok()?;
    let mut cursor = documentation_cursor(source, declaration_start);
    let mut parts = Vec::new();

    for comment in comments
        .iter()
        .filter(|comment| !comment.is_trailing)
        .rev()
    {
        if comment.start >= cursor {
            continue;
        }
        let end = comment.start.saturating_add(comment.text.len()).min(source.len());
        if end > cursor {
            continue;
        }
        let gap = &source[end..cursor];
        if !gap.chars().all(char::is_whitespace) || gap.chars().filter(|ch| *ch == '\n').count() > 1 {
            break;
        }
        let text = normalize_documentation_comment(&comment.text);
        if !text.is_empty() {
            parts.push(text);
        }
        cursor = comment.start;
    }

    if parts.is_empty() {
        None
    } else {
        parts.reverse();
        Some(parts.join("\n"))
    }
}

impl WorkspaceIndex {
    fn replace_document(&mut self, uri: &Url, state: &DocumentState) {
        let Some(symbols) = state.effective_symbols() else {
            return;
        };
        // Index only declarations physically owned by this document. The
        // compiler symbol table also contains injected prelude/import-spliced
        // entries, which must never be advertised as exports of every file.
        // If the current source is temporarily unparsable, preserve the
        // previous last-known-good module entry instead of replacing it.
        let Some(raw_program) = raw_program_for_source(&state.source) else {
            return;
        };
        let raw_functions = raw_program
            .items
            .iter()
            .filter_map(|item| match item {
                TopLevelItem::Function(function) => Some((function.name.clone(), function.clone())),
                _ => None,
            })
            .collect::<HashMap<_, _>>();
        let raw_globals = raw_program
            .items
            .iter()
            .filter_map(|item| match item {
                TopLevelItem::Var(decl) => Some((decl.name.clone(), decl.clone())),
                _ => None,
            })
            .collect::<HashMap<_, _>>();
        let raw_dynamics = raw_program
            .items
            .iter()
            .filter_map(|item| match item {
                TopLevelItem::Dynamic(decl) => Some(decl.name.clone()),
                _ => None,
            })
            .collect::<HashSet<_>>();
        let raw_types = raw_program
            .items
            .iter()
            .filter_map(|item| match item {
                TopLevelItem::Type(decl) => Some((decl.name.clone(), decl.clone())),
                _ => None,
            })
            .collect::<HashMap<_, _>>();
        let raw_enums = raw_program
            .items
            .iter()
            .filter_map(|item| match item {
                TopLevelItem::Enum(decl) => Some((decl.name.clone(), decl.clone())),
                _ => None,
            })
            .collect::<HashMap<_, _>>();
        let raw_sections = raw_program
            .items
            .iter()
            .filter_map(|item| match item {
                TopLevelItem::Section(decl) => {
                    decl.path.first().cloned().map(|name| (name, decl.clone()))
                }
                _ => None,
            })
            .collect::<HashMap<_, _>>();
        let raw_impls = raw_program
            .items
            .iter()
            .filter_map(|item| match item {
                TopLevelItem::Impl(decl) => Some(decl.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let raw_groups = raw_program
            .items
            .iter()
            .filter_map(|item| match item {
                TopLevelItem::FunctionGroup(decl) => Some((decl.name.clone(), decl.clone())),
                _ => None,
            })
            .collect::<HashMap<_, _>>();
        let raw_tasks = raw_program
            .items
            .iter()
            .filter_map(|item| match item {
                TopLevelItem::Task(decl) => Some((decl.name.clone(), decl.as_ref().clone())),
                _ => None,
            })
            .collect::<HashMap<_, _>>();

        let mut module = IndexedModule {
            symbols: Vec::new(),
            exports: Vec::new(),
        };

        let mut push_symbol = |symbol: IndexedSymbol| {
            if symbol.exported && !symbol.private {
                module.exports.push(symbol.clone());
            }
            module.symbols.push(symbol);
        };

        for (name, entry) in &symbols.globals {
            if !raw_globals.contains_key(name) && !raw_dynamics.contains(name) {
                continue;
            }
            match entry {
                GlobalEntry::Var {
                    ty, exported, span, ..
                } => {
                    let range = span_to_lsp_range(&state.source, span);
                    let signature = signature_from_callable_type(
                        name,
                        ty,
                        raw_globals.get(name),
                        Some(uri.to_string()),
                    );
                    let kind = if signature.is_some() {
                        IndexedSymbolKind::Callable
                    } else {
                        IndexedSymbolKind::Variable
                    };
                    push_symbol(IndexedSymbol {
                        id: SymbolId::new(uri, kind, name, span),
                        name: name.clone(),
                        semantic_path: name.clone(),
                        kind,
                        uri: uri.clone(),
                        range,
                        selection_range: range,
                        container_name: None,
                        exported: *exported,
                        private: !*exported,
                        signature,
                        method_receiver: None,
                        detail: Some(format_spar_type(ty)),
                        documentation: raw_globals
                            .get(name)
                            .and_then(|decl| leading_documentation(&state.source, decl.span.start)),
                    });
                }
                GlobalEntry::Dynamic { span, .. } => {
                    let range = span_to_lsp_range(&state.source, span);
                    push_symbol(IndexedSymbol {
                        id: SymbolId::new(uri, IndexedSymbolKind::Variable, name, span),
                        name: name.clone(),
                        semantic_path: name.clone(),
                        kind: IndexedSymbolKind::Variable,
                        uri: uri.clone(),
                        range,
                        selection_range: range,
                        container_name: None,
                        exported: false,
                        private: true,
                        signature: None,
                        method_receiver: None,
                        detail: Some("dynamic".to_string()),
                        documentation: None,
                    });
                }
            }
        }

        for (name, entry) in &symbols.functions {
            if !raw_functions.contains_key(name) {
                continue;
            }
            let range = span_to_lsp_range(&state.source, &entry.span);
            let signature = signature_from_entry(
                name,
                entry,
                &state.source,
                raw_functions.get(name),
                Some(uri.to_string()),
            );
            push_symbol(IndexedSymbol {
                id: SymbolId::new(uri, IndexedSymbolKind::Function, name, &entry.span),
                name: name.clone(),
                semantic_path: name.clone(),
                kind: IndexedSymbolKind::Function,
                uri: uri.clone(),
                range,
                selection_range: range,
                container_name: None,
                exported: !entry.is_private,
                private: entry.is_private,
                signature: Some(signature.clone()),
                method_receiver: None,
                detail: Some(signature.label()),
                documentation: raw_functions
                    .get(name)
                    .and_then(|decl| leading_documentation(&state.source, decl.span.start)),
            });
        }

        for (name, entry) in &symbols.types {
            if !raw_types.contains_key(name) {
                continue;
            }
            let range = span_to_lsp_range(&state.source, &entry.span);
            push_symbol(IndexedSymbol {
                id: SymbolId::new(uri, IndexedSymbolKind::Type, name, &entry.span),
                name: name.clone(),
                semantic_path: name.clone(),
                kind: IndexedSymbolKind::Type,
                uri: uri.clone(),
                range,
                selection_range: range,
                container_name: None,
                exported: entry.exported,
                private: !entry.exported,
                signature: None,
                method_receiver: None,
                detail: Some("type".to_string()),
                documentation: raw_types
                    .get(name)
                    .and_then(|decl| leading_documentation(&state.source, decl.span.start)),
            });
        }

        for (type_name, decl) in &raw_types {
            for field in &decl.fields {
                let range = span_to_lsp_range(&state.source, &field.span);
                push_symbol(IndexedSymbol {
                    id: SymbolId::new(
                        uri,
                        IndexedSymbolKind::Field,
                        &format!("{type_name}::{}", field.name),
                        &field.span,
                    ),
                    name: field.name.clone(),
                    semantic_path: format!("{type_name}::{}", field.name),
                    kind: IndexedSymbolKind::Field,
                    uri: uri.clone(),
                    range,
                    selection_range: range,
                    container_name: Some(type_name.clone()),
                    exported: false,
                    private: false,
                    signature: None,
                    method_receiver: None,
                    detail: Some(format_type_field_shape(&field.shape)),
                    documentation: leading_documentation(&state.source, field.span.start),
                });
            }
        }

        for (name, entry) in &symbols.enums {
            if !raw_enums.contains_key(name) {
                continue;
            }
            let range = span_to_lsp_range(&state.source, &entry.span);
            push_symbol(IndexedSymbol {
                id: SymbolId::new(uri, IndexedSymbolKind::Enum, name, &entry.span),
                name: name.clone(),
                semantic_path: name.clone(),
                kind: IndexedSymbolKind::Enum,
                uri: uri.clone(),
                range,
                selection_range: range,
                container_name: None,
                exported: entry.exported,
                private: !entry.exported,
                signature: None,
                method_receiver: None,
                detail: Some("enum".to_string()),
                documentation: raw_enums
                    .get(name)
                    .and_then(|decl| leading_documentation(&state.source, decl.span.start)),
            });
        }

        for (enum_name, decl) in &raw_enums {
            for variant in &decl.variants {
                let range = ident_range_in_span(&state.source, &decl.span, variant);
                push_symbol(IndexedSymbol {
                    id: SymbolId(format!("{}#enum-variant:{enum_name}::{variant}", uri)),
                    name: variant.clone(),
                    semantic_path: format!("{enum_name}::{variant}"),
                    kind: IndexedSymbolKind::EnumVariant,
                    uri: uri.clone(),
                    range,
                    selection_range: range,
                    container_name: Some(enum_name.clone()),
                    exported: false,
                    private: false,
                    signature: None,
                    method_receiver: None,
                    detail: Some(format!("{enum_name} variant")),
                    documentation: None,
                });
            }
        }

        for (path, entry) in &symbols.sections {
            if path.len() != 1 {
                continue;
            }
            let name = path[0].clone();
            if !raw_sections.contains_key(&name) {
                continue;
            }
            let range = span_to_lsp_range(&state.source, &entry.span);
            let kind = if entry.canonical {
                IndexedSymbolKind::Struct
            } else {
                IndexedSymbolKind::Section
            };
            push_symbol(IndexedSymbol {
                id: SymbolId::new(uri, kind, &name, &entry.span),
                name: name.clone(),
                semantic_path: name.clone(),
                kind,
                uri: uri.clone(),
                range,
                selection_range: range,
                container_name: None,
                exported: entry.exported,
                private: entry.private,
                signature: None,
                method_receiver: None,
                detail: Some(if entry.canonical { "struct" } else { "section" }.to_string()),
                documentation: raw_sections
                    .get(&name)
                    .and_then(|decl| leading_documentation(&state.source, decl.span.start)),
            });
            if entry.canonical {
                if let Some(decl) = raw_sections.get(&name) {
                    let constructor = constructor_signature(&name, decl, Some(uri.to_string()));
                    push_symbol(IndexedSymbol {
                        id: SymbolId::new(
                            uri,
                            IndexedSymbolKind::Constructor,
                            &format!("{name}::constructor"),
                            &entry.span,
                        ),
                        name: name.clone(),
                        semantic_path: format!("{name}::constructor"),
                        kind: IndexedSymbolKind::Constructor,
                        uri: uri.clone(),
                        range,
                        selection_range: range,
                        container_name: Some(name.clone()),
                        exported: false,
                        private: entry.private,
                        signature: Some(constructor.clone()),
                        method_receiver: None,
                        detail: Some(constructor.label()),
                        documentation: leading_documentation(&state.source, decl.span.start),
                    });
                }
            }
        }

        for (section_name, decl) in &raw_sections {
            for item in &decl.items {
                let spar::ast::SectionItem::Field(field) = item else {
                    continue;
                };
                let range = span_to_lsp_range(&state.source, &field.span);
                push_symbol(IndexedSymbol {
                    id: SymbolId::new(
                        uri,
                        IndexedSymbolKind::Field,
                        &format!("{section_name}::{}", field.name),
                        &field.span,
                    ),
                    name: field.name.clone(),
                    semantic_path: format!("{section_name}::{}", field.name),
                    kind: IndexedSymbolKind::Field,
                    uri: uri.clone(),
                    range,
                    selection_range: range,
                    container_name: Some(section_name.clone()),
                    exported: false,
                    private: decl.private,
                    signature: None,
                    method_receiver: None,
                    detail: field
                        .ty
                        .as_ref()
                        .map(format_spar_type)
                        .or_else(|| Some("field".to_string())),
                    documentation: leading_documentation(&state.source, field.span.start),
                });
            }
        }

        for decl in &raw_impls {
            let Some(owner) = impl_owner_name(&decl.target) else {
                continue;
            };
            let Some(indexed_methods) = symbols.methods.get(&owner) else {
                continue;
            };
            for method in &decl.methods {
                let Some(entry) = indexed_methods.get(&method.function.name) else {
                    continue;
                };
                let range = span_to_lsp_range(&state.source, &method.function.name_span);
                let mut signature = signature_from_entry(
                    &method.function.name,
                    &entry.function,
                    &state.source,
                    Some(&method.function),
                    Some(format!("{}::{owner}", uri)),
                );
                if entry.has_receiver
                    && signature.params.first().is_some_and(|param| param.name == "self")
                {
                    signature.params.remove(0);
                }
                signature.argument_style = CallableArgumentStyle::Positional;
                let receiver = if !entry.has_receiver {
                    IndexedMethodReceiver::Static
                } else if entry.receiver_mutable {
                    IndexedMethodReceiver::Mutable
                } else {
                    IndexedMethodReceiver::Shared
                };
                push_symbol(IndexedSymbol {
                    id: SymbolId::new(
                        uri,
                        IndexedSymbolKind::Method,
                        &format!("{owner}::{}", method.function.name),
                        &method.function.name_span,
                    ),
                    name: method.function.name.clone(),
                    semantic_path: format!("{owner}::{}", method.function.name),
                    kind: IndexedSymbolKind::Method,
                    uri: uri.clone(),
                    range,
                    selection_range: range,
                    container_name: Some(owner.clone()),
                    exported: false,
                    private: method.function.is_private,
                    signature: Some(signature.clone()),
                    method_receiver: Some(receiver),
                    detail: Some(signature.label()),
                    documentation: leading_documentation(&state.source, method.function.span.start),
                });
            }
        }

        for (name, group) in &symbols.function_groups {
            if !raw_groups.contains_key(name) {
                continue;
            }
            let range = span_to_lsp_range(&state.source, &group.span);
            push_symbol(IndexedSymbol {
                id: SymbolId::new(uri, IndexedSymbolKind::FunctionGroup, name, &group.span),
                name: name.clone(),
                semantic_path: name.clone(),
                kind: IndexedSymbolKind::FunctionGroup,
                uri: uri.clone(),
                range,
                selection_range: range,
                container_name: None,
                exported: !group.is_private,
                private: group.is_private,
                signature: None,
                method_receiver: None,
                detail: Some("function group".to_string()),
                documentation: raw_groups
                    .get(name)
                    .and_then(|decl| leading_documentation(&state.source, decl.span.start)),
            });
        }

        for (group_name, decl) in &raw_groups {
            for function in &decl.functions {
                let Some(entry) = symbols
                    .function_groups
                    .get(group_name)
                    .and_then(|group| group.functions.get(&function.name))
                else {
                    continue;
                };
                let range = span_to_lsp_range(&state.source, &function.name_span);
                let signature = signature_from_entry(
                    &function.name,
                    entry,
                    &state.source,
                    Some(function),
                    Some(format!("{}::{group_name}", uri)),
                );
                push_symbol(IndexedSymbol {
                    id: SymbolId::new(
                        uri,
                        IndexedSymbolKind::FunctionGroupMember,
                        &format!("{group_name}::{}", function.name),
                        &function.name_span,
                    ),
                    name: function.name.clone(),
                    semantic_path: format!("{group_name}::{}", function.name),
                    kind: IndexedSymbolKind::FunctionGroupMember,
                    uri: uri.clone(),
                    range,
                    selection_range: range,
                    container_name: Some(group_name.clone()),
                    exported: false,
                    private: decl.is_private || function.is_private,
                    signature: Some(signature.clone()),
                    method_receiver: None,
                    detail: Some(signature.label()),
                    documentation: leading_documentation(&state.source, function.span.start),
                });
            }
        }

        for (name, task) in &symbols.tasks {
            if !raw_tasks.contains_key(name) {
                continue;
            }
            let range = span_to_lsp_range(&state.source, &task.name_span);
            let signature = CallableSignature {
                name: name.clone(),
                params: task
                    .params
                    .iter()
                    .map(|(name, ty)| CallableParam {
                        name: name.clone(),
                        ty: format_spar_type(ty),
                        has_default: false,
                        default_repr: None,
                    })
                    .collect(),
                return_type: "task".to_string(),
                is_async: false,
                origin: Some(uri.to_string()),
                argument_style: CallableArgumentStyle::Named,
            };
            push_symbol(IndexedSymbol {
                id: SymbolId::new(uri, IndexedSymbolKind::Task, name, &task.name_span),
                name: name.clone(),
                semantic_path: name.clone(),
                kind: IndexedSymbolKind::Task,
                uri: uri.clone(),
                range,
                selection_range: range,
                container_name: None,
                exported: true,
                private: false,
                signature: Some(signature.clone()),
                method_receiver: None,
                detail: Some(signature.label()),
                documentation: raw_tasks
                    .get(name)
                    .and_then(|decl| leading_documentation(&state.source, decl.name_span.start)),
            });
        }

        module.symbols.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| a.semantic_path.cmp(&b.semantic_path))
        });
        module.exports.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| a.semantic_path.cmp(&b.semantic_path))
        });
        self.modules.insert(uri.clone(), module);
    }

    fn remove_document(&mut self, uri: &Url) {
        self.modules.remove(uri);
    }

    #[allow(dead_code)]
    fn exports_for_uri(&self, uri: &Url) -> &[IndexedSymbol] {
        self.modules
            .get(uri)
            .map(|module| module.exports.as_slice())
            .unwrap_or(&[])
    }

    fn symbols_for_uri(&self, uri: &Url) -> &[IndexedSymbol] {
        self.modules
            .get(uri)
            .map(|module| module.symbols.as_slice())
            .unwrap_or(&[])
    }

    fn constructor_for_owner(&self, uri: &Url, owner: &str) -> Option<&IndexedSymbol> {
        self.modules.get(uri)?.symbols.iter().find(|symbol| {
            symbol.kind == IndexedSymbolKind::Constructor
                && symbol.container_name.as_deref() == Some(owner)
        })
    }

    fn methods_for_owner(&self, uri: &Url, owner: &str) -> Vec<&IndexedSymbol> {
        let mut methods = self
            .modules
            .get(uri)
            .into_iter()
            .flat_map(|module| module.symbols.iter())
            .filter(|symbol| {
                symbol.kind == IndexedSymbolKind::Method
                    && symbol.container_name.as_deref() == Some(owner)
            })
            .collect::<Vec<_>>();
        methods.sort_by(|a, b| a.name.cmp(&b.name));
        methods
    }

    fn visible_constructor_for_owner(&self, current_uri: &Url, owner: &str) -> Option<&IndexedSymbol> {
        if let Some(local) = self.constructor_for_owner(current_uri, owner) {
            return Some(local);
        }
        self.modules.iter().find_map(|(uri, module)| {
            let exported_owner = module.symbols.iter().any(|symbol| {
                symbol.kind == IndexedSymbolKind::Struct
                    && symbol.name == owner
                    && symbol.exported
                    && !symbol.private
            });
            if !exported_owner || uri == current_uri {
                return None;
            }
            module.symbols.iter().find(|symbol| {
                symbol.kind == IndexedSymbolKind::Constructor
                    && symbol.container_name.as_deref() == Some(owner)
            })
        })
    }

    fn visible_methods_for_owner(&self, current_uri: &Url, owner: &str) -> Vec<&IndexedSymbol> {
        let mut methods = self.methods_for_owner(current_uri, owner);
        for (uri, module) in &self.modules {
            if uri == current_uri {
                continue;
            }
            let exported_owner = module.symbols.iter().any(|symbol| {
                symbol.kind == IndexedSymbolKind::Struct
                    && symbol.name == owner
                    && symbol.exported
                    && !symbol.private
            });
            if !exported_owner {
                continue;
            }
            methods.extend(module.symbols.iter().filter(|symbol| {
                symbol.kind == IndexedSymbolKind::Method
                    && symbol.container_name.as_deref() == Some(owner)
                    && !symbol.private
            }));
        }
        methods.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.uri.as_str().cmp(b.uri.as_str())));
        methods.dedup_by(|a, b| a.name == b.name && a.uri == b.uri);
        methods
    }

    fn visible_callable_named(&self, current_uri: &Url, name: &str) -> Option<&IndexedSymbol> {
        if let Some(local) = self.modules.get(current_uri).and_then(|module| {
            module.symbols.iter().find(|symbol| {
                symbol.kind == IndexedSymbolKind::Callable && symbol.name == name
            })
        }) {
            return Some(local);
        }
        self.modules
            .values()
            .flat_map(|module| module.exports.iter())
            .find(|symbol| {
                symbol.kind == IndexedSymbolKind::Callable
                    && symbol.name == name
                    && !symbol.private
            })
    }

    fn search(&self, query: &str) -> Vec<&IndexedSymbol> {
        let needle = query.to_lowercase();
        let mut matches = self
            .modules
            .values()
            .flat_map(|module| module.symbols.iter())
            .filter(|symbol| {
                !symbol.private
                    && (needle.is_empty()
                        || symbol.name.to_lowercase().contains(&needle)
                        || symbol.semantic_path.to_lowercase().contains(&needle))
            })
            .collect::<Vec<_>>();
        matches.sort_by(|a, b| {
            let a_prefix = a.name.to_lowercase().starts_with(&needle);
            let b_prefix = b.name.to_lowercase().starts_with(&needle);
            b_prefix
                .cmp(&a_prefix)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.uri.as_str().cmp(b.uri.as_str()))
        });
        matches
    }

    fn find_by_name(&self, name: &str) -> Vec<&IndexedSymbol> {
        let mut matches = self
            .modules
            .values()
            .flat_map(|module| module.symbols.iter())
            .filter(|symbol| symbol.name == name)
            .collect::<Vec<_>>();
        matches.sort_by(|a, b| a.uri.as_str().cmp(b.uri.as_str()));
        matches
    }

    fn find_by_id(&self, id: &str) -> Option<&IndexedSymbol> {
        self.modules
            .values()
            .flat_map(|module| module.symbols.iter())
            .find(|symbol| symbol.id.0 == id)
    }
}

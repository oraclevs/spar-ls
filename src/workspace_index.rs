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
    Function,
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
            Self::Function => "function",
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
            Self::Function | Self::FunctionGroupMember => SymbolKind::FUNCTION,
            Self::Type => SymbolKind::CLASS,
            Self::Enum => SymbolKind::ENUM,
            Self::EnumVariant => SymbolKind::ENUM_MEMBER,
            Self::Section | Self::FunctionGroup => SymbolKind::MODULE,
            Self::Field => SymbolKind::FIELD,
            Self::Task => SymbolKind::FUNCTION,
        }
    }

    fn is_type(self) -> bool {
        matches!(self, Self::Type | Self::Enum)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CallableParam {
    name: String,
    ty: String,
    has_default: bool,
    default_repr: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CallableSignature {
    name: String,
    params: Vec<CallableParam>,
    return_type: String,
    is_async: bool,
    origin: Option<String>,
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
    detail: Option<String>,
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
    source: &str,
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
    }
}

fn raw_program_for_source(source: &str) -> Option<Program> {
    // Tolerates a half-typed statement (see `parse_with_statement_repair`).
    parse_with_statement_repair(source)
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
                TopLevelItem::Var(decl) => Some(decl.name.clone()),
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
            if !raw_globals.contains(name) {
                continue;
            }
            match entry {
                GlobalEntry::Var {
                    ty, exported, span, ..
                } => {
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
                        exported: *exported,
                        private: !*exported,
                        signature: None,
                        detail: Some(format_spar_type(ty)),
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
                        detail: Some("dynamic".to_string()),
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
                detail: Some(signature.label()),
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
                detail: Some("type".to_string()),
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
                    detail: Some(format_type_field_shape(&field.shape)),
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
                detail: Some("enum".to_string()),
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
                    detail: Some(format!("{enum_name} variant")),
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
            push_symbol(IndexedSymbol {
                id: SymbolId::new(uri, IndexedSymbolKind::Section, &name, &entry.span),
                name: name.clone(),
                semantic_path: name,
                kind: IndexedSymbolKind::Section,
                uri: uri.clone(),
                range,
                selection_range: range,
                container_name: None,
                exported: entry.exported,
                private: entry.private,
                signature: None,
                detail: Some("section".to_string()),
            });
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
                    detail: field
                        .ty
                        .as_ref()
                        .map(format_spar_type)
                        .or_else(|| Some("field".to_string())),
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
                detail: Some("function group".to_string()),
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
                    detail: Some(signature.label()),
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
                detail: Some(signature.label()),
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

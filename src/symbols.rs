// ── Standard document/workspace symbols ──────────────────────────────────────

fn ident_range_in_span(source: &str, span: &Span, name: &str) -> Range {
    let start = span.start.min(source.len());
    let end = span.end.max(start).min(source.len());
    let search_end = (end + 256).min(source.len());
    let haystack = &source[start..search_end];
    let relative = haystack.find(name).unwrap_or(0);
    let ident_start = start + relative;
    let ident_end = (ident_start + name.len()).min(source.len());
    Range {
        start: byte_offset_to_lsp_position(source, ident_start),
        end: byte_offset_to_lsp_position(source, ident_end),
    }
}

#[allow(deprecated)]
fn document_symbol(
    source: &str,
    name: String,
    kind: SymbolKind,
    span: &Span,
    selection_span: Option<&Span>,
    detail: Option<String>,
    children: Vec<DocumentSymbol>,
) -> DocumentSymbol {
    let selection_range = selection_span
        .map(|span| span_to_lsp_range(source, span))
        .unwrap_or_else(|| ident_range_in_span(source, span, &name));
    // Some declarations carry a span that covers only their first token (`struct`,
    // `function`), which would exclude the name. LSP requires selectionRange to be
    // inside range (VS Code throws otherwise), so widen range to cover the name
    // and every child.
    let mut range = span_to_lsp_range(source, span);
    let before = |a: Position, b: Position| (a.line, a.character) < (b.line, b.character);
    let mut cover = |inner: Range| {
        if before(inner.start, range.start) {
            range.start = inner.start;
        }
        if before(range.end, inner.end) {
            range.end = inner.end;
        }
    };
    cover(selection_range);
    for child in &children {
        cover(child.range);
    }
    DocumentSymbol {
        name,
        detail,
        kind,
        tags: None,
        deprecated: None,
        range,
        selection_range,
        children: (!children.is_empty()).then_some(children),
    }
}

fn field_document_symbols(source: &str, items: &[spar::ast::SectionItem]) -> Vec<DocumentSymbol> {
    let mut out = Vec::new();
    for item in items {
        let spar::ast::SectionItem::Field(field) = item else { continue; };
        let children = match &field.value {
            Some(spar::ast::FieldValue::Nested(items)) => field_document_symbols(source, items),
            _ => Vec::new(),
        };
        out.push(document_symbol(
            source,
            field.name.clone(),
            SymbolKind::FIELD,
            &field.span,
            None,
            field.ty.as_ref().map(format_spar_type),
            children,
        ));
    }
    out
}

fn document_symbols(state: &DocumentState) -> Vec<DocumentSymbol> {
    let Some(program) = raw_program_for_source(&state.source).or_else(|| state.ast.clone()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in &program.items {
        match item {
            TopLevelItem::Var(var) => out.push(document_symbol(
                &state.source,
                var.name.clone(),
                SymbolKind::VARIABLE,
                &var.span,
                None,
                Some(format_spar_type(&var.ty)),
                Vec::new(),
            )),
            TopLevelItem::Dynamic(var) => out.push(document_symbol(
                &state.source,
                var.name.clone(),
                SymbolKind::VARIABLE,
                &var.span,
                None,
                Some("dynamic".to_string()),
                Vec::new(),
            )),
            TopLevelItem::Impl(imp) => {
                let target = format_spar_type(&imp.target);
                let children = imp
                    .methods
                    .iter()
                    .map(|method| {
                        let function = &method.function;
                        let params = function
                            .params
                            .iter()
                            .map(|param| {
                                document_symbol(
                                    &state.source,
                                    param.name.clone(),
                                    SymbolKind::VARIABLE,
                                    &param.span,
                                    None,
                                    Some(format_spar_type(&param.ty)),
                                    Vec::new(),
                                )
                            })
                            .collect();
                        document_symbol(
                            &state.source,
                            function.name.clone(),
                            SymbolKind::METHOD,
                            &function.span,
                            Some(&function.name_span),
                            Some(format!("-> {}", format_spar_type(&function.ret))),
                            params,
                        )
                    })
                    .collect();
                out.push(document_symbol(
                    &state.source,
                    format!("impl {target}"),
                    SymbolKind::NAMESPACE,
                    &imp.span,
                    None,
                    Some("impl".to_string()),
                    children,
                ));
            }
            TopLevelItem::Function(function) => {
                let children = function.params.iter().map(|param| document_symbol(
                    &state.source,
                    param.name.clone(),
                    SymbolKind::VARIABLE,
                    &param.span,
                    None,
                    Some(format_spar_type(&param.ty)),
                    Vec::new(),
                )).collect();
                out.push(document_symbol(
                    &state.source,
                    function.name.clone(),
                    SymbolKind::FUNCTION,
                    &function.span,
                    Some(&function.name_span),
                    Some(format!("-> {}", format_spar_type(&function.ret))),
                    children,
                ));
            }
            TopLevelItem::Type(decl) => {
                let children = decl.fields.iter().map(|field| document_symbol(
                    &state.source,
                    field.name.clone(),
                    SymbolKind::FIELD,
                    &field.span,
                    None,
                    Some(format_type_field_shape(&field.shape)),
                    Vec::new(),
                )).collect();
                out.push(document_symbol(
                    &state.source,
                    decl.name.clone(),
                    SymbolKind::CLASS,
                    &decl.span,
                    Some(&decl.name_span),
                    Some("type".to_string()),
                    children,
                ));
            }
            TopLevelItem::Enum(decl) => {
                let children = decl.variants.iter().map(|variant| document_symbol(
                    &state.source,
                    variant.clone(),
                    SymbolKind::ENUM_MEMBER,
                    &decl.span,
                    None,
                    None,
                    Vec::new(),
                )).collect();
                out.push(document_symbol(
                    &state.source,
                    decl.name.clone(),
                    SymbolKind::ENUM,
                    &decl.span,
                    Some(&decl.name_span),
                    Some("enum".to_string()),
                    children,
                ));
            }
            TopLevelItem::Section(section) => {
                let name = section.path.join("::");
                out.push(document_symbol(
                    &state.source,
                    name,
                    SymbolKind::MODULE,
                    &section.span,
                    None,
                    Some("section".to_string()),
                    field_document_symbols(&state.source, &section.items),
                ));
            }
            TopLevelItem::FunctionGroup(group) => {
                let children = group.functions.iter().map(|function| document_symbol(
                    &state.source,
                    function.name.clone(),
                    SymbolKind::FUNCTION,
                    &function.span,
                    Some(&function.name_span),
                    Some(format!("-> {}", format_spar_type(&function.ret))),
                    Vec::new(),
                )).collect();
                out.push(document_symbol(
                    &state.source,
                    group.name.clone(),
                    SymbolKind::MODULE,
                    &group.span,
                    Some(&group.name_span),
                    Some("function group".to_string()),
                    children,
                ));
            }
            TopLevelItem::Task(task) => {
                let children = task.params.iter().map(|param| document_symbol(
                    &state.source,
                    param.name.clone(),
                    SymbolKind::VARIABLE,
                    &param.span,
                    None,
                    Some(format_spar_type(&param.ty)),
                    Vec::new(),
                )).collect();
                out.push(document_symbol(
                    &state.source,
                    task.name.clone(),
                    SymbolKind::FUNCTION,
                    &task.span,
                    Some(&task.name_span),
                    Some("task".to_string()),
                    children,
                ));
            }
            TopLevelItem::Import(_)
            | TopLevelItem::SchemaSection(_)
            | TopLevelItem::SchemaFrom(_)
            | TopLevelItem::Statement(_) => {}
        }
    }
    out
}

#[allow(deprecated)]
fn flatten_document_symbols(uri: &Url, symbols: &[DocumentSymbol]) -> Vec<SymbolInformation> {
    fn walk(uri: &Url, parent: Option<&str>, symbols: &[DocumentSymbol], out: &mut Vec<SymbolInformation>) {
        for symbol in symbols {
            out.push(SymbolInformation {
                name: symbol.name.clone(),
                kind: symbol.kind,
                tags: symbol.tags.clone(),
                deprecated: symbol.deprecated,
                location: Location { uri: uri.clone(), range: symbol.selection_range },
                container_name: parent.map(ToString::to_string),
            });
            if let Some(children) = &symbol.children {
                walk(uri, Some(&symbol.name), children, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(uri, None, symbols, &mut out);
    out
}

fn document_symbol_response(uri: &Url, state: &DocumentState, hierarchical: bool) -> DocumentSymbolResponse {
    let symbols = document_symbols(state);
    if hierarchical {
        DocumentSymbolResponse::Nested(symbols)
    } else {
        DocumentSymbolResponse::Flat(flatten_document_symbols(uri, &symbols))
    }
}

#[allow(deprecated)]
fn workspace_symbols(index: &WorkspaceIndex, query: &str) -> Vec<SymbolInformation> {
    index.search(query).into_iter().map(|symbol| SymbolInformation {
        name: symbol.name.clone(),
        kind: symbol.kind.symbol_kind(),
        tags: None,
        deprecated: None,
        location: Location { uri: symbol.uri.clone(), range: symbol.range },
        container_name: symbol.container_name.clone(),
    }).collect()
}

// ── Import/module IntelliSense ────────────────────────────────────────────────

fn resolve_import_for_editor(
    base_dir: &std::path::Path,
    path: &str,
    package: bool,
) -> Option<(PathBuf, String, SymbolTable)> {
    let options = SparLanguageServer::compile_options(base_dir);
    let mut loader = ImportLoader::new(base_dir);
    if let Some(locator) = options.locator {
        loader = loader.with_locator(locator);
    }
    let decl = spar::ast::ImportDecl {
        path: path.to_string(),
        package,
        kind: spar::ast::ImportKind::Selective(Vec::new()),
        span: Span::dummy(),
    };
    let resolved = loader.resolve_import(&decl).ok()?;
    let source = std::fs::read_to_string(&resolved.path).ok()?;
    let mut compile = CompileOptions::for_path(&resolved.path);
    compile.evaluate = false;
    compile.locator = resolved.locator;
    let compilation = Compiler::new(compile).compile(&source);
    Some((resolved.path, source, compilation.symbols?))
}

fn import_symbol_id(uri: &Url, kind: IndexedSymbolKind, name: &str, span: &Span) -> String {
    SymbolId::new(uri, kind, name, span).0
}

fn completion_data(symbol_id: String, origin: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "spar-completion",
        "symbolId": symbol_id,
        "origin": origin,
    })
}

fn declaration_documentation(source: &str, start: usize) -> Option<Documentation> {
    leading_documentation(source, start).map(|value| {
        Documentation::MarkupContent(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        })
    })
}

fn selective_import_completion_items(
    base_dir: &std::path::Path,
    path: &str,
    package: bool,
    type_only: bool,
    already: &HashSet<String>,
) -> Option<Vec<CompletionItem>> {
    let (resolved_path, source, symbols) = resolve_import_for_editor(base_dir, path, package)?;
    let uri = Url::from_file_path(&resolved_path).ok()?;
    let raw_program = raw_program_for_source(&source)?;
    let mut items = Vec::new();

    // Keep this list aligned with Spar's loader::splice_selective export
    // contract. Compiler SymbolTable contains prelude and transitively spliced
    // symbols, so it is metadata-only here; legal import candidates come from
    // declarations physically owned by the resolved source file.
    for item in &raw_program.items {
        match item {
            TopLevelItem::Var(decl) if decl.exported && !type_only => {
                if already.contains(&decl.name) { continue; }
                let detail = symbols.globals.get(&decl.name).and_then(|entry| match entry {
                    GlobalEntry::Var { ty, .. } => Some(format_spar_type(ty)),
                    _ => None,
                });
                let indexed_kind = match symbols.globals.get(&decl.name) {
                    Some(GlobalEntry::Var {
                        ty: spar::ast::SparType::Function { .. },
                        ..
                    }) => IndexedSymbolKind::Callable,
                    _ => IndexedSymbolKind::Variable,
                };
                items.push(CompletionItem {
                    label: decl.name.clone(),
                    kind: Some(if indexed_kind == IndexedSymbolKind::Callable {
                        CompletionItemKind::FUNCTION
                    } else {
                        CompletionItemKind::VARIABLE
                    }),
                    detail,
                    filter_text: Some(decl.name.clone()),
                    data: Some(completion_data(
                        import_symbol_id(&uri, indexed_kind, &decl.name, &decl.span),
                        path,
                    )),
                    documentation: declaration_documentation(&source, decl.span.start),
                    ..Default::default()
                });
            }
            TopLevelItem::Section(decl)
                if decl.exported && !decl.private && !type_only && decl.path.len() == 1 =>
            {
                let name = &decl.path[0];
                if already.contains(name) { continue; }
                let indexed_kind = if decl.canonical {
                    IndexedSymbolKind::Struct
                } else {
                    IndexedSymbolKind::Section
                };
                items.push(CompletionItem {
                    label: name.clone(),
                    kind: Some(if decl.canonical {
                        CompletionItemKind::STRUCT
                    } else {
                        CompletionItemKind::MODULE
                    }),
                    detail: Some(if decl.canonical { "struct" } else { "section" }.to_string()),
                    filter_text: Some(name.clone()),
                    data: Some(completion_data(
                        import_symbol_id(&uri, indexed_kind, name, &decl.span),
                        path,
                    )),
                    documentation: declaration_documentation(&source, decl.span.start),
                    ..Default::default()
                });
            }
            TopLevelItem::Function(decl) if !decl.is_private && !type_only => {
                if already.contains(&decl.name) { continue; }
                let (detail, span) = if let Some(entry) = symbols.functions.get(&decl.name) {
                    let signature = signature_from_entry(
                        &decl.name,
                        entry,
                        &source,
                        Some(decl),
                        Some(path.to_string()),
                    );
                    (Some(signature.label()), &entry.span)
                } else {
                    (None, &decl.span)
                };
                items.push(CompletionItem {
                    label: decl.name.clone(),
                    kind: Some(CompletionItemKind::FUNCTION),
                    detail,
                    filter_text: Some(decl.name.clone()),
                    data: Some(completion_data(
                        import_symbol_id(&uri, IndexedSymbolKind::Function, &decl.name, span),
                        path,
                    )),
                    documentation: declaration_documentation(&source, decl.span.start),
                    ..Default::default()
                });
            }
            TopLevelItem::Type(decl) if decl.exported => {
                if already.contains(&decl.name) { continue; }
                let span = symbols.types.get(&decl.name).map(|entry| &entry.span).unwrap_or(&decl.span);
                items.push(CompletionItem {
                    label: decl.name.clone(),
                    kind: Some(CompletionItemKind::CLASS),
                    detail: Some("type".to_string()),
                    filter_text: Some(decl.name.clone()),
                    data: Some(completion_data(
                        import_symbol_id(&uri, IndexedSymbolKind::Type, &decl.name, span),
                        path,
                    )),
                    documentation: declaration_documentation(&source, decl.span.start),
                    ..Default::default()
                });
            }
            TopLevelItem::Enum(decl) if decl.exported => {
                if already.contains(&decl.name) { continue; }
                let span = symbols.enums.get(&decl.name).map(|entry| &entry.span).unwrap_or(&decl.span);
                items.push(CompletionItem {
                    label: decl.name.clone(),
                    kind: Some(CompletionItemKind::ENUM),
                    detail: Some("enum".to_string()),
                    filter_text: Some(decl.name.clone()),
                    data: Some(completion_data(
                        import_symbol_id(&uri, IndexedSymbolKind::Enum, &decl.name, span),
                        path,
                    )),
                    documentation: declaration_documentation(&source, decl.span.start),
                    ..Default::default()
                });
            }
            TopLevelItem::FunctionGroup(decl) if !decl.is_private && !type_only => {
                if already.contains(&decl.name) { continue; }
                let span = symbols
                    .function_groups
                    .get(&decl.name)
                    .map(|entry| &entry.span)
                    .unwrap_or(&decl.span);
                items.push(CompletionItem {
                    label: decl.name.clone(),
                    kind: Some(CompletionItemKind::MODULE),
                    detail: Some("function group".to_string()),
                    filter_text: Some(decl.name.clone()),
                    data: Some(completion_data(
                        import_symbol_id(&uri, IndexedSymbolKind::FunctionGroup, &decl.name, span),
                        path,
                    )),
                    documentation: declaration_documentation(&source, decl.span.start),
                    ..Default::default()
                });
            }
            _ => {}
        }
    }

    items.sort_by(|a, b| a.label.cmp(&b.label));
    Some(items)
}

fn package_import_path_labels(base_dir: &std::path::Path, prefix: &str) -> Vec<String> {
    let options = SparLanguageServer::compile_options(base_dir);
    let Some(locator) = options.locator else { return Vec::new(); };
    let mut labels = Vec::new();

    let (alias_prefix, submodule_prefix) = match prefix.split_once('/') {
        Some((alias, rest)) => (alias, Some(rest)),
        None => (prefix, None),
    };

    if submodule_prefix.is_none() {
        labels.extend(
            locator
                .visible_import_aliases()
                .into_iter()
                .filter(|alias| alias.starts_with(alias_prefix)),
        );
        return labels;
    }

    let Some(alias) = locator
        .visible_import_aliases()
        .into_iter()
        .find(|alias| alias == alias_prefix)
    else {
        return labels;
    };
    let Some(entry) = locator.resolve_import(base_dir, &alias) else {
        return labels;
    };
    let Some(module_root) = entry.parent() else { return labels; };
    let submodule_prefix = submodule_prefix.unwrap_or_default();
    let slash = submodule_prefix.rfind('/');
    let (dir_prefix, leaf_prefix) = match slash {
        Some(index) => (&submodule_prefix[..=index], &submodule_prefix[index + 1..]),
        None => ("", submodule_prefix),
    };
    let search_dir = if dir_prefix.is_empty() {
        module_root.to_path_buf()
    } else {
        module_root.join(dir_prefix)
    };
    if let Ok(entries) = std::fs::read_dir(search_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else { continue; };
            if !name.starts_with(leaf_prefix) { continue; }
            if path.is_dir() {
                labels.push(format!("{alias}/{dir_prefix}{name}/"));
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("spar") {
                let stem = path.file_stem().and_then(|stem| stem.to_str()).unwrap_or(name);
                labels.push(format!("{alias}/{dir_prefix}{stem}"));
            }
        }
    }
    labels
}

fn import_path_completion_items(
    base_dir: &std::path::Path,
    prefix: &str,
    package: bool,
) -> Vec<CompletionItem> {
    let mut labels = Vec::<String>::new();
    if package {
        if prefix.is_empty() || "std".starts_with(prefix) || prefix.starts_with("std/") {
            labels.extend(
                spar::bundled_stdlib_module_names()
                    .into_iter()
                    .filter(|label| label.starts_with(prefix)),
            );
        }
        labels.extend(package_import_path_labels(base_dir, prefix));
    } else {
        let slash = prefix.rfind('/');
        let (dir_prefix, leaf_prefix) = match slash {
            Some(index) => (&prefix[..=index], &prefix[index + 1..]),
            None => ("", prefix),
        };
        let search_dir = if dir_prefix.is_empty() {
            base_dir.to_path_buf()
        } else {
            base_dir.join(dir_prefix)
        };
        if let Ok(entries) = std::fs::read_dir(search_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|name| name.to_str()) else { continue; };
                if !name.starts_with(leaf_prefix) { continue; }
                if path.is_dir() {
                    labels.push(format!("{dir_prefix}{name}/"));
                } else if path.extension().and_then(|ext| ext.to_str()) == Some("spar") {
                    labels.push(format!("{dir_prefix}{name}"));
                }
            }
        }
    }
    labels.sort();
    labels.dedup();
    labels
        .into_iter()
        .map(|label| CompletionItem {
            label: label.clone(),
            kind: Some(CompletionItemKind::MODULE),
            insert_text: Some(label),
            ..Default::default()
        })
        .collect()
}

fn enrich_completion_from_index(
    mut item: CompletionItem,
    index: &WorkspaceIndex,
    include_detail: bool,
    include_documentation: bool,
) -> CompletionItem {
    let Some(data) = item.data.as_ref() else { return item; };
    let Some(symbol_id) = data.get("symbolId").and_then(|value| value.as_str()) else { return item; };
    let Some(symbol) = index.find_by_id(symbol_id) else { return item; };
    if include_detail && item.detail.is_none() {
        item.detail = symbol.detail.clone();
    }
    if include_documentation && item.documentation.is_none() {
        let origin = symbol.container_name.clone().unwrap_or_else(|| symbol.uri.to_string());
        let mut value = symbol.documentation.clone().unwrap_or_else(|| {
            format!("**Spar {}** `{}`", symbol.kind.as_key(), symbol.name)
        });
        if !value.is_empty() {
            value.push_str("\n\n");
        }
        value.push_str(&format!("Defined in `{origin}`"));
        item.documentation = Some(Documentation::MarkupContent(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }));
    }
    item
}

/// `import pkg { | ` before any `from "..."` exists: list exports of every
/// bundled std module, and add the `from "<module>"` clause when one is picked.
fn import_export_discovery_items(
    base_dir: &std::path::Path,
    package: bool,
    type_only: bool,
    already: &HashSet<String>,
    source: &str,
    _cursor: usize,
    close_at: Option<usize>,
) -> Vec<CompletionItem> {
    if !package {
        return Vec::new();
    }
    let mut items = Vec::new();
    for module in spar::bundled_stdlib_module_names() {
        if module == "std" || module == "std/prelude" {
            continue;
        }
        let Some(module_items) =
            selective_import_completion_items(base_dir, &module, true, type_only, already)
        else {
            continue;
        };
        for mut item in module_items {
            let name = item.label.clone();
            item.detail = Some(module.clone());
            match close_at {
                Some(close) => {
                    // Names go inside the braces; the clause goes right after `}`.
                    item.insert_text = Some(name);
                    let position = byte_offset_to_lsp_position(source, close + 1);
                    item.additional_text_edits = Some(vec![TextEdit {
                        range: Range { start: position, end: position },
                        new_text: format!(" from \"{module}\""),
                    }]);
                }
                None => {
                    // No closing brace yet: complete the whole statement.
                    item.insert_text = Some(format!("{name} }} from \"{module}\";"));
                    item.additional_text_edits = None;
                }
            }
            items.push(item);
        }
    }
    items.sort_by(|a, b| a.label.cmp(&b.label).then_with(|| a.detail.cmp(&b.detail)));
    items
}

// ── Portable auto-import code actions ────────────────────────────────────────

#[derive(Debug, Clone)]
struct AutoImportCandidate {
    symbol_id: SymbolId,
    import_path: String,
    package: bool,
    type_only: bool,
    name: String,
}

fn relative_import_path(from_file: &std::path::Path, to_file: &std::path::Path) -> Option<String> {
    let from = from_file.parent()?;
    let from_components = from.components().collect::<Vec<_>>();
    let to_components = to_file.components().collect::<Vec<_>>();
    let mut common = 0usize;
    while common < from_components.len()
        && common < to_components.len()
        && from_components[common] == to_components[common]
    {
        common += 1;
    }
    let mut path = PathBuf::new();
    for _ in common..from_components.len() {
        path.push("..");
    }
    for component in &to_components[common..] {
        path.push(component.as_os_str());
    }
    let rendered = path.to_string_lossy().replace('\\', "/");
    if rendered.starts_with("../") || rendered == ".." {
        Some(rendered)
    } else {
        Some(format!("./{rendered}"))
    }
}


fn package_import_name(current_file: &std::path::Path, module_path: &std::path::Path) -> Option<String> {
    let project_dir = current_file
        .parent()?
        .ancestors()
        .find(|directory| directory.join(spar::package::PACKAGE_MANIFEST_FILE).is_file())?;
    let lockfile = spar::package::Lockfile::read(&project_dir.join(spar::package::PACKAGE_LOCK_FILE)).ok()?;
    let store = spar::package::PackageStore::new(spar::package::StorePaths::from_env());
    let locator = spar::package::ModuleLocator::for_root(lockfile, store);
    let target = module_path.canonicalize().unwrap_or_else(|_| module_path.to_path_buf());

    for alias in locator.visible_import_aliases() {
        let Some(entry) = locator.resolve_import(project_dir, &alias) else { continue; };
        let entry = entry.canonicalize().unwrap_or(entry);
        if entry == target {
            return Some(alias);
        }
        let Some(root) = entry.parent() else { continue; };
        let Ok(relative) = target.strip_prefix(root) else { continue; };
        if relative.extension().and_then(|ext| ext.to_str()) != Some("spar") { continue; }
        let mut relative = relative.to_path_buf();
        relative.set_extension("");
        let suffix = relative.to_string_lossy().replace('\\', "/");
        if !suffix.is_empty() {
            return Some(format!("{alias}/{suffix}"));
        }
    }
    None
}

fn stdlib_import_name(path: &std::path::Path) -> Option<String> {
    let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    spar::bundled_stdlib_module_names().into_iter().find(|module| {
        spar::resolve_bundled_stdlib_import(module)
            .map(|candidate| candidate.canonicalize().unwrap_or(candidate) == target)
            .unwrap_or(false)
    })
}

fn auto_import_candidates(
    index: &WorkspaceIndex,
    unresolved_name: &str,
    current_uri: &Url,
    type_context: bool,
) -> Vec<AutoImportCandidate> {
    let current_path = current_uri.to_file_path().ok();
    let mut candidates = index
        .find_by_name(unresolved_name)
        .into_iter()
        .filter(|symbol| symbol.exported && !symbol.private && symbol.uri != *current_uri)
        .filter(|symbol| if type_context { symbol.kind.is_type() } else { !symbol.kind.is_type() })
        .filter_map(|symbol| {
            let module_path = symbol.uri.to_file_path().ok()?;
            let (import_path, package) = if let Some(std_name) = stdlib_import_name(&module_path) {
                (std_name, true)
            } else if let Some(package_name) = package_import_name(current_path.as_deref()?, &module_path) {
                (package_name, true)
            } else {
                (relative_import_path(current_path.as_deref()?, &module_path)?, false)
            };
            Some(AutoImportCandidate {
                symbol_id: symbol.id.clone(),
                import_path,
                package,
                type_only: symbol.kind.is_type(),
                name: symbol.name.clone(),
            })
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| a.import_path.cmp(&b.import_path).then_with(|| a.name.cmp(&b.name)));
    candidates.dedup_by(|a, b| a.symbol_id == b.symbol_id);
    candidates
}

fn insertion_header_offset(source: &str) -> usize {
    let mut offset = 0usize;
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("#!") || trimmed.starts_with("@SchemaFile") || trimmed.starts_with("@LoadEnv") || trimmed.is_empty() {
            offset += line.len();
        } else {
            break;
        }
    }
    offset
}

fn zero_range_at(source: &str, byte: usize) -> Range {
    let pos = byte_offset_to_lsp_position(source, byte);
    Range { start: pos, end: pos }
}

fn build_import_workspace_edit(source: &str, uri: &Url, candidate: &AutoImportCandidate) -> WorkspaceEdit {
    let raw = raw_program_for_source(source);
    if let Some(program) = &raw {
        for item in &program.items {
            let TopLevelItem::Import(import) = item else { continue; };
            if import.path != candidate.import_path || import.package != candidate.package { continue; }
            let compatible = matches!(
                (&import.kind, candidate.type_only),
                (spar::ast::ImportKind::Selective(_), false)
                    | (spar::ast::ImportKind::TypeSelective(_), true)
            );
            if !compatible { continue; }
            let items = match &import.kind {
                spar::ast::ImportKind::Selective(items) | spar::ast::ImportKind::TypeSelective(items) => items,
                _ => continue,
            };
            if items.iter().any(|item| item.name == candidate.name || item.alias.as_deref() == Some(candidate.name.as_str())) {
                return WorkspaceEdit::default();
            }
            let (_, statement_end) = statement_bounds(source, import.span.start);
            let body = &source[import.span.start.min(source.len())..statement_end.min(source.len())];
            if let Some(close_rel) = body.rfind('}') {
                let close = import.span.start + close_rel;
                let prefix = if items.is_empty() { "" } else { ", " };
                let edit = TextEdit {
                    range: zero_range_at(source, close),
                    new_text: format!("{prefix}{}", candidate.name),
                };
                return WorkspaceEdit {
                    changes: Some(HashMap::from([(uri.clone(), vec![edit])])),
                    document_changes: None,
                    change_annotations: None,
                };
            }
        }
    }

    let mut insert_at = insertion_header_offset(source);
    if let Some(program) = &raw {
        for item in &program.items {
            if let TopLevelItem::Import(import) = item {
                let (_, end) = statement_bounds(source, import.span.start);
                insert_at = insert_at.max(end);
            }
        }
    }
    let pkg = if candidate.package { " pkg" } else { "" };
    let ty = if candidate.type_only { " type" } else { "" };
    let mut line = format!("import{pkg}{ty} {{ {} }} from \"{}\";\n", candidate.name, candidate.import_path);
    if insert_at > 0 && !source[..insert_at].ends_with('\n') {
        line.insert(0, '\n');
    }
    WorkspaceEdit {
        changes: Some(HashMap::from([(
            uri.clone(),
            vec![TextEdit { range: zero_range_at(source, insert_at), new_text: line }],
        )])),
        document_changes: None,
        change_annotations: None,
    }
}


fn diagnostic_is_unresolved(diagnostic: &Diagnostic) -> bool {
    let message = diagnostic.message.to_ascii_lowercase();
    message.contains("undefined function")
        || message.contains("undefined reference")
        || message.contains("undefined type")
}

fn code_actions_for_unresolved(
    state: &DocumentState,
    index: &WorkspaceIndex,
    uri: &Url,
    range: Range,
    diagnostics: &[Diagnostic],
) -> Vec<CodeActionOrCommand> {
    let relevant = diagnostics.iter().any(|diagnostic| {
        diagnostic_is_unresolved(diagnostic)
            && diagnostic.range.start <= range.end
            && range.start <= diagnostic.range.end
    });
    if !relevant { return Vec::new(); }
    let name = word_at_position(&state.source, range.start);
    if name.is_empty() { return Vec::new(); }
    let type_context = is_in_type_position(&state.source, range.start);
    let candidates = auto_import_candidates(index, &name, uri, type_context);
    let ambiguous = candidates.len() > 1;
    candidates.into_iter().map(|candidate| {
        let edit = build_import_workspace_edit(&state.source, uri, &candidate);
        CodeActionOrCommand::CodeAction(CodeAction {
            title: format!("Import `{}` from `{}`", candidate.name, candidate.import_path),
            kind: Some(CodeActionKind::QUICKFIX),
            diagnostics: None,
            edit: Some(edit),
            command: None,
            is_preferred: Some(!ambiguous),
            disabled: None,
            data: Some(serde_json::json!({
                "kind": "spar-auto-import",
                "symbolId": candidate.symbol_id.0,
                "origin": candidate.import_path,
            })),
        })
    }).collect()
}

/// Quick-fix for the removed `task [Name]` syntax: rewrites it to `task Name`.
fn task_bracket_quick_fixes(
    state: &DocumentState,
    uri: &Url,
    diagnostics: &[Diagnostic],
) -> Vec<CodeActionOrCommand> {
    diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("task [Name] is removed"))
        .filter_map(|diagnostic| {
            let line_number = diagnostic.range.start.line;
            let line = state.source.lines().nth(line_number as usize)?;
            let task_index = line.find("task")?;
            let open = line[task_index..].find('[')? + task_index;
            let close = line[open..].find(']')? + open;
            let name = line[open + 1..close].trim();
            if name.is_empty() {
                return None;
            }
            let column = |byte: usize| line[..byte].encode_utf16().count() as u32;
            let edit = TextEdit {
                range: Range {
                    start: Position { line: line_number, character: column(open) },
                    end: Position { line: line_number, character: column(close + 1) },
                },
                new_text: name.to_string(),
            };
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), vec![edit]);
            Some(CodeActionOrCommand::CodeAction(CodeAction {
                title: format!("Remove brackets: task {name}"),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![diagnostic.clone()]),
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    document_changes: None,
                    change_annotations: None,
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }))
        })
        .collect()
}

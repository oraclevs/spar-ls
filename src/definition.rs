fn span_location(uri: Url, source: &str, span: &Span) -> Location {
    let (line, col) = byte_to_lsp_pos(source, span.start);
    let length = span.end.saturating_sub(span.start).max(1) as u32;
    Location::new(uri, Range::new(Position::new(line, col), Position::new(line, col + length)))
}

fn imported_uri(state: &DocumentState, alias: &str) -> Option<Url> {
    Url::from_file_path(state.import_paths.get(alias)?).ok()
}

fn symbol_span(symbols: &SymbolTable, path: &[String], word: &str) -> Option<Span> {
    if path.is_empty() {
        if let Some(entry) = symbols.globals.get(word) {
            return Some(match entry { GlobalEntry::Var { span, .. } | GlobalEntry::Dynamic { span, .. } => span.clone() });
        }
        if let Some(entry) = symbols.functions.get(word) { return Some(entry.span.clone()); }
        if let Some(entry) = symbols.types.get(word) { return Some(entry.span.clone()); }
        if let Some(entry) = symbols.enums.get(word) { return Some(entry.span.clone()); }
        if let Some(entry) = symbols.function_groups.get(word) { return Some(entry.span.clone()); }
        if let Some(entry) = symbols.sections.get(&vec![word.to_string()]) { return Some(entry.span.clone()); }
    }
    // `Group::member` — jumps to the member function's own span (exact).
    // `EnumName::Variant` — no per-variant span exists in the AST, so this
    // jumps to the enum declaration itself rather than the specific variant.
    if path.len() == 1 {
        if let Some(entry) = symbols.function_groups.get(&path[0]) {
            if let Some(member) = entry.functions.get(word) { return Some(member.span.clone()); }
        }
        if let Some(entry) = symbols.enums.get(&path[0]) {
            if entry.variants.iter().any(|v| v == word) { return Some(entry.span.clone()); }
        }
    }
    let section_path = if path.is_empty() { vec![] } else { path.to_vec() };
    if let Some(entry) = symbols.sections.get(&section_path) {
        if let Some(field) = entry.fields.get(word) { return Some(field.span.clone()); }
    }
    let mut complete = section_path;
    complete.push(word.to_string());
    symbols.sections.get(&complete).map(|e| e.span.clone())
}

fn local_decl_span(program: &Program, offset: usize, word: &str) -> Option<Span> {
    fn walk(stmts: &[FuncStmt], offset: usize, word: &str, found: &mut Option<Span>) {
        for stmt in stmts {
            match stmt {
                FuncStmt::LocalVar(v) => {
                    if v.span.start <= offset && v.name == word { *found = Some(v.span.clone()); }
                }
                FuncStmt::If(i) => { walk(&i.then_stmts, offset, word, found); walk(&i.else_stmts, offset, word, found); }
                FuncStmt::For(statement) => {
                    match &statement.binding {
                        spar::ast::ForBinding::Value { name, span } => {
                            if span.start <= offset && name == word { *found = Some(span.clone()); }
                        }
                        spar::ast::ForBinding::Indexed { index_name, index_span, value_name, value_span } => {
                            if index_span.start <= offset && index_name == word { *found = Some(index_span.clone()); }
                            if value_span.start <= offset && value_name == word { *found = Some(value_span.clone()); }
                        }
                    }
                    walk(&statement.body, offset, word, found);
                }
                FuncStmt::Return(_, _)
                | FuncStmt::Assignment { .. }
                | FuncStmt::Expression(_, _)
                | FuncStmt::Break(_)
                | FuncStmt::Continue(_)
                | FuncStmt::Try(_) => {}
            }
        }
    }
    for item in &program.items {
        let TopLevelItem::Function(f) = item else { continue };
        if !(f.span.start <= offset && offset <= f.span.end) { continue; }
        if let Some(p) = f.params.iter().find(|p| p.name == word) { return Some(p.span.clone()); }
        let mut found = None;
        walk(&f.body.stmts, offset, word, &mut found);
        if found.is_some() { return found; }
    }
    None
}

/// Tasks aren't resolved into `SymbolTable` (unlike globals/functions/
/// types/enums/sections), so this scans the AST directly — same reason
/// `local_decl_span` doesn't go through `SymbolTable` either.
fn task_decl_span(program: &Program, word: &str) -> Option<Span> {
    program.items.iter().find_map(|item| {
        let TopLevelItem::Task(task) = item else { return None };
        (task.name == word).then(|| task.name_span.clone())
    })
}

/// If `word` is a top-level name brought in by a `Selective`/
/// `TypeSelective` import, resolves it against that import
/// target's own freshly-parsed `SymbolTable` (`state.spliced_import_symbols`)
/// instead of the local (possibly retagged, see `spar::loader::
/// retag_top_level_span`) span baked into the compiled/spliced AST — so
/// go-to-definition lands in the file the symbol actually came from, at its
/// real line, rather than on the local `import { ... }` statement.
///
/// Known residual gap: an aliased selective request (`import { foo as bar }
/// from "x.spar";`) is looked up here by its *original* name (`foo`) against
/// the target file's own symbol table — correct — but this function itself
/// is only reached for the word actually under the cursor, so hovering the
/// local alias `bar` elsewhere in the importing file still resolves through
/// it correctly; what's NOT attempted is fixing up spans nested inside a
/// spliced item (e.g. an individual field of a spliced `[Section]`) — those
/// still carry whatever span the retag/splice step left them with.
fn spliced_definition(state: &DocumentState, _current: &Url, word: &str) -> Option<Location> {
    use spar::ast::ImportKind;
    for decl in &state.spliced_import_decls {
        let original_name = match &decl.kind {
            ImportKind::Selective(items) | ImportKind::TypeSelective(items) => items
                .iter()
                .find(|item| item.alias.as_deref().unwrap_or(item.name.as_str()) == word)
                .map(|item| item.name.clone()),
            ImportKind::Aliased(_) | ImportKind::Schema => None,
        };
        let Some(original_name) = original_name else {
            continue;
        };
        let Some(target_symbols) = state.spliced_import_symbols.get(&decl.path) else {
            continue;
        };
        let Some(span) = symbol_span(target_symbols, &[], &original_name) else {
            continue;
        };
        let Some(target_path) = state.spliced_import_paths.get(&decl.path) else {
            continue;
        };
        let Ok(target_uri) = Url::from_file_path(target_path) else {
            continue;
        };
        let Ok(target_source) = std::fs::read_to_string(target_path) else {
            continue;
        };
        return Some(span_location(target_uri, &target_source, &span));
    }
    None
}

fn definition_at(uri: &Url, state: &DocumentState, pos: Position) -> Option<Location> {
    let word = word_at_position(&state.source, pos);
    if word.is_empty() { return None; }
    let program = state.ast.as_ref()?;
    let offset = lsp_pos_to_byte_offset(&state.source, pos);
    if let Some(span) = local_decl_span(program, offset, &word) {
        return Some(span_location(uri.clone(), &state.source, &span));
    }
    if let Some(span) = task_decl_span(program, &word) {
        return Some(span_location(uri.clone(), &state.source, &span));
    }
    let prefix = path_prefix_before_word(&state.source, pos).unwrap_or_default();
    if let Some(alias) = prefix.first() {
        if let Some(imported) = state.effective_import_symbols().get(alias) {
            let target_uri = imported_uri(state, alias)?;
            let target_source = std::fs::read_to_string(target_uri.to_file_path().ok()?).ok()?;
            let span = symbol_span(imported, &prefix[1..], &word)?;
            return Some(span_location(target_uri, &target_source, &span));
        }
    }
    if prefix.is_empty() {
        if let Some(location) = spliced_definition(state, uri, &word) {
            return Some(location);
        }
    }
    let symbols = state.effective_symbols()?;
    let span = symbol_span(symbols, &prefix, &word).or_else(|| {
        symbols.sections.values().find_map(|s| s.fields.get(&word).map(|f| f.span.clone()))
    })?;
    Some(span_location(uri.clone(), &state.source, &span))
}

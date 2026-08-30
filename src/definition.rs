fn span_location(uri: Url, source: &str, span: &Span) -> Location {
    let (line, col) = byte_to_lsp_pos(source, span.start);
    let length = span.end.saturating_sub(span.start).max(1) as u32;
    Location::new(uri, Range::new(Position::new(line, col), Position::new(line, col + length)))
}

fn imported_uri(program: &Program, current: &Url, alias: &str) -> Option<Url> {
    use spar::ast::ImportKind;
    let base = current.to_file_path().ok()?.parent()?.to_path_buf();
    program.items.iter().find_map(|item| {
        let TopLevelItem::Import(decl) = item else { return None };
        let ImportKind::Aliased(explicit) = &decl.kind else { return None };
        let derived = std::path::Path::new(&decl.path).file_stem()?.to_str()?;
        if explicit.as_deref().unwrap_or(derived) == alias {
            Url::from_file_path(base.join(&decl.path)).ok()
        } else { None }
    })
}

fn symbol_span(symbols: &SymbolTable, path: &[String], word: &str) -> Option<Span> {
    if path.is_empty() {
        if let Some(entry) = symbols.globals.get(word) {
            return Some(match entry { GlobalEntry::Var { span, .. } | GlobalEntry::Dynamic { span, .. } => span.clone() });
        }
        if let Some(entry) = symbols.functions.get(word) { return Some(entry.span.clone()); }
        if let Some(entry) = symbols.types.get(word) { return Some(entry.span.clone()); }
        if let Some(entry) = symbols.enums.get(word) { return Some(entry.span.clone()); }
        if let Some(entry) = symbols.sections.get(&vec![word.to_string()]) { return Some(entry.span.clone()); }
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
                FuncStmt::For { var_name, body, span, .. } => {
                    if span.start <= offset && var_name == word { *found = Some(span.clone()); }
                    walk(body, offset, word, found);
                }
                FuncStmt::Return(_, _) => {}
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
            let target_uri = imported_uri(program, uri, alias)?;
            let target_source = std::fs::read_to_string(target_uri.to_file_path().ok()?).ok()?;
            let span = symbol_span(imported, &prefix[1..], &word)?;
            return Some(span_location(target_uri, &target_source, &span));
        }
    }
    let symbols = state.effective_symbols()?;
    let span = symbol_span(symbols, &prefix, &word).or_else(|| {
        symbols.sections.values().find_map(|s| s.fields.get(&word).map(|f| f.span.clone()))
    })?;
    Some(span_location(uri.clone(), &state.source, &span))
}

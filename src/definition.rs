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

fn local_decl_span(program: &Program, source: &str, offset: usize, word: &str) -> Option<Span> {
    fn walk(stmts: &[FuncStmt], source: &str, offset: usize, word: &str, found: &mut Option<Span>) {
        for stmt in stmts {
            match stmt {
                FuncStmt::LocalVar(v) => {
                    if v.span.start <= offset && v.name == word {
                        *found = Some(ident_span_in(source, &v.span, word).unwrap_or_else(|| v.span.clone()));
                    }
                }
                FuncStmt::If(i) => { walk(&i.then_stmts, source, offset, word, found); walk(&i.else_stmts, source, offset, word, found); }
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
                    walk(&statement.body, source, offset, word, found);
                }
                FuncStmt::Return(_, _)
                | FuncStmt::Assignment { .. }
                | FuncStmt::FieldAssignment { .. }
                | FuncStmt::Expression(_, _)
                | FuncStmt::Break(_)
                | FuncStmt::Continue(_)
                | FuncStmt::Try(_) => {}
            }
        }
    }
    // `FunctionDecl::span` covers only the `function` keyword; the body's span is its
    // closing brace, so the real extent runs from the keyword to the body's end.
    let check = |f: &spar::ast::FunctionDecl| -> Option<Span> {
        let end = f.body.span.end.max(f.span.end);
        if !(f.span.start <= offset && offset <= end) { return None; }
        if let Some(p) = f.params.iter().find(|p| p.name == word) {
            // the parameter's span may start at its type; prefer the name itself
            return Some(ident_span_in(source, &p.span, word).unwrap_or_else(|| p.span.clone()));
        }
        let mut found = None;
        walk(&f.body.stmts, source, offset, word, &mut found);
        found
    };
    for item in &program.items {
        match item {
            TopLevelItem::Function(f) => {
                if let Some(span) = check(f) { return Some(span); }
            }
            TopLevelItem::FunctionGroup(group) => {
                for f in &group.functions {
                    if let Some(span) = check(f) { return Some(span); }
                }
            }
            TopLevelItem::Impl(imp) => {
                for method in &imp.methods {
                    if let Some(span) = check(&method.function) { return Some(span); }
                }
            }
            _ => {}
        }
    }
    None
}

/// The span of the whole-word `name` at or after the start of `span` (declaration
/// spans often begin at a keyword, so this finds the identifier itself).
fn ident_span_in(source: &str, span: &Span, name: &str) -> Option<Span> {
    let byte = find_ident_byte(source, span.start.min(source.len()), name)?;
    let (line, col) = byte_to_lsp_pos(source, byte);
    Some(Span::new(byte, byte + name.len(), line + 1, col + 1))
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


fn position_in_range(pos: Position, range: &Range) -> bool {
    range.start <= pos && pos <= range.end
}

fn method_callee_at_position(source: &str, pos: Position) -> Option<(String, usize)> {
    let offset = lsp_pos_to_byte_offset(source, pos).min(source.len());
    let bytes = source.as_bytes();
    let is_ident = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';

    let mut start = offset;
    while start > 0 && is_ident(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = offset;
    while end < bytes.len() && is_ident(bytes[end]) {
        end += 1;
    }
    if start == end {
        return None;
    }

    let method = source.get(start..end)?;
    let mut dot = start;
    while dot > 0 && bytes[dot - 1].is_ascii_whitespace() {
        dot -= 1;
    }
    if dot == 0 || bytes[dot - 1] != b'.' {
        return None;
    }

    let mut receiver_start = dot - 1;
    while receiver_start > 0 {
        let byte = bytes[receiver_start - 1];
        if is_ident(byte) || byte == b'.' {
            receiver_start -= 1;
        } else {
            break;
        }
    }
    let receiver = source.get(receiver_start..dot - 1)?.trim();
    if receiver.is_empty() {
        return None;
    }
    Some((format!("{receiver}.{method}"), start))
}

fn method_symbol_at_position<'a>(
    uri: &Url,
    state: &DocumentState,
    pos: Position,
    index: &'a WorkspaceIndex,
) -> Option<&'a IndexedSymbol> {
    let word = word_at_position(&state.source, pos);
    if word.is_empty() {
        return None;
    }

    if let Some(symbol) = index.symbols_for_uri(uri).iter().find(|symbol| {
        symbol.kind == IndexedSymbolKind::Method
            && symbol.name == word
            && position_in_range(pos, &symbol.selection_range)
    }) {
        return Some(symbol);
    }

    let (callee, offset) = method_callee_at_position(&state.source, pos)?;
    resolve_method_symbol(
        state,
        index,
        uri,
        &state.source,
        offset,
        &callee,
    )
}

fn indexed_definition_at(
    uri: &Url,
    state: &DocumentState,
    pos: Position,
    index: &WorkspaceIndex,
) -> Option<Location> {
    if let Some(symbol) = method_symbol_at_position(uri, state, pos, index) {
        return Some(Location {
            uri: symbol.uri.clone(),
            range: symbol.selection_range,
        });
    }
    definition_at(uri, state, pos)
}

fn definition_at(uri: &Url, state: &DocumentState, pos: Position) -> Option<Location> {
    let word = word_at_position(&state.source, pos);
    if word.is_empty() { return None; }
    let program = state.ast.as_ref()?;
    let offset = lsp_pos_to_byte_offset(&state.source, pos);
    if let Some(binding) = local_binding_at_offset(state, &word, offset) {
        let range = Range {
            start: byte_offset_to_lsp_position(&state.source, binding.decl_start),
            end: byte_offset_to_lsp_position(&state.source, binding.decl_end),
        };
        return Some(Location { uri: uri.clone(), range });
    }
    if let Some(span) = local_decl_span(program, &state.source, offset, &word) {
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
    // Declaration spans often start at a keyword (`var`, `type`); point at the name.
    let span = ident_span_in(&state.source, &span, &word).unwrap_or(span);
    Some(span_location(uri.clone(), &state.source, &span))
}

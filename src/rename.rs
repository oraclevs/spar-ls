// ── Portable semantic rename ─────────────────────────────────────────────────

fn is_valid_rename(symbol: Option<&IndexedSymbol>, new_name: &str) -> bool {
    if new_name.is_empty() || !new_name.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
        return false;
    }
    if let Some(symbol) = symbol {
        if matches!(
            symbol.kind,
            IndexedSymbolKind::Struct | IndexedSymbolKind::Type | IndexedSymbolKind::Enum
        ) {
            return spar::naming::is_pascal_case(new_name);
        }
    }
    spar::naming::is_camel_case(new_name) || new_name.chars().all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
}

fn declaration_is_editable(target: &SemanticTarget, workspace_root: Option<&std::path::Path>) -> bool {
    let Ok(path) = target.declaration.uri.to_file_path() else { return false; };
    if spar::is_bundled_stdlib_path(&path) { return false; }
    workspace_root.is_none_or(|root| path.starts_with(root))
}

fn prepare_rename_at(
    uri: &Url,
    state: &DocumentState,
    pos: Position,
    index: &WorkspaceIndex,
    workspace_root: Option<&std::path::Path>,
) -> Option<PrepareRenameResponse> {
    let target = semantic_target_at(uri, state, pos, index)?;
    if !declaration_is_editable(&target, workspace_root) { return None; }
    let current_range = semantic_identifier_occurrences(state, &target.name)
        .into_iter()
        .find(|(_, range)| range.start <= pos && pos <= range.end)
        .map(|(_, range)| range)
        .unwrap_or(target.declaration.range);
    Some(PrepareRenameResponse::RangeWithPlaceholder {
        range: current_range,
        placeholder: target.name,
    })
}

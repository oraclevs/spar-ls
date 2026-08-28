// ── Completion builders ───────────────────────────────────────────────────────

fn keyword_items() -> Vec<CompletionItem> {
    [
        // declaration keywords
        "var", "export", "private", "import", "dynamic", "as", "section", "function",
        // control keywords
        "if", "else", "for", "in", "return", // literals
        "true", "false",
    ]
    .iter()
    .map(|kw| CompletionItem {
        label: kw.to_string(),
        kind: Some(CompletionItemKind::KEYWORD),
        ..Default::default()
    })
    .collect()
}

fn type_keyword_items() -> Vec<CompletionItem> {
    ["int", "float", "str", "bool", "section"]
        .iter()
        .map(|kw| CompletionItem {
            label: kw.to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            ..Default::default()
        })
        .collect()
}

fn builtin_items() -> Vec<CompletionItem> {
    [
        ("env", "(name: str) -> str"),
        ("int", "(value: int | float | str) -> int"),
        ("float", "(value: int | float | str) -> float"),
        ("str", "(value: int | float | bool | str) -> str"),
        ("bool", "(value: str | bool) -> bool"),
    ]
    .iter()
    .map(|(name, sig)| CompletionItem {
        label: name.to_string(),
        kind: Some(CompletionItemKind::FUNCTION),
        detail: Some(sig.to_string()),
        ..Default::default()
    })
    .collect()
}

fn function_completion_items(functions: &HashMap<String, FunctionEntry>) -> Vec<CompletionItem> {
    functions
        .iter()
        .map(|(name, entry)| {
            let param_list = entry
                .params
                .iter()
                .map(|(pname, pty)| format!("{}: {}", pname, format_spar_type(pty)))
                .collect::<Vec<_>>()
                .join(", ");
            CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some(format!(
                    "({}) -> {}",
                    param_list,
                    format_spar_type(&entry.ret)
                )),
                insert_text: Some(format!("{}($1)", name)),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            }
        })
        .collect()
}

fn section_field_completions(
    symbols: &SymbolTable,
    path: &[String],
    section: &SectionEntry,
) -> Vec<CompletionItem> {
    section
        .fields
        .iter()
        .map(|(name, entry)| {
            if entry.ty == Some(SparType::Section) {
                CompletionItem {
                    label: name.clone(),
                    kind: Some(CompletionItemKind::MODULE),
                    detail: Some("section".to_string()),
                    insert_text: Some(format!("{}::", name)),
                    ..Default::default()
                }
            } else {
                CompletionItem {
                    label: name.clone(),
                    kind: Some(CompletionItemKind::FIELD),
                    detail: Some(
                        entry
                            .ty
                            .as_ref()
                            .map(format_spar_type)
                            .unwrap_or_else(|| resolve_field_type_display(symbols, path, name)),
                    ),
                    ..Default::default()
                }
            }
        })
        .collect()
}


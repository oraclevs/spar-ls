// ── Callable signature help + named-argument completion ──────────────────────

fn signature_from_resolved_entry(
    name: &str,
    entry: &FunctionEntry,
    origin: Option<String>,
) -> CallableSignature {
    CallableSignature {
        name: name.to_string(),
        params: entry
            .params
            .iter()
            .map(|(param_name, ty)| CallableParam {
                name: param_name.clone(),
                ty: format_spar_type(ty),
                has_default: entry.default_params.contains(param_name),
                default_repr: None,
            })
            .collect(),
        return_type: format_spar_type(&entry.ret),
        is_async: entry.is_async,
        origin,
        argument_style: CallableArgumentStyle::Named,
    }
}


fn simple_receiver_chain(text: &str) -> Option<Chain> {
    let mut parts = text.split('.');
    let root = parts.next()?.trim();
    if root.is_empty() || !root.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
        return None;
    }
    let mut steps = Vec::new();
    for part in parts {
        let part = part.trim();
        if part.is_empty() || !part.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
            return None;
        }
        steps.push(ChainStep::Field(part.to_string()));
    }
    Some(Chain { root: root.to_string(), steps })
}

fn resolve_method_symbol<'a>(
    state: &DocumentState,
    index: &'a WorkspaceIndex,
    uri: &Url,
    source: &str,
    offset: usize,
    callee: &str,
) -> Option<&'a IndexedSymbol> {
    let (receiver_text, method_name) = callee.rsplit_once('.')?;
    let symbols = state.effective_symbols()?;
    let static_receiver = if !receiver_text.contains('.') {
        let path = vec![receiver_text.to_string()];
        symbols.sections.get(&path).is_some_and(|section| section.canonical)
    } else {
        false
    };
    let owner = if static_receiver {
        receiver_text.to_string()
    } else {
        let chain = simple_receiver_chain(receiver_text)?;
        let scope = local_names_at(source, offset);
        let ty = type_of_chain(&chain, &scope, symbols, 0)?;
        owner_name_for_type(&ty)?.to_string()
    };
    index
        .visible_methods_for_owner(uri, &owner)
        .into_iter()
        .find(|symbol| {
            symbol.name == method_name
                && matches!(
                    (static_receiver, symbol.method_receiver),
                    (true, Some(IndexedMethodReceiver::Static))
                        | (false, Some(IndexedMethodReceiver::Shared | IndexedMethodReceiver::Mutable))
                )
        })
}

fn resolve_method_signature(
    state: &DocumentState,
    index: &WorkspaceIndex,
    uri: &Url,
    source: &str,
    offset: usize,
    callee: &str,
) -> Option<CallableSignature> {
    resolve_method_symbol(state, index, uri, source, offset, callee)
        .and_then(|symbol| symbol.signature.clone())
}

fn resolve_callable_named(
    state: &DocumentState,
    index: &WorkspaceIndex,
    uri: &Url,
    source: &str,
    offset: usize,
    callee: &str,
) -> Option<CallableSignature> {
    if callee.contains('.') {
        return resolve_method_signature(state, index, uri, source, offset, callee);
    }

    if !callee.contains("::") {
        if let Some(symbols) = state.effective_symbols() {
            let path = vec![callee.to_string()];
            if symbols.sections.get(&path).is_some_and(|section| section.canonical) {
                if let Some(signature) = index
                    .visible_constructor_for_owner(uri, callee)
                    .and_then(|symbol| symbol.signature.clone())
                {
                    return Some(signature);
                }
            }
        }
        let callable_in_scope = state
            .effective_symbols()
            .and_then(|symbols| symbols.globals.get(callee))
            .is_some_and(|entry| matches!(
                entry,
                GlobalEntry::Var { ty: SparType::Function { .. }, .. }
            ));
        if callable_in_scope {
            if let Some(signature) = index
                .visible_callable_named(uri, callee)
                .and_then(|symbol| symbol.signature.clone())
            {
                return Some(signature);
            }
        }
    }

    let segments = callee.split("::").collect::<Vec<_>>();

    // Aliased import call: `module::function(...)`.
    if segments.len() == 2 {
        let qualifier = segments[0];
        let member = segments[1];
        if let Some(imported) = state.effective_import_symbols().get(qualifier) {
            if let Some(entry) = imported.functions.get(member) {
                return Some(signature_from_resolved_entry(
                    member,
                    entry,
                    Some(qualifier.to_string()),
                ));
            }
        }

        // Local function-group call: `Group::member(...)`.
        if let Some(symbols) = state.effective_symbols() {
            if let Some(group) = symbols.function_groups.get(qualifier) {
                if let Some(entry) = group.functions.get(member) {
                    return Some(signature_from_resolved_entry(
                        member,
                        entry,
                        Some(qualifier.to_string()),
                    ));
                }
            }
        }
    }

    // Cross-file function-group call: `module::Group::member(...)`.
    if segments.len() == 3 {
        let import_alias = segments[0];
        let group_name = segments[1];
        let member = segments[2];
        if let Some(imported) = state.effective_import_symbols().get(import_alias) {
            if let Some(group) = imported.function_groups.get(group_name) {
                if let Some(entry) = group.functions.get(member) {
                    return Some(signature_from_resolved_entry(
                        member,
                        entry,
                        Some(format!("{import_alias}::{group_name}")),
                    ));
                }
            }
        }
    }

    if let Some(symbols) = state.effective_symbols() {
        if let Some(entry) = symbols.functions.get(callee).or_else(|| symbols.imported_functions.get(callee)) {
            if let Some(indexed) = index
                .symbols_for_uri(uri)
                .iter()
                .find(|symbol| symbol.name == callee && symbol.signature.is_some())
            {
                if let Some(signature) = &indexed.signature {
                    return Some(signature.clone());
                }
            }
            return Some(signature_from_resolved_entry(callee, entry, None));
        }
        if let Some(task) = symbols.tasks.get(callee) {
            return Some(CallableSignature {
                name: callee.to_string(),
                params: task.params.iter().map(|(name, ty)| CallableParam {
                    name: name.clone(),
                    ty: format_spar_type(ty),
                    has_default: false,
                    default_repr: None,
                }).collect(),
                return_type: "task".to_string(),
                is_async: false,
                origin: None,
                argument_style: CallableArgumentStyle::Named,
            });
        }
    }
    None
}

fn signature_help_at(
    state: &DocumentState,
    index: &WorkspaceIndex,
    uri: &Url,
    source: &str,
    offset: usize,
) -> Option<SignatureHelp> {
    let EditorContext::CallArguments { callee, supplied, active_parameter, value_of } = editor_context(source, state.ast.as_ref(), offset) else {
        return None;
    };
    let signature = resolve_callable_named(state, index, uri, source, offset, &callee)?;
    let mut active = active_parameter as usize;
    if active >= signature.params.len() {
        active = signature.params.len().saturating_sub(1);
    }
    // Named arguments can arrive out of positional order. Prefer the first
    // still-missing parameter after a comma when possible.
    if !supplied.is_empty() && active_parameter as usize >= supplied.len() {
        if let Some((index, _)) = signature.params.iter().enumerate().find(|(_, param)| !supplied.contains(&param.name)) {
            active = index;
        }
    }
    // A named argument in progress (`b: |`) pins the active parameter by name.
    if let Some(name) = &value_of {
        if let Some((position, _)) = signature.params.iter().enumerate().find(|(_, param)| &param.name == name) {
            active = position;
        }
    }
    let parameters = signature.params.iter().map(|param| {
        let mut label = format!("{}: {}", param.name, param.ty);
        if param.has_default {
            label.push_str(" = ");
            label.push_str(param.default_repr.as_deref().unwrap_or("…"));
        }
        ParameterInformation {
            label: ParameterLabel::Simple(label),
            documentation: None,
        }
    }).collect();
    Some(SignatureHelp {
        signatures: vec![SignatureInformation {
            label: signature.label(),
            documentation: signature.origin.as_ref().map(|origin| Documentation::String(format!("Defined in {origin}"))),
            parameters: Some(parameters),
            active_parameter: Some(active as u32),
        }],
        active_signature: Some(0),
        active_parameter: Some(active as u32),
    })
}

fn named_parameter_items(signature: &CallableSignature, supplied: &[String]) -> Vec<CompletionItem> {
    signature
        .params
        .iter()
        .enumerate()
        .filter(|(_, param)| !supplied.contains(&param.name))
        .map(|(position, param)| CompletionItem {
            label: format!("{}:", param.name),
            kind: Some(CompletionItemKind::FIELD),
            detail: Some(if param.has_default {
                format!("{} = {}", param.ty, param.default_repr.as_deref().unwrap_or("…"))
            } else {
                param.ty.clone()
            }),
            insert_text: Some(format!("{}: ", param.name)),
            // Required parameters first, then optional, each in declared order.
            sort_text: Some(format!(
                "{}_{:03}",
                if param.has_default { "1" } else { "0" },
                position
            )),
            ..Default::default()
        })
        .collect()
}

#[cfg(test)]
fn value_items_for_type(names: &[ScopeName], expected: &str) -> Vec<CompletionItem> {
    let mut items = scope_completion_items(names);
    for item in &mut items {
        let matches = item.detail.as_deref() == Some(expected);
        item.sort_text = Some(format!("{}_{}", if matches { "0" } else { "1" }, item.label));
    }
    items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
    items
}

fn named_argument_completion_items(
    state: &DocumentState,
    index: &WorkspaceIndex,
    uri: &Url,
    source: &str,
    offset: usize,
) -> Option<Vec<CompletionItem>> {
    let EditorContext::CallArguments { callee, supplied, value_of, .. } =
        editor_context(source, state.ast.as_ref(), offset)
    else {
        return None;
    };
    let signature = resolve_callable_named(state, index, uri, source, offset, &callee)?;
    if signature.argument_style != CallableArgumentStyle::Named {
        return None;
    }
    if value_of.is_some() {
        // Value position: general expression completion takes over (see `expected_value_type`).
        return None;
    }
    Some(named_parameter_items(&signature, &supplied))
}

/// In `f(param: |)`, the declared type of `param`, used to rank matching locals first.
fn expected_value_type(
    state: &DocumentState,
    index: &WorkspaceIndex,
    uri: &Url,
    source: &str,
    offset: usize,
) -> Option<String> {
    let EditorContext::CallArguments { callee, value_of: Some(param_name), .. } =
        editor_context(source, state.ast.as_ref(), offset)
    else {
        return None;
    };
    let signature = resolve_callable_named(state, index, uri, source, offset, &callee)?;
    if signature.argument_style != CallableArgumentStyle::Named {
        return None;
    }
    signature.params.into_iter().find(|param| param.name == param_name).map(|param| param.ty)
}

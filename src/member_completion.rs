// ── Type-aware `receiver.` completion ────────────────────────────────────────

/// The part of the source before the cursor that names the receiver: everything
/// after an optional partial member name and the `.` that precedes it.
struct ReceiverAt {
    chain: Chain,
    /// Byte offset just after the `.`.
    after_dot: usize,
}

fn receiver_before_cursor(source: &str, offset: usize) -> Option<ReceiverAt> {
    let mut end = offset.min(source.len());
    while !source.is_char_boundary(end) {
        end -= 1;
    }
    let bytes = source.as_bytes();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    // Skip a partially typed member name.
    let mut cursor = end;
    while cursor > 0 && is_ident(bytes[cursor - 1]) {
        cursor -= 1;
    }
    if cursor == 0 || bytes[cursor - 1] != b'.' {
        return None;
    }
    let dot = cursor - 1;
    // `...spread` and `1.5` are not member accesses.
    if dot > 0 && bytes[dot - 1] == b'.' {
        return None;
    }
    // Walk the receiver chain backwards: idents, `.`, and balanced `[...]`.
    let mut steps_rev: Vec<ChainStep> = Vec::new();
    let mut position = dot; // exclusive end of the receiver text
    loop {
        if position == 0 {
            return None;
        }
        if bytes[position - 1] == b']' {
            let mut depth = 0i32;
            let mut index = position;
            loop {
                if index == 0 {
                    return None;
                }
                index -= 1;
                match bytes[index] {
                    b']' => depth += 1,
                    b'[' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            steps_rev.push(ChainStep::Index);
            position = index;
            continue;
        }
        let end_ident = position;
        while position > 0 && is_ident(bytes[position - 1]) {
            position -= 1;
        }
        if position == end_ident {
            return None;
        }
        let word = &source[position..end_ident];
        if word.as_bytes()[0].is_ascii_digit() {
            return None;
        }
        if position > 0 && bytes[position - 1] == b'.' && !(position > 1 && bytes[position - 2] == b'.') {
            steps_rev.push(ChainStep::Field(word.to_string()));
            position -= 1;
            continue;
        }
        steps_rev.reverse();
        return Some(ReceiverAt {
            chain: Chain { root: word.to_string(), steps: steps_rev },
            after_dot: dot + 1,
        });
    }
}

fn element_type(ty: &SparType) -> Option<SparType> {
    match ty {
        SparType::List(inner) => Some((**inner).clone()),
        SparType::Applied { name, arguments } if name == "List" => arguments.first().cloned(),
        SparType::Applied { name, arguments } if name == "Map" => arguments.last().cloned(),
        _ => None,
    }
}

fn field_type_of(symbols: &SymbolTable, ty: &SparType, field: &str) -> Option<SparType> {
    let SparType::Named(name) = ty else {
        return None;
    };
    // A field of a dynamic Record is itself dynamic.
    if name == "Record" {
        return Some(ty.clone());
    }
    if let Some(entry) = symbols.types.get(name) {
        let field = entry.fields.iter().find(|candidate| candidate.name == field)?;
        return match &field.shape {
            spar::ast::TypeFieldShape::Primitive(ty) => Some(ty.clone()),
            spar::ast::TypeFieldShape::Named(name) => Some(SparType::Named(name.clone())),
            spar::ast::TypeFieldShape::TypeParameter(name) => Some(SparType::TypeParameter(name.clone())),
            spar::ast::TypeFieldShape::Applied { name, arguments } => {
                Some(SparType::Applied { name: name.clone(), arguments: arguments.clone() })
            }
            spar::ast::TypeFieldShape::Section(_) => None,
        };
    }
    let path = vec![name.clone()];
    let section = symbols.sections.get(&path)?;
    section.fields.get(field)?.ty.clone()
}

fn type_of_chain(chain: &Chain, scope: &[ScopeName], symbols: &SymbolTable, depth: usize) -> Option<SparType> {
    if depth > 8 {
        return None;
    }
    let mut current = if let Some((index, binding)) =
        scope.iter().enumerate().rev().find(|(_, binding)| binding.name == chain.root)
    {
        // Only names declared before this one can define its type (no cycles).
        let before = &scope[..index];
        if let Some(ty) = &binding.declared {
            ty.clone()
        } else if let Some(init) = &binding.init {
            type_of_chain(init, before, symbols, depth + 1)?
        } else if let Some(iterable) = &binding.element_of {
            element_type(&type_of_chain(iterable, before, symbols, depth + 1)?)?
        } else {
            return None;
        }
    } else {
        match symbols.globals.get(&chain.root)? {
            GlobalEntry::Var { ty, .. } => ty.clone(),
            GlobalEntry::Dynamic { .. } => return None,
        }
    };
    for step in &chain.steps {
        current = match step {
            ChainStep::Field(field) => field_type_of(symbols, &current, field)?,
            ChainStep::Index => element_type(&current)?,
        };
    }
    Some(current)
}


fn owner_name_for_type(ty: &SparType) -> Option<&str> {
    match ty {
        SparType::Named(name) | SparType::Applied { name, .. } => Some(name.as_str()),
        SparType::Str => Some("str"),
        SparType::List(_) => Some("List"),
        _ => None,
    }
}

/// Methods the runtime provides for built-in types (`str`, `List`, `Table`,
/// `Record`, ...), which have no `impl` in any source file.
fn builtin_method_completion_items(symbols: &SymbolTable, owner: &str) -> Vec<CompletionItem> {
    let Some(methods) = symbols.methods.get(owner) else {
        return Vec::new();
    };
    methods
        .iter()
        .filter(|(_, entry)| {
            entry.native_method.is_some() && entry.has_receiver && !entry.function.is_private
        })
        .map(|(name, entry)| {
            // The receiver is the first parameter; callers do not write it.
            let params = entry
                .function
                .params
                .iter()
                .skip(1)
                .map(|(param, ty)| format!("{param}: {}", spar::typechecker::display_type(ty)))
                .collect::<Vec<_>>();
            CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::METHOD),
                detail: Some(format!(
                    "{name}({}) -> {}",
                    params.join(", "),
                    spar::typechecker::display_type(&entry.function.ret)
                )),
                insert_text: Some(name.clone()),
                insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
                sort_text: Some(format!("1_{:03}_{name}", params.len())),
                ..Default::default()
            }
        })
        .collect()
}

fn indexed_method_completion_items(
    index: &WorkspaceIndex,
    uri: &Url,
    owner: &str,
    static_receiver: bool,
) -> Vec<CompletionItem> {
    index
        .visible_methods_for_owner(uri, owner)
        .into_iter()
        .filter(|symbol| {
            matches!(
                (static_receiver, symbol.method_receiver),
                (true, Some(IndexedMethodReceiver::Static))
                    | (false, Some(IndexedMethodReceiver::Shared | IndexedMethodReceiver::Mutable))
            )
        })
        .filter_map(|symbol| {
            let signature = symbol.signature.as_ref()?;
            Some(CompletionItem {
                label: symbol.name.clone(),
                kind: Some(CompletionItemKind::METHOD),
                detail: Some(signature.label()),
                insert_text: Some(symbol.name.clone()),
                insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
                sort_text: Some(format!("1_{:03}_{}", signature.params.len(), symbol.name)),
                ..Default::default()
            })
        })
        .collect()
}

fn merge_member_items(mut fields: Vec<CompletionItem>, mut methods: Vec<CompletionItem>) -> Vec<CompletionItem> {
    for field in &mut fields {
        field.sort_text.get_or_insert_with(|| format!("0_{}", field.label));
    }
    fields.append(&mut methods);
    fields.sort_by(|a, b| a.sort_text.cmp(&b.sort_text).then_with(|| a.label.cmp(&b.label)));
    fields
}
fn type_field_completion_items(symbols: &SymbolTable, ty: &SparType) -> Vec<CompletionItem> {
    let SparType::Named(name) = ty else {
        return Vec::new();
    };
    if let Some(entry) = symbols.types.get(name) {
        return entry
            .fields
            .iter()
            .enumerate()
            .map(|(position, field)| CompletionItem {
                label: field.name.clone(),
                kind: Some(CompletionItemKind::FIELD),
                detail: Some(format!(
                    "{}{}",
                    format_type_field_shape(&field.shape),
                    if field.optional { " (optional)" } else { "" }
                )),
                sort_text: Some(format!("{position:03}")),
                ..Default::default()
            })
            .collect();
    }
    let path = vec![name.clone()];
    let Some(section) = symbols.sections.get(&path) else {
        return Vec::new();
    };
    section_field_completions(symbols, &path, section)
}

/// Completion after `receiver.` (or `receiver.par|`). `Some(items)` means the
/// cursor is in a member-access position and these are the members, possibly
/// none: callers must not fall back to general expression completion. `None`
/// means this is not a member access.
fn typed_member_items_indexed(
    source: &str,
    offset: usize,
    symbols: &SymbolTable,
    index: &WorkspaceIndex,
    uri: &Url,
) -> Option<Vec<CompletionItem>> {
    let receiver = receiver_before_cursor(source, offset)?;
    let scope = local_names_at(source, offset);
    let is_variable = scope.iter().any(|binding| binding.name == receiver.chain.root)
        || matches!(symbols.globals.get(&receiver.chain.root), Some(GlobalEntry::Var { .. }));
    if is_variable {
        return Some(match type_of_chain(&receiver.chain, &scope, symbols, 0) {
            Some(ty) => {
                let fields = type_field_completion_items(symbols, &ty);
                let mut methods = owner_name_for_type(&ty)
                    .map(|owner| indexed_method_completion_items(index, uri, owner, false))
                    .unwrap_or_default();
                if let Some(owner) = owner_name_for_type(&ty) {
                    for item in builtin_method_completion_items(symbols, owner) {
                        if !methods.iter().any(|existing| existing.label == item.label) {
                            methods.push(item);
                        }
                    }
                }
                merge_member_items(fields, methods)
            }
            None => Vec::new(),
        });
    }

    // `Struct.` is a static-method receiver. Preserve legacy canonical-section
    // field completion while adding only receiver-less impl functions.
    if receiver.chain.steps.is_empty() {
        let owner = receiver.chain.root.as_str();
        let path = vec![owner.to_string()];
        if let Some(section) = symbols.sections.get(&path) {
            if section.canonical {
                let fields = section_field_completions(symbols, &path, section);
                let methods = indexed_method_completion_items(index, uri, owner, true);
                return Some(merge_member_items(fields, methods));
            }
        }
        let upto = &source[..receiver.after_dot];
        return member_completion_items(upto, receiver.after_dot, symbols).or(Some(Vec::new()));
    }
    Some(Vec::new())
}


#[cfg(test)]
fn typed_member_items(source: &str, offset: usize, symbols: &SymbolTable) -> Option<Vec<CompletionItem>> {
    let index = WorkspaceIndex::default();
    let uri = Url::parse("file:///__spar_ls_member_completion__.spar").ok()?;
    typed_member_items_indexed(source, offset, symbols, &index, &uri)
}

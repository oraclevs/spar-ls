// ── Checker-driven intelligence: expression types, member/argument/type-name hover ──────────────
//
// The text-based inference in `member_completion.rs` cannot follow closures, loop variables or
// call results. The type checker already infers all of that, so the server asks it for the type of
// every expression once per request (`TypeChecker::check_with_type_map`) and uses those types for
// hover and `.` completion. Every span is verified against the current source text because the
// compiled program also contains items spliced in from imported files.

/// The parsed program, or - while the file has syntax errors - the program parsed from the source
/// with the offending statements blanked out (offsets are preserved), so semantic features keep
/// working mid-edit instead of disappearing until the file parses again.
fn effective_ast(state: &DocumentState) -> Option<std::borrow::Cow<'_, Program>> {
    if let Some(ast) = &state.ast {
        return Some(std::borrow::Cow::Borrowed(ast));
    }
    let repaired = repair_source(&state.source)?;
    let tokens = Lexer::new(&repaired).tokenize().ok()?;
    Parser::new(tokens).parse().ok().map(std::borrow::Cow::Owned)
}

fn typed_map_for(state: &DocumentState) -> spar::typechecker::TypeMap {
    let (Some(ast), Some(symbols)) = (effective_ast(state), state.effective_symbols()) else {
        return Default::default();
    };
    typed_map_of(&ast, symbols, state.source.len())
}

fn typed_map_of(ast: &Program, symbols: &SymbolTable, len: usize) -> spar::typechecker::TypeMap {
    let (_, mut map) = spar::typechecker::TypeChecker::check_with_type_map(ast, symbols);
    map.expressions.retain(|s| s.end <= len && s.start < s.end);
    map.receivers.retain(|s| s.end <= len);
    map
}

/// Receiver type of the member access whose `.` is at byte `dot`.
fn receiver_type_at_dot(source: &str, map: &spar::typechecker::TypeMap, dot: usize) -> Option<SparType> {
    if source.as_bytes().get(dot) != Some(&b'.') {
        return None;
    }
    map.receivers.iter().rev().find(|r| r.start == dot).map(|r| r.ty.clone())
}

/// While the user is typing `recv.` the file usually does not parse. Insert a placeholder member
/// (and the closers the surrounding text needs) so the checker can type the receiver anyway.
fn receiver_type_for_incomplete(state: &DocumentState, symbols: &SymbolTable, dot: usize) -> Option<SparType> {
    let source = &state.source;
    if source.as_bytes().get(dot) != Some(&b'.') {
        return None;
    }
    let after = dot + 1;
    // Drop a partially typed member name.
    let mut member_end = after;
    let bytes = source.as_bytes();
    while member_end < bytes.len() && (bytes[member_end].is_ascii_alphanumeric() || bytes[member_end] == b'_') {
        member_end += 1;
    }
    for suffix in ["", ";", ")", ");", "]", "];", "}", "};"] {
        let candidate = format!("{}.__member{}{}", &source[..dot], suffix, &source[member_end..]);
        let Ok(tokens) = spar::Lexer::new(&candidate).tokenize() else { continue };
        let Ok(program) = spar::Parser::new(tokens).parse() else { continue };
        let map = typed_map_of(&program, symbols, candidate.len());
        if let Some(ty) = receiver_type_at_dot(&candidate, &map, dot) {
            return Some(ty);
        }
    }
    None
}

fn word_bounds(source: &str, offset: usize) -> Option<(usize, usize)> {
    let bytes = source.as_bytes();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut start = offset.min(bytes.len());
    while start > 0 && is_ident(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = offset.min(bytes.len());
    while end < bytes.len() && is_ident(bytes[end]) {
        end += 1;
    }
    (start < end).then_some((start, end))
}

fn describe_callable(name: &str, params: &[spar::SemanticParameter], ret: &SparType) -> String {
    let params = params
        .iter()
        .map(|p| format!("{}: {}", p.name, format_spar_type(&p.ty)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{name}({params}) -> {}", format_spar_type(ret))
}

fn method_doc(name: &str) -> Option<&'static str> {
    Some(match name {
        "asStr" => "Reads a dynamic (`Record`) value as `str`; fails at runtime if it is not a string.",
        "asInt" => "Reads a dynamic (`Record`) value as `int`; fails at runtime if it is not an integer.",
        "asFloat" => "Reads a dynamic (`Record`) value as `float`.",
        "asBool" => "Reads a dynamic (`Record`) value as `bool`.",
        "asList" => "Reads a dynamic (`Record`) value as a list of dynamic values.",
        "typeName" => "The runtime type name of a dynamic value (`\"str\"`, `\"int\"`, `\"list\"`, ...).",
        "append" => "Adds a value to the end of the list (the list variable must be `mut`).",
        "length" => "Number of elements (list, map) or characters (str).",
        _ => return None,
    })
}

/// Hover for `receiver.member` and `receiver.member(...)`.
fn member_hover(state: &DocumentState, symbols: &SymbolTable, map: &spar::typechecker::TypeMap, offset: usize) -> Option<String> {
    let (ws, we) = word_bounds(&state.source, offset)?;
    let name = &state.source[ws..we];
    let bytes = state.source.as_bytes();
    if ws == 0 || bytes[ws - 1] != b'.' || (ws > 1 && bytes[ws - 2] == b'.') {
        return None;
    }
    let receiver = receiver_type_at_dot(&state.source, map, ws - 1)?;
    let snapshot = spar::SemanticSnapshot::new(symbols.clone());
    if let Some(method) = snapshot.methods_for_type(&receiver).into_iter().find(|m| m.callable.name == name) {
        let signature = describe_callable(name, &method.callable.parameters, &method.callable.return_type);
        let mut text = format!("```spar\n(method) {}.{signature}\n```", format_spar_type(&receiver));
        if let Some(doc) = method_doc(name) {
            text.push_str(&format!("\n\n{doc}"));
        }
        return Some(text);
    }
    if let Some(field) = snapshot.fields_for_type(&receiver).into_iter().find(|f| f.name == name) {
        return Some(format!(
            "```spar\n(field) {}.{name}: {}\n```",
            format_spar_type(&receiver),
            format_spar_type(&field.ty)
        ));
    }
    if matches!(receiver, SparType::InlineRecord | SparType::Any) {
        return Some(format!(
            "```spar\n(field) {name}: Any\n```\nDynamic `Record` field. Convert it with `.asStr()`, `.asInt()`, `.asFloat()`, `.asBool()` or `.asList()` before typed use."
        ));
    }
    None
}

/// Finds the callee text and the argument list the cursor is inside: `name(` / `recv.name(`.
fn enclosing_call(source: &str, offset: usize) -> Option<(usize, usize)> {
    let bytes = source.as_bytes();
    let mut depth = 0i32;
    let mut i = offset.min(bytes.len());
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b')' | b']' | b'}' => depth += 1,
            b'(' if depth == 0 => {
                let mut end = i;
                while end > 0 && bytes[end - 1].is_ascii_whitespace() {
                    end -= 1;
                }
                let mut start = end;
                while start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
                    start -= 1;
                }
                return (start < end).then_some((start, end));
            }
            b'(' | b'[' | b'{' => depth -= 1,
            b';' if depth == 0 => return None,
            _ => {}
        }
    }
    None
}

/// Hover for a named-argument label: `append(value: x)` -> `(parameter) value: T`.
fn named_argument_hover(
    state: &DocumentState,
    symbols: &SymbolTable,
    map: &spar::typechecker::TypeMap,
    offset: usize,
) -> Option<String> {
    let (ws, we) = word_bounds(&state.source, offset)?;
    let after = state.source[we..].trim_start();
    if !after.starts_with(':') || after.starts_with("::") {
        return None;
    }
    let label = &state.source[ws..we];
    let (cs, ce) = enclosing_call(&state.source, ws)?;
    let callee = &state.source[cs..ce];
    let snapshot = spar::SemanticSnapshot::new(symbols.clone());
    // `recv.callee(`: method parameters.
    if cs > 0 && state.source.as_bytes()[cs - 1] == b'.' {
        let recv = receiver_type_at_dot(&state.source, map, cs - 1)?;
        let method = snapshot.methods_for_type(&recv).into_iter().find(|m| m.callable.name == callee)?;
        let p = method.callable.parameters.iter().find(|p| p.name == label)?;
        return Some(format!("```spar\n(parameter) {}: {}\n```\nof `{}.{callee}`", p.name, format_spar_type(&p.ty), format_spar_type(&recv)));
    }
    if let Some(function) = symbols.functions.get(callee) {
        let (_, ty) = function.params.iter().find(|(n, _)| n == label)?;
        return Some(format!("```spar\n(parameter) {label}: {}\n```\nof `{callee}`", format_spar_type(ty)));
    }
    // Struct construction `Type(field: ...)`.
    let owner = SparType::Named(callee.to_string());
    let field = snapshot.fields_for_type(&owner).into_iter().find(|f| f.name == label)?;
    Some(format!("```spar\n(field) {label}: {}\n```\nof `{callee}`", format_spar_type(&field.ty)))
}

fn builtin_type_doc(name: &str) -> Option<&'static str> {
    Some(match name {
        "List" => "```spar\nList<T>\n```\nOrdered, growable list. Literal: `[1, 2, 3]`. Methods: `append`, `length`, `map`, `where`, ...",
        "Map" => "```spar\nMap<K, V>\n```\nKey/value map that keeps insertion order.",
        "Option" => "```spar\nOption<T>\n```\nEither `some(value: x)` or `none()`.",
        "Result" => "```spar\nResult<T, E>\n```\nEither `ok(value: x)` or `err(error: e)`.",
        "Record" => "```spar\nRecord\n```\nDynamic object whose fields are typed at runtime. Read fields with `.asStr()`, `.asInt()`, `.asFloat()`, `.asBool()`, `.asList()`.",
        "Promise" => "```spar\nPromise<T>\n```\nResult of an `async` call; use `await` to get the `T`.",
        "Table" => "```spar\nTable<Row>\n```\nRows with a schema; supports `where`, `select`, `sortBy`, `groupBy`.",
        "Stream" => "```spar\nStream<T>\n```\nLazily produced sequence of `T`.",
        "Bytes" => "```spar\nBytes\n```\nRaw binary data.",
        "Buffer" => "```spar\nBuffer\n```\nNative-owned typed array (from a native module). Pass it to native functions without copying.",
        "HttpResponse" => "```spar\nHttpResponse\n```\nResponse of `get`/`post`...: `status`, `body`, `headers`, plus `text()`, `json<T>()`, `isSuccess()`.",
        "Schema" => "```spar\nSchema\n```\nDescription of a table's columns.",
        _ => return None,
    })
}

/// Last-resort hover: builtin type names, then the inferred type of an identifier expression.
fn typed_identifier_hover(
    state: &DocumentState,
    map: &spar::typechecker::TypeMap,
    offset: usize,
) -> Option<String> {
    let (ws, we) = word_bounds(&state.source, offset)?;
    let word = &state.source[ws..we];
    if let Some(doc) = builtin_type_doc(word) {
        return Some(doc.to_string());
    }
    let span = map
        .expressions
        .iter()
        .filter(|s| s.start == ws && s.end == we)
        .last()?;
    Some(format!("```spar\n{word}: {}\n```", format_spar_type(&span.ty)))
}

/// Inferred type of `var name = <init>` when the declaration has no annotation.
fn inferred_binding_type(source: &str, map: &spar::typechecker::TypeMap, decl_end: usize) -> Option<SparType> {
    let rest = source.get(decl_end..)?;
    let trimmed = rest.trim_start();
    let after_eq = trimmed.strip_prefix('=')?;
    if after_eq.starts_with('=') {
        return None;
    }
    let init_start = source.len() - after_eq.len() + (after_eq.len() - after_eq.trim_start().len());
    map.expressions
        .iter()
        .filter(|s| s.start == init_start)
        .max_by_key(|s| s.end)
        .map(|s| s.ty.clone())
}

/// Fields and methods of `ty`, the same list `typed_member_items_indexed` builds for variables.
fn member_items_for_type(symbols: &SymbolTable, index: &WorkspaceIndex, uri: &Url, ty: &SparType) -> Vec<CompletionItem> {
    let fields = type_field_completion_items(symbols, ty);
    let mut methods = owner_name_for_type(ty)
        .map(|owner| indexed_method_completion_items(index, uri, owner, false))
        .unwrap_or_default();
    for item in builtin_method_completion_items(symbols, ty) {
        if !methods.iter().any(|existing| existing.label == item.label) {
            methods.push(item);
        }
    }
    let mut items = merge_member_items(fields, methods);
    if matches!(ty, SparType::InlineRecord | SparType::Any) {
        for (name, doc) in [
            ("asStr", "Read as str"),
            ("asInt", "Read as int"),
            ("asFloat", "Read as float"),
            ("asBool", "Read as bool"),
            ("asList", "Read as a list of dynamic values"),
            ("typeName", "Runtime type name"),
        ] {
            if !items.iter().any(|i| i.label == name) {
                items.push(CompletionItem {
                    label: name.into(),
                    kind: Some(CompletionItemKind::METHOD),
                    detail: Some(doc.into()),
                    sort_text: Some(format!("1_000_{name}")),
                    ..Default::default()
                });
            }
        }
    }
    items
}

/// `typed_member_items_indexed`, falling back to the checker's inferred receiver type when the
/// text-based chain walk cannot resolve it (closure parameters, call results, loop variables...).
fn typed_member_items_with_types(
    state: &DocumentState,
    offset: usize,
    symbols: &SymbolTable,
    index: &WorkspaceIndex,
    uri: &Url,
) -> Option<Vec<CompletionItem>> {
    let primary = typed_member_items_indexed(&state.source, offset, symbols, index, uri);
    if matches!(&primary, Some(items) if !items.is_empty()) {
        return primary;
    }
    // The `.` before the (possibly partial) member name; the receiver may be any expression,
    // including a call result, which the text-based chain walk cannot describe.
    let bytes = state.source.as_bytes();
    let mut cursor = offset.min(bytes.len());
    while cursor > 0 && (bytes[cursor - 1].is_ascii_alphanumeric() || bytes[cursor - 1] == b'_') {
        cursor -= 1;
    }
    if cursor == 0 || bytes[cursor - 1] != b'.' || (cursor > 1 && bytes[cursor - 2] == b'.') {
        return primary;
    }
    let dot = cursor - 1;
    let ty = receiver_type_at_dot(&state.source, &typed_map_for(state), dot)
        .or_else(|| receiver_type_for_incomplete(state, symbols, dot))?;
    let items = member_items_for_type(symbols, index, uri, &ty);
    if items.is_empty() {
        primary
    } else {
        Some(items)
    }
}

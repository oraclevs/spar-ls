// ── Cursor context recovery for editor queries ───────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum EditorContext {
    ImportPath {
        prefix: String,
        package: bool,
    },
    SelectiveImport {
        type_only: bool,
        package: bool,
        path: Option<String>,
        already: HashSet<String>,
    },
    CallArguments {
        callee: String,
        supplied: Vec<String>,
        active_parameter: u32,
        /// `Some(param)` when the cursor sits after `param:` of the current argument.
        value_of: Option<String>,
    },
    Expression,
    Suppressed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorLexicalState {
    Code,
    String,
    LineComment,
    BlockComment,
}

fn lexical_state_at(source: &str, offset: usize) -> CursorLexicalState {
    let bytes = source.as_bytes();
    let end = offset.min(bytes.len());
    let mut i = 0usize;
    let mut block_depth = 0usize;
    let mut in_line = false;
    let mut in_string = false;
    let mut escaped = false;

    while i < end {
        let b = bytes[i];
        let next = bytes.get(i + 1).copied();
        if in_line {
            if b == b'\n' {
                in_line = false;
            }
            i += 1;
            continue;
        }
        if block_depth > 0 {
            if b == b'/' && next == Some(b'*') {
                block_depth += 1;
                i += 2;
                continue;
            }
            if b == b'*' && next == Some(b'/') {
                block_depth = block_depth.saturating_sub(1);
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if b == b'/' && next == Some(b'/') {
            in_line = true;
            i += 2;
            continue;
        }
        if b == b'/' && next == Some(b'*') {
            block_depth = 1;
            i += 2;
            continue;
        }
        if b == b'"' {
            in_string = true;
        }
        i += 1;
    }

    if in_line {
        CursorLexicalState::LineComment
    } else if block_depth > 0 {
        CursorLexicalState::BlockComment
    } else if in_string {
        CursorLexicalState::String
    } else {
        CursorLexicalState::Code
    }
}

fn statement_bounds(source: &str, offset: usize) -> (usize, usize) {
    let bytes = source.as_bytes();
    let mut semicolons = Vec::new();
    let mut i = 0usize;
    let mut block_depth = 0usize;
    let mut in_line = false;
    let mut in_string = false;
    let mut escaped = false;
    while i < bytes.len() {
        let b = bytes[i];
        let next = bytes.get(i + 1).copied();
        if in_line {
            if b == b'\n' { in_line = false; }
            i += 1;
            continue;
        }
        if block_depth > 0 {
            if b == b'/' && next == Some(b'*') { block_depth += 1; i += 2; continue; }
            if b == b'*' && next == Some(b'/') { block_depth -= 1; i += 2; continue; }
            i += 1;
            continue;
        }
        if in_string {
            if escaped { escaped = false; }
            else if b == b'\\' { escaped = true; }
            else if b == b'"' { in_string = false; }
            i += 1;
            continue;
        }
        if b == b'/' && next == Some(b'/') { in_line = true; i += 2; continue; }
        if b == b'/' && next == Some(b'*') { block_depth = 1; i += 2; continue; }
        if b == b'"' { in_string = true; i += 1; continue; }
        if b == b';' { semicolons.push(i); }
        i += 1;
    }
    let clamped = offset.min(source.len());
    let start = semicolons.iter().copied().filter(|at| *at < clamped).max().map_or(0, |at| at + 1);
    let end = semicolons.iter().copied().find(|at| *at >= clamped).map_or(source.len(), |at| at + 1);
    (start, end)
}

fn quoted_import_path(stmt: &str) -> Option<String> {
    let from_at = stmt.find("from\"")
        .or_else(|| stmt.find("from \""))
        .or_else(|| stmt.find("from\n\""));
    let quote_start = if let Some(from_at) = from_at {
        stmt[from_at..].find('"').map(|rel| from_at + rel)?
    } else {
        stmt.find('"')?
    };
    let rest = &stmt[quote_start + 1..];
    let quote_end = rest.find('"')?;
    Some(rest[..quote_end].to_string())
}

fn import_context(source: &str, offset: usize) -> Option<EditorContext> {
    let (start, end) = statement_bounds(source, offset);
    let stmt = source.get(start..end)?;
    let local = offset.saturating_sub(start).min(stmt.len());
    let prefix = stmt.get(..local)?;
    let trimmed = stmt.trim_start();
    if !trimmed.starts_with("import ") && !trimmed.starts_with("import\n") {
        return None;
    }
    let package = trimmed.starts_with("import pkg ") || trimmed.starts_with("import pkg\n");
    let type_only = trimmed.starts_with("import type ")
        || trimmed.starts_with("import type\n")
        || trimmed.starts_with("import pkg type ")
        || trimmed.starts_with("import pkg type\n");

    // If the cursor is in a quoted import path, preserve that string as an
    // import-path context instead of suppressing ordinary string completion.
    let mut quote_open = None;
    let mut escaped = false;
    for (index, ch) in prefix.char_indices() {
        if escaped { escaped = false; continue; }
        if ch == '\\' { escaped = true; continue; }
        if ch == '"' {
            quote_open = if quote_open.is_some() { None } else { Some(index) };
        }
    }
    if let Some(open) = quote_open {
        let before = prefix[..open].trim_end();
        if before.ends_with("from") || (!before.contains('{') && before.starts_with("import")) {
            return Some(EditorContext::ImportPath {
                prefix: prefix[open + 1..].to_string(),
                package,
            });
        }
    }

    let open = stmt.find('{')?;
    if local < open + 1 {
        return Some(EditorContext::Expression);
    }
    let close = stmt[open + 1..].find('}').map(|rel| open + 1 + rel);
    if close.is_some_and(|close| local > close) {
        return Some(EditorContext::Expression);
    }

    let body_end = close.unwrap_or(stmt.len());
    let body = &stmt[open + 1..body_end];
    let already = body
        .split(',')
        .filter_map(|part| {
            let name = part.trim().split_whitespace().next()?;
            (!name.is_empty()).then(|| name.to_string())
        })
        .collect::<HashSet<_>>();

    Some(EditorContext::SelectiveImport {
        type_only,
        package,
        path: quoted_import_path(stmt),
        already,
    })
}

fn masked_code(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = bytes.to_vec();
    let mut i = 0usize;
    let mut block_depth = 0usize;
    let mut in_line = false;
    let mut in_string = false;
    let mut escaped = false;
    while i < bytes.len() {
        let b = bytes[i];
        let next = bytes.get(i + 1).copied();
        if in_line {
            if b == b'\n' { in_line = false; } else { out[i] = b' '; }
            i += 1;
            continue;
        }
        if block_depth > 0 {
            out[i] = if b == b'\n' { b'\n' } else { b' ' };
            if b == b'/' && next == Some(b'*') { if i + 1 < out.len() { out[i + 1] = b' '; } block_depth += 1; i += 2; continue; }
            if b == b'*' && next == Some(b'/') { if i + 1 < out.len() { out[i + 1] = b' '; } block_depth -= 1; i += 2; continue; }
            i += 1;
            continue;
        }
        if in_string {
            if b != b'\n' { out[i] = b' '; }
            if escaped { escaped = false; }
            else if b == b'\\' { escaped = true; }
            else if b == b'"' { in_string = false; }
            i += 1;
            continue;
        }
        if b == b'/' && next == Some(b'/') { out[i]=b' '; if i+1<out.len(){out[i+1]=b' ';} in_line=true; i+=2; continue; }
        if b == b'/' && next == Some(b'*') { out[i]=b' '; if i+1<out.len(){out[i+1]=b' ';} block_depth=1; i+=2; continue; }
        if b == b'"' { out[i]=b' '; in_string=true; i+=1; continue; }
        i += 1;
    }
    String::from_utf8(out).unwrap_or_default()
}

fn split_top_level_arguments(source: &str, masked: &str) -> (Vec<String>, u32) {
    let mut segments = Vec::new();
    let mut start = 0usize;
    let mut paren = 0i32;
    let mut bracket = 0i32;
    let mut brace = 0i32;
    for (index, ch) in masked.char_indices() {
        match ch {
            '(' => paren += 1,
            ')' => paren -= 1,
            '[' => bracket += 1,
            ']' => bracket -= 1,
            '{' => brace += 1,
            '}' => brace -= 1,
            ',' if paren == 0 && bracket == 0 && brace == 0 => {
                segments.push(source[start..index].to_string());
                start = index + 1;
            }
            _ => {}
        }
    }
    segments.push(source[start..].to_string());
    let active = segments.len().saturating_sub(1) as u32;
    (segments, active)
}

fn call_context(source: &str, offset: usize) -> Option<EditorContext> {
    let prefix = source.get(..offset.min(source.len()))?;
    let masked = masked_code(prefix);
    let bytes = masked.as_bytes();
    let mut depth = 0i32;
    let mut open = None;
    for i in (0..bytes.len()).rev() {
        match bytes[i] {
            b')' => depth += 1,
            b'(' if depth == 0 => { open = Some(i); break; }
            b'(' => depth -= 1,
            _ => {}
        }
    }
    let open = open?;
    let before = &masked[..open];
    let end = before.trim_end().len();
    let mut start = end;
    while start > 0 {
        let b = before.as_bytes()[start - 1];
        if b.is_ascii_alphanumeric() || b == b'_' || b == b':' {
            start -= 1;
        } else {
            break;
        }
    }
    let callee = before[start..end].trim().to_string();
    if callee.is_empty() || matches!(callee.as_str(), "if" | "for" | "function" | "task" | "shell") {
        return None;
    }
    let leading = before[..start].trim_end();
    if leading.ends_with("function") || leading.ends_with("task") {
        return None;
    }
    let args_source = &prefix[open + 1..];
    let args_masked = &masked[open + 1..];
    let (segments, active_parameter) = split_top_level_arguments(args_source, args_masked);
    let current_segment = segments.last().map(String::as_str).unwrap_or("");
    let value_of = current_segment.find(':').and_then(|colon| {
        let name = current_segment[..colon].trim();
        (!name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_'))
            .then(|| name.to_string())
    });
    let supplied = segments[..segments.len().saturating_sub(1)]
        .iter()
        .filter_map(|segment| {
            let colon = segment.find(':')?;
            let candidate = segment[..colon].trim();
            (!candidate.is_empty() && candidate.chars().all(|c| c.is_alphanumeric() || c == '_'))
                .then(|| candidate.to_string())
        })
        .collect();
    Some(EditorContext::CallArguments { callee, supplied, active_parameter, value_of })
}

fn editor_context(source: &str, _ast: Option<&Program>, offset: usize) -> EditorContext {
    let lexical = lexical_state_at(source, offset);
    if matches!(lexical, CursorLexicalState::LineComment | CursorLexicalState::BlockComment) {
        return EditorContext::Suppressed;
    }
    if let Some(context) = import_context(source, offset) {
        return context;
    }
    if lexical == CursorLexicalState::String {
        return EditorContext::Suppressed;
    }
    if let Some(context) = call_context(source, offset) {
        return context;
    }
    EditorContext::Expression
}

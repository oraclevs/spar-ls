// ── Completion builders ───────────────────────────────────────────────────────

fn keyword_items() -> Vec<CompletionItem> {
    [
        // declaration keywords
        "var", "export", "private", "import", "dynamic", "as", "struct", "type", "function",
        "schema", "task", "try", "catch",
        // control keywords
        "if", "else", "for", "in", "break", "continue", "return", "mut", // literals
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

fn package_metadata_completion_items(
    path: &std::path::Path,
    source: &str,
    offset: usize,
) -> Option<Vec<CompletionItem>> {
    let file_name = path.file_name()?.to_str()?;
    let prefix = source.get(..offset)?;
    let section = ["Package", "Dependencies", "Overrides", "Lock"]
        .into_iter()
        .filter_map(|name| prefix.rfind(&format!("[{name}]")).map(|at| (at, name)))
        .max_by_key(|(at, _)| *at)?
        .1;
    let section_prefix = &prefix[prefix.rfind(&format!("[{section}]")).unwrap_or(0)..];
    if section_prefix.matches('{').count() <= section_prefix.matches('}').count() {
        return None;
    }

    let current_line = prefix.rsplit_once('\n').map_or(prefix, |(_, line)| line);
    if file_name == spar::package::PACKAGE_MANIFEST_FILE
        && section == "Package"
        && current_line.contains("kind:")
    {
        return Some(value_items(&["application", "library", "config"]));
    }
    if file_name == spar::package::PACKAGE_LOCK_FILE && current_line.contains("sourceKind:") {
        return Some(value_items(&["github", "path"]));
    }

    let fields: &[(&str, &str)] = match (file_name, section) {
        (spar::package::PACKAGE_MANIFEST_FILE, "Package") => &[
            ("name", "name: \"${1:package-name}\";"),
            ("version", "version: \"${1:0.1.0}\";"),
            ("kind", "kind: \"${1:application}\";"),
            ("entry", "entry: \"${1:src/main.spar}\";"),
        ],
        (spar::package::PACKAGE_MANIFEST_FILE, "Dependencies") => &[
            ("github dependency", "${1:alias}: str = \"github:${2:owner/repository@1.0.0}\";"),
            ("local dependency", "${1:alias}: str = \"path:${2:../package}\";"),
        ],
        (spar::package::PACKAGE_MANIFEST_FILE, "Overrides") => &[("local override", "${1:alias}: str = \"path:${2:../package}\";")],
        (spar::package::PACKAGE_LOCK_FILE, "Lock") => &[
            ("formatVersion", "formatVersion: 1;"),
            ("root", "root: [SparLockedDependency] = [$1];"),
            ("packages", "packages: [SparLockedPackage] = [$1];"),
        ],
        _ => return None,
    };
    Some(
        fields
            .iter()
            .filter(|(label, _)| {
                label.contains(' ') || !section_prefix.contains(&format!("{label}:"))
            })
            .map(|(label, insert_text)| CompletionItem {
                label: (*label).to_string(),
                kind: Some(CompletionItemKind::FIELD),
                insert_text: Some((*insert_text).to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            })
            .collect(),
    )
}

fn value_items(values: &[&str]) -> Vec<CompletionItem> {
    values
        .iter()
        .map(|value| CompletionItem {
            label: (*value).to_string(),
            kind: Some(CompletionItemKind::VALUE),
            insert_text: Some((*value).to_string()),
            ..Default::default()
        })
        .collect()
}


// ── Structured-pipe completion ─────────────────────────────────────────────

/// Return the byte offsets of top-level `|>` operators in `text`. Strings and
/// comments are already blanked by `masked_code`; nesting keeps pipes inside
/// call arguments/closures from being mistaken for the current pipeline.
fn structured_pipe_statement_start(source: &str, offset: usize) -> usize {
    let end = offset.min(source.len());
    let masked = masked_code(&source[..end]);
    let bytes = masked.as_bytes();
    let mut paren = 0i32;
    let mut bracket = 0i32;
    let mut brace = 0i32;
    let mut index = bytes.len();
    while index > 0 {
        index -= 1;
        match bytes[index] {
            b')' => paren += 1,
            b'(' if paren > 0 => paren -= 1,
            b']' => bracket += 1,
            b'[' if bracket > 0 => bracket -= 1,
            b'}' => brace += 1,
            b'{' if brace > 0 => brace -= 1,
            b';' if paren == 0 && bracket == 0 && brace == 0 => return index + 1,
            b'{' if paren == 0 && bracket == 0 && brace == 0 => return index + 1,
            _ => {}
        }
    }
    0
}

fn top_level_structured_pipes(text: &str) -> Vec<usize> {
    let masked = masked_code(text);
    let bytes = masked.as_bytes();
    let mut out = Vec::new();
    let mut paren = 0i32;
    let mut bracket = 0i32;
    let mut brace = 0i32;
    let mut index = 0usize;
    while index + 1 < bytes.len() {
        match bytes[index] {
            b'(' => paren += 1,
            b')' => paren = (paren - 1).max(0),
            b'[' => bracket += 1,
            b']' => bracket = (bracket - 1).max(0),
            b'{' => brace += 1,
            b'}' => brace = (brace - 1).max(0),
            b'|' if bytes[index + 1] == b'>' && paren == 0 && bracket == 0 && brace == 0 => {
                out.push(index);
                index += 2;
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    out
}

fn strip_pipeline_assignment_prefix(text: &str) -> &str {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix("return ") {
        return rest.trim();
    }
    // A pipeline commonly appears as the RHS of `var x = ...` or an
    // assignment. The final standalone `=` before the pipeline is the RHS
    // boundary; comparison operators are deliberately ignored.
    let bytes = trimmed.as_bytes();
    let mut candidate = None;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'=' {
            continue;
        }
        let previous = index.checked_sub(1).and_then(|at| bytes.get(at)).copied();
        let next = bytes.get(index + 1).copied();
        if previous != Some(b'=')
            && previous != Some(b'!')
            && previous != Some(b'<')
            && previous != Some(b'>')
            && next != Some(b'=')
            && next != Some(b'>')
        {
            candidate = Some(index);
        }
    }
    candidate.map_or(trimmed, |at| trimmed[at + 1..].trim())
}

fn parse_chain_text(text: &str) -> Option<Chain> {
    let lexed = Lexer::new(text.trim()).tokenize().ok()?;
    let tokens = lexed.iter().map(|token| &token.token).collect::<Vec<_>>();
    parse_chain_tokens(&tokens, 0)
}

fn atomic_pipeline_input_type(
    text: &str,
    source: &str,
    at: usize,
    symbols: &SymbolTable,
) -> Option<SparType> {
    let text = strip_pipeline_assignment_prefix(text);
    if let Some(chain) = parse_chain_text(text) {
        let scope = local_names_at(source, at);
        if let Some(ty) = type_of_chain(&chain, &scope, symbols, 0) {
            return Some(ty);
        }
    }

    let trimmed = text.trim();
    if trimmed == "true" || trimmed == "false" {
        return Some(SparType::Bool);
    }
    if trimmed.parse::<i64>().is_ok() {
        return Some(SparType::Int);
    }
    if trimmed.parse::<f64>().is_ok() && trimmed.contains('.') {
        return Some(SparType::Float);
    }
    if trimmed.starts_with('"') && trimmed.ends_with('"') {
        return Some(SparType::Str);
    }

    // A simple function/constructor call is enough to keep chained pipeline
    // completion type-aware without reparsing the user's incomplete statement.
    if let Some(open) = trimmed.find('(') {
        let name = trimmed[..open].trim();
        if name.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == ':') {
            if let Some(entry) = symbols
                .functions
                .get(name)
                .or_else(|| symbols.imported_functions.get(name))
            {
                return Some(entry.ret.clone());
            }
            let path = vec![name.to_string()];
            if symbols.sections.get(&path).is_some_and(|section| section.canonical) {
                return Some(SparType::Named(name.to_string()));
            }
        }
    }
    None
}

fn bind_pipeline_type_parameters(
    expected: &SparType,
    actual: &SparType,
    bindings: &mut HashMap<String, SparType>,
) -> bool {
    match expected {
        SparType::TypeParameter(name) => match bindings.get(name) {
            Some(bound) => bound == actual,
            None => {
                bindings.insert(name.clone(), actual.clone());
                true
            }
        },
        SparType::List(expected_inner) => match actual {
            SparType::List(actual_inner) => {
                bind_pipeline_type_parameters(expected_inner, actual_inner, bindings)
            }
            SparType::Applied { name, arguments } if name == "List" && arguments.len() == 1 => {
                bind_pipeline_type_parameters(expected_inner, &arguments[0], bindings)
            }
            _ => false,
        },
        SparType::Applied { name, arguments } => match actual {
            SparType::Applied { name: actual_name, arguments: actual_arguments }
                if name == actual_name && arguments.len() == actual_arguments.len() =>
            {
                arguments.iter().zip(actual_arguments).all(|(expected, actual)| {
                    bind_pipeline_type_parameters(expected, actual, bindings)
                })
            }
            SparType::List(actual_inner) if name == "List" && arguments.len() == 1 => {
                bind_pipeline_type_parameters(&arguments[0], actual_inner, bindings)
            }
            _ => false,
        },
        SparType::Function { params, return_type } => match actual {
            SparType::Function { params: actual_params, return_type: actual_return }
                if params.len() == actual_params.len() =>
            {
                params.iter().zip(actual_params).all(|(expected, actual)| {
                    bind_pipeline_type_parameters(expected, actual, bindings)
                }) && bind_pipeline_type_parameters(return_type, actual_return, bindings)
            }
            _ => false,
        },
        _ => expected == actual,
    }
}

fn substitute_pipeline_type(ty: &SparType, bindings: &HashMap<String, SparType>) -> SparType {
    match ty {
        SparType::TypeParameter(name) => bindings.get(name).cloned().unwrap_or_else(|| ty.clone()),
        SparType::List(inner) => {
            SparType::List(Box::new(substitute_pipeline_type(inner, bindings)))
        }
        SparType::Applied { name, arguments } => SparType::Applied {
            name: name.clone(),
            arguments: arguments
                .iter()
                .map(|argument| substitute_pipeline_type(argument, bindings))
                .collect(),
        },
        SparType::Function { params, return_type } => SparType::Function {
            params: params
                .iter()
                .map(|param| substitute_pipeline_type(param, bindings))
                .collect(),
            return_type: Box::new(substitute_pipeline_type(return_type, bindings)),
        },
        _ => ty.clone(),
    }
}

fn pipeline_function_result(
    entry: &FunctionEntry,
    input: &SparType,
) -> Option<(Vec<(String, SparType)>, SparType)> {
    let (_, first) = entry.params.first()?;
    let mut bindings = HashMap::new();
    if !bind_pipeline_type_parameters(first, input, &mut bindings) {
        return None;
    }
    let remaining = entry
        .params
        .iter()
        .skip(1)
        .map(|(name, ty)| (name.clone(), substitute_pipeline_type(ty, &bindings)))
        .collect();
    let output = substitute_pipeline_type(&entry.ret, &bindings);
    Some((remaining, output))
}

fn pipeline_callable_result(ty: &SparType, input: &SparType) -> Option<(Vec<SparType>, SparType)> {
    let SparType::Function { params, return_type } = ty else {
        return None;
    };
    let first = params.first()?;
    let mut bindings = HashMap::new();
    if !bind_pipeline_type_parameters(first, input, &mut bindings) {
        return None;
    }
    Some((
        params
            .iter()
            .skip(1)
            .map(|param| substitute_pipeline_type(param, &bindings))
            .collect(),
        substitute_pipeline_type(return_type, &bindings),
    ))
}

fn pipeline_stage_name(stage: &str) -> Option<&str> {
    let trimmed = stage.trim();
    let end = trimmed.find('(').unwrap_or(trimmed.len());
    let name = trimmed[..end].trim();
    (!name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == ':'))
    .then_some(name)
}

fn infer_pipeline_prefix_type(
    text: &str,
    source: &str,
    absolute_start: usize,
    symbols: &SymbolTable,
) -> Option<SparType> {
    let pipes = top_level_structured_pipes(text);
    let mut segments = Vec::new();
    let mut start = 0usize;
    for pipe in &pipes {
        segments.push(&text[start..*pipe]);
        start = *pipe + 2;
    }
    segments.push(&text[start..]);
    let mut current = atomic_pipeline_input_type(
        segments.first()?.trim(),
        source,
        absolute_start + text.len(),
        symbols,
    )?;
    for stage in segments.iter().skip(1) {
        let name = pipeline_stage_name(stage)?;
        if let Some(entry) = symbols
            .functions
            .get(name)
            .or_else(|| symbols.imported_functions.get(name))
        {
            current = pipeline_function_result(entry, &current)?.1;
            continue;
        }
        let callable = match symbols.globals.get(name) {
            Some(GlobalEntry::Var { ty, .. }) => ty,
            _ => return None,
        };
        current = pipeline_callable_result(callable, &current)?.1;
    }
    Some(current)
}

fn pipeline_completion_insert(name: &str, params: &[(String, SparType)], snippets: bool) -> String {
    if params.is_empty() {
        return format!("{name}()");
    }
    if !snippets {
        return format!("{name}()");
    }
    let arguments = params
        .iter()
        .enumerate()
        .map(|(index, (param, _))| format!("${{{}:{}}}", index + 1, param))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{name}({arguments})")
}

/// Completion immediately after a structured-value pipe. `Some` means the
/// cursor is in a `|>` stage position; an empty vector deliberately suppresses
/// unrelated expression completion when no callable accepts the input type.
fn structured_pipe_contextual_diagnostic(
    state: &DocumentState,
    error: &SparError,
) -> Option<String> {
    let raw = diagnostics::error_message(error);
    let stage = raw
        .strip_prefix("structured pipe stage '")?
        .split_once('\'')?
        .0;
    let symbols = state.effective_symbols()?;
    let span = diagnostics::error_span(error);
    let statement_start = structured_pipe_statement_start(&state.source, span.start);
    let statement_end = state.source[statement_start..]
        .find(';')
        .map(|relative| statement_start + relative)
        .unwrap_or(state.source.len());
    let statement = &state.source[statement_start..statement_end];
    let pipes = top_level_structured_pipes(statement);

    let mut input_type = None;
    for (index, pipe) in pipes.iter().enumerate() {
        let stage_start = pipe + 2;
        let stage_end = pipes.get(index + 1).copied().unwrap_or(statement.len());
        let stage_text = &statement[stage_start..stage_end];
        if pipeline_stage_name(stage_text) == Some(stage) {
            input_type = infer_pipeline_prefix_type(
                &statement[..*pipe],
                &state.source,
                statement_start,
                symbols,
            );
            break;
        }
    }
    let input_type = input_type?;
    let expected = if let Some(entry) = symbols
        .functions
        .get(stage)
        .or_else(|| symbols.imported_functions.get(stage))
    {
        entry.params.first().map(|(_, ty)| ty.clone())
    } else {
        match symbols.globals.get(stage) {
            Some(GlobalEntry::Var {
                ty: SparType::Function { params, .. },
                ..
            }) => params.first().cloned(),
            _ => None,
        }
    }?;

    if input_type == expected {
        return None;
    }
    Some(format!(
        "structured pipe stage '{stage}': cannot pass {} into a first parameter of type {}",
        format_spar_type(&input_type),
        format_spar_type(&expected),
    ))
}

fn structured_pipe_completion_items(
    source: &str,
    offset: usize,
    symbols: &SymbolTable,
    snippets: bool,
) -> Option<Vec<CompletionItem>> {
    let statement_start = structured_pipe_statement_start(source, offset);
    let prefix = source.get(statement_start..offset.min(source.len()))?;
    let pipes = top_level_structured_pipes(prefix);
    let current_pipe = *pipes.last()?;
    let stage_prefix = prefix[current_pipe + 2..].trim();
    if !stage_prefix
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == ':')
    {
        return None;
    }
    let pipeline_prefix = &prefix[..current_pipe];
    let input_type = infer_pipeline_prefix_type(
        pipeline_prefix,
        source,
        statement_start,
        symbols,
    )?;

    let mut items = Vec::new();
    let mut seen = HashSet::new();
    for (name, entry) in symbols
        .functions
        .iter()
        .chain(symbols.imported_functions.iter())
    {
        if !name.starts_with(stage_prefix) || seen.contains(name) {
            continue;
        }
        let Some((remaining, output)) = pipeline_function_result(entry, &input_type) else {
            continue;
        };
        seen.insert(name.clone());
        let detail = format!(
            "{} |> {}({}) -> {}",
            format_spar_type(&input_type),
            name,
            remaining
                .iter()
                .map(|(param, ty)| format!("{param}: {}", format_spar_type(ty)))
                .collect::<Vec<_>>()
                .join(", "),
            format_spar_type(&output)
        );
        items.push(CompletionItem {
            label: name.clone(),
            kind: Some(CompletionItemKind::FUNCTION),
            detail: Some(detail),
            insert_text: Some(pipeline_completion_insert(name, &remaining, snippets)),
            insert_text_format: Some(if snippets {
                InsertTextFormat::SNIPPET
            } else {
                InsertTextFormat::PLAIN_TEXT
            }),
            sort_text: Some(format!("0_{name}")),
            ..Default::default()
        });
    }

    for (name, entry) in &symbols.globals {
        if !name.starts_with(stage_prefix) || seen.contains(name) {
            continue;
        }
        let GlobalEntry::Var { ty, .. } = entry else {
            continue;
        };
        let Some((remaining, output)) = pipeline_callable_result(ty, &input_type) else {
            continue;
        };
        seen.insert(name.clone());
        let params = remaining
            .into_iter()
            .enumerate()
            .map(|(index, ty)| (format!("arg{}", index + 1), ty))
            .collect::<Vec<_>>();
        items.push(CompletionItem {
            label: name.clone(),
            kind: Some(CompletionItemKind::FUNCTION),
            detail: Some(format!(
                "{} |> {}(...) -> {}",
                format_spar_type(&input_type),
                name,
                format_spar_type(&output)
            )),
            insert_text: Some(pipeline_completion_insert(name, &params, snippets)),
            insert_text_format: Some(if snippets {
                InsertTextFormat::SNIPPET
            } else {
                InsertTextFormat::PLAIN_TEXT
            }),
            sort_text: Some(format!("1_{name}")),
            ..Default::default()
        });
    }
    items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text).then_with(|| a.label.cmp(&b.label)));
    Some(items)
}

fn top_level_start(item: &TopLevelItem) -> usize {
    match item {
        TopLevelItem::Import(d) => d.span.start,
        TopLevelItem::Var(d) => d.span.start,
        TopLevelItem::Dynamic(d) => d.span.start,
        TopLevelItem::Section(d) => d.span.start,
        TopLevelItem::Impl(d) => d.span.start,
        TopLevelItem::Function(d) => d.span.start,
        TopLevelItem::SchemaSection(d) => d.span.start,
        TopLevelItem::Type(d) => d.span.start,
        TopLevelItem::SchemaFrom(d) => d.span.start,
        TopLevelItem::Enum(d) => d.span.start,
        TopLevelItem::FunctionGroup(d) => d.span.start,
        TopLevelItem::Task(d) => d.span.start,
        TopLevelItem::Statement(statement) => match statement {
            FuncStmt::LocalVar(d) => d.span.start,
            FuncStmt::Assignment { span, .. }
            | FuncStmt::FieldAssignment { span, .. }
            | FuncStmt::Expression(_, span)
            | FuncStmt::Return(_, span)
            | FuncStmt::Break(span)
            | FuncStmt::Continue(span) => span.start,
            FuncStmt::If(d) => d.span.start,
            FuncStmt::For(d) => d.span.start,
            FuncStmt::Try(d) => d.span.start,
        },
    }
}

fn task_at_offset<'a>(
    program: &'a Program,
    source: &str,
    offset: usize,
) -> Option<(&'a spar::ast::TaskDecl, usize, usize)> {
    program.items.iter().enumerate().find_map(|(index, item)| {
        let TopLevelItem::Task(task) = item else {
            return None;
        };
        let (body_start, body_end) = task_body_bounds(program, source, index, task)?;
        (body_start <= offset && offset <= body_end).then_some((task.as_ref(), body_start, body_end))
    })
}

fn task_body_bounds(
    program: &Program,
    source: &str,
    index: usize,
    task: &spar::ast::TaskDecl,
) -> Option<(usize, usize)> {
    let next_start = program
        .items
        .get(index + 1)
        .map(top_level_start)
        .unwrap_or(source.len());
    let tokens = Lexer::new(source).tokenize().ok()?;
    let mut paren_depth = 0u32;
    let mut brace_depth = 0u32;
    let mut body_start = None;
    for token in tokens
        .into_iter()
        .filter(|token| token.span.start >= task.name_span.end && token.span.start < next_start)
    {
        if body_start.is_none() {
            match token.token {
                spar::Token::LParen => paren_depth += 1,
                spar::Token::RParen => paren_depth = paren_depth.saturating_sub(1),
                spar::Token::LBrace if paren_depth == 0 => {
                    body_start = Some(token.span.end);
                    brace_depth = 1;
                }
                _ => {}
            }
            continue;
        }
        match token.token {
            spar::Token::LBrace => brace_depth += 1,
            spar::Token::RBrace => {
                brace_depth = brace_depth.saturating_sub(1);
                if brace_depth == 0 {
                    return Some((body_start?, token.span.start));
                }
            }
            _ => {}
        }
    }
    None
}

fn offset_in_expr(expr: &spar::ast::Expr, offset: usize) -> bool {
    !matches!(expr, spar::ast::Expr::Literal(_))
        && expr_span(expr).start <= offset
        && offset <= expr_span(expr).end
}

fn task_interpolation_at_offset(
    task: &spar::ast::TaskDecl,
    offset: usize,
) -> Option<&spar::ast::Expr> {
    task.run_blocks.iter().find_map(|block| match &block.body {
        spar::ast::RunBody::Bash(commands) => commands.iter().find_map(|command| {
            command.parts.iter().find_map(|part| match part {
                spar::ast::ShellTemplatePart::Expr(expr) if offset_in_expr(expr, offset) => {
                    Some(expr)
                }
                _ => None,
            })
        }),
        spar::ast::RunBody::Native(shell) => native_interpolation_at_offset(shell, offset),
    })
}

/// The `${...}` expression under `offset` inside a native shell body: command
/// words in flattened `steps`, plus command-only expression statements
/// (nested shells recurse).
fn native_interpolation_at_offset(
    shell: &spar::ast::ShellExpr,
    offset: usize,
) -> Option<&spar::ast::Expr> {
    use spar::ast::{ShellStep, Statement};
    shell
        .steps
        .iter()
        .find_map(|(_, step)| match step {
            ShellStep::Command(command) => interpolation_in_command(command, offset),
            ShellStep::Pipeline(commands) => commands
                .iter()
                .find_map(|command| interpolation_in_command(command, offset)),
            ShellStep::MixedPipeline(pipeline) => pipeline
                .input
                .iter()
                .chain(pipeline.output.iter())
                .find_map(|command| interpolation_in_command(command, offset)),
        })
        .or_else(|| {
            shell.statements.iter().find_map(|statement| match statement {
                Statement::Expression(spar::ast::Expr::Shell(inner), _) => {
                    native_interpolation_at_offset(inner, offset)
                }
                _ => None,
            })
        })
}

fn interpolation_in_command(
    command: &spar::ast::ShellCommandExpr,
    offset: usize,
) -> Option<&spar::ast::Expr> {
    std::iter::once(&command.program)
        .chain(command.environment.iter().map(|entry| &entry.value))
        .chain(command.args.iter())
        .find_map(|word| {
            word.parts.iter().find_map(|part| match part {
                spar::ast::ShellWordPart::Expr(expr) if offset_in_expr(expr, offset) => Some(expr),
                _ => None,
            })
        })
}

fn task_param_items(params: &[(String, String)]) -> Vec<CompletionItem> {
    params
        .iter()
        .map(|(name, ty)| CompletionItem {
            label: name.clone(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some(ty.clone()),
            ..Default::default()
        })
        .collect()
}

/// True when the cursor sits inside an unclosed `${...` interpolation.
fn in_open_interpolation(source: &str, offset: usize) -> bool {
    source
        .get(..offset)
        .and_then(|before| before.rfind("${").map(|start| !before[start..].contains('}')))
        .unwrap_or(false)
}

fn task_body_items(scope: &crate::task_context::TaskScope) -> Vec<CompletionItem> {
    let fields = [
        ("description", "Human-readable task description", "description: \"$1\";"),
        ("default", "Run when no task name is supplied", "default: ${1|true,false|};"),
        ("quiet", "Command echoing (quiet by default; set false to show commands)", "quiet: ${1|true,false|};"),
        ("private", "Hide the task from public listings", "private: ${1|true,false|};"),
        ("group", "Group shown in task listings", "group: \"$1\";"),
        ("confirm", "Confirmation prompt before running", "confirm: \"$1\";"),
        ("dependsOn", "Tasks that must run first", "dependsOn: [$1];"),
        ("env", "Environment variables for commands", "env: {\n\t$0\n};"),
        ("cwd", "Working directory for commands", "cwd: \"$1\";"),
    ];
    let mut items: Vec<CompletionItem> = fields
        .into_iter()
        .filter(|(name, _, _)| !scope.present_fields.iter().any(|present| present == name))
        .map(|(label, detail, insert_text)| CompletionItem {
            label: label.to_string(),
            kind: Some(CompletionItemKind::FIELD),
            detail: Some(detail.to_string()),
            insert_text: Some(insert_text.to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        })
        .collect();

    // One `run` block per OS slot, whichever shell it uses.
    let slot_free = |os: Option<&str>| !scope.used_os_slots.iter().any(|used| used.as_deref() == os);
    let mut run_snippets: Vec<(&str, &str, &str)> = Vec::new();
    if slot_free(None) {
        run_snippets.push(("run", "Spar shell commands, any OS", "run {\n\t$0\n};"));
        run_snippets.push(("run bash", "Bash commands, any OS", "run bash {\n\t$0\n};"));
    }
    for os in ["linux", "macos", "windows"] {
        if slot_free(Some(os)) {
            run_snippets.push((
                match os {
                    "linux" => "run linux",
                    "macos" => "run macos",
                    _ => "run windows",
                },
                "Spar shell commands, this OS only",
                match os {
                    "linux" => "run linux {\n\t$0\n};",
                    "macos" => "run macos {\n\t$0\n};",
                    _ => "run windows {\n\t$0\n};",
                },
            ));
            run_snippets.push((
                match os {
                    "linux" => "run bash linux",
                    "macos" => "run bash macos",
                    _ => "run bash windows",
                },
                "Bash commands, this OS only",
                match os {
                    "linux" => "run bash linux {\n\t$0\n};",
                    "macos" => "run bash macos {\n\t$0\n};",
                    _ => "run bash windows {\n\t$0\n};",
                },
            ));
        }
    }
    items.extend(run_snippets.into_iter().map(|(label, detail, insert_text)| CompletionItem {
        label: label.to_string(),
        kind: Some(CompletionItemKind::SNIPPET),
        detail: Some(detail.to_string()),
        insert_text: Some(insert_text.to_string()),
        insert_text_format: Some(InsertTextFormat::SNIPPET),
        ..Default::default()
    }));
    items
}

fn run_header_items(
    shell_seen: bool,
    os_seen: bool,
    used_os_slots: &[Option<String>],
) -> Vec<CompletionItem> {
    if os_seen {
        return Vec::new();
    }
    let mut items = Vec::new();
    if !shell_seen {
        for (word, detail) in [
            ("spar", "Native Spar shell language (default)"),
            ("bash", "Raw bash, run with bash -c"),
        ] {
            items.push(CompletionItem {
                label: word.to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                detail: Some(detail.to_string()),
                ..Default::default()
            });
        }
    }
    for os in ["linux", "macos", "windows"] {
        if used_os_slots.iter().any(|used| used.as_deref() == Some(os)) {
            continue;
        }
        items.push(CompletionItem {
            label: os.to_string(),
            kind: Some(CompletionItemKind::ENUM_MEMBER),
            detail: Some("Only run on this operating system".to_string()),
            ..Default::default()
        });
    }
    items
}

fn task_field_value_items(field: &str) -> Vec<CompletionItem> {
    match field {
        "quiet" | "default" | "private" => ["true", "false"]
            .into_iter()
            .map(|value| CompletionItem {
                label: value.to_string(),
                kind: Some(CompletionItemKind::VALUE),
                ..Default::default()
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn task_dependency_items(symbols: Option<&SymbolTable>, current: &str) -> Vec<CompletionItem> {
    symbols
        .map(|symbols| {
            symbols
                .tasks
                .keys()
                .filter(|name| name.as_str() != current)
                .map(|name| CompletionItem {
                    label: name.clone(),
                    kind: Some(CompletionItemKind::REFERENCE),
                    detail: Some("task dependency".to_string()),
                    ..Default::default()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Completion inside a `task` declaration. Works from the text before the
/// cursor alone (see `task_context`), so it keeps working while the file
/// doesn't parse; `symbols` only supplies the task names for `dependsOn`.
/// `None` means "not a task-specific position" — callers fall through to the
/// general completion path.
fn task_completion_items(
    source: &str,
    symbols: Option<&SymbolTable>,
    offset: usize,
) -> Option<Vec<CompletionItem>> {
    use crate::task_context::{task_context, TaskContext};
    let scope = task_context(source, offset)?;
    match &scope.context {
        TaskContext::Params => None,
        TaskContext::Body => Some(task_body_items(&scope)),
        TaskContext::FieldValue(field) => Some(task_field_value_items(field)),
        TaskContext::DependsOn => Some(task_dependency_items(symbols, &scope.name)),
        TaskContext::RunHeader { shell_seen, os_seen } => {
            Some(run_header_items(*shell_seen, *os_seen, &scope.used_os_slots))
        }
        TaskContext::RunBody(shell) => {
            if in_open_interpolation(source, offset) {
                // A native body's `${expr}` takes any Spar expression (its own
                // locals, globals, functions): fall through to the general
                // expression completion, where the task's parameters are
                // added by `run_interpolation_params`. A raw bash body only
                // binds the task's parameters.
                if *shell != spar::ast::RunShell::Bash && symbols.is_some() {
                    None
                } else {
                    Some(task_param_items(&scope.params))
                }
            } else if *shell == spar::ast::RunShell::Bash {
                // Raw bash: nothing of ours to offer, and the generic Spar
                // completions would only be noise.
                Some(Vec::new())
            } else {
                None
            }
        }
    }
}

/// The enclosing task's parameters, when the cursor is inside an open `${...`
/// in one of its run bodies.
fn run_interpolation_params(source: &str, offset: usize) -> Vec<CompletionItem> {
    use crate::task_context::{task_context, TaskContext};
    match task_context(source, offset) {
        Some(scope)
            if matches!(scope.context, TaskContext::RunBody(shell) if shell != spar::ast::RunShell::Bash)
                && in_open_interpolation(source, offset) =>
        {
            task_param_items(&scope.params)
                .into_iter()
                .map(|item| with_tier(item, 0))
                .collect()
        }
        _ => Vec::new(),
    }
}

fn member_completion_items(
    source: &str,
    offset: usize,
    symbols: &SymbolTable,
) -> Option<Vec<CompletionItem>> {
    let before_cursor = source.get(..offset)?;
    let before_dot = before_cursor.strip_suffix('.')?;
    let base_start = before_dot
        .char_indices()
        .rev()
        .find(|(_, ch)| !ch.is_alphanumeric() && *ch != '_')
        .map_or(0, |(index, ch)| index + ch.len_utf8());
    let base = before_dot.get(base_start..)?;
    if base.is_empty() {
        return Some(Vec::new());
    }

    if let Some(group) = symbols.function_groups.get(base) {
        return Some(function_completion_items(&group.functions, false));
    }

    let section_path = vec![base.to_string()];
    if let Some(section) = symbols.sections.get(&section_path) {
        return Some(section_field_completions(symbols, &section_path, section));
    }

    let named_kind = match symbols.globals.get(base) {
        Some(GlobalEntry::Var {
            ty: SparType::Named(name),
            ..
        }) => Some(name.as_str()),
        _ if symbols.types.contains_key(base) || symbols.enums.contains_key(base) => Some(base),
        _ => None,
    };
    let Some(name) = named_kind else {
        return Some(Vec::new());
    };

    if let Some(entry) = symbols.types.get(name) {
        return Some(
            entry
            .fields
            .iter()
            .map(|field| CompletionItem {
                label: field.name.clone(),
                kind: Some(CompletionItemKind::FIELD),
                detail: Some(format_type_field_shape(&field.shape)),
                ..Default::default()
            })
            .collect(),
        );
    }

    Some(
        symbols
            .enums
            .get(name)
            .into_iter()
            .flat_map(|entry| &entry.variants)
            .map(|variant| CompletionItem {
                label: variant.clone(),
                kind: Some(CompletionItemKind::ENUM_MEMBER),
                ..Default::default()
            })
            .collect(),
    )
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


fn constructor_completion_items(
    symbols: &SymbolTable,
    index: &WorkspaceIndex,
    uri: &Url,
    snippets: bool,
) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    for (path, section) in &symbols.sections {
        if path.len() != 1 || !section.canonical {
            continue;
        }
        let owner = &path[0];
        if !seen.insert(owner.clone()) {
            continue;
        }
        let Some(constructor) = index.visible_constructor_for_owner(uri, owner) else {
            continue;
        };
        let Some(signature) = constructor.signature.as_ref() else {
            continue;
        };
        let (insert_text, insert_text_format) = if snippets {
            let required = signature
                .params
                .iter()
                .filter(|param| !param.has_default)
                .enumerate()
                .map(|(position, param)| format!("{}: ${{{}}}", param.name, position + 1))
                .collect::<Vec<_>>()
                .join(", ");
            (
                Some(format!("{}({required})", owner)),
                Some(InsertTextFormat::SNIPPET),
            )
        } else {
            (Some(owner.clone()), Some(InsertTextFormat::PLAIN_TEXT))
        };
        items.push(CompletionItem {
            label: owner.clone(),
            kind: Some(CompletionItemKind::CONSTRUCTOR),
            detail: Some(signature.label()),
            insert_text,
            insert_text_format,
            sort_text: Some(format!("1_constructor_{owner}")),
            ..Default::default()
        });
    }
    items
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

fn function_completion_items(functions: &HashMap<String, FunctionEntry>, snippets: bool) -> Vec<CompletionItem> {
    functions
        .iter()
        .map(|(name, entry)| {
            let param_list = entry
                .params
                .iter()
                .map(|(pname, pty)| format!("{}: {}", pname, format_spar_type(pty)))
                .collect::<Vec<_>>()
                .join(", ");
            let required = entry
                .params
                .iter()
                .filter(|(param, _)| !entry.default_params.contains(param))
                .collect::<Vec<_>>();
            let (insert_text, insert_text_format) = if snippets {
                let args = required
                    .iter()
                    .enumerate()
                    .map(|(index, (param, _))| format!("{}: ${{{}}}", param, index + 1))
                    .collect::<Vec<_>>()
                    .join(", ");
                (Some(format!("{}({})", name, args)), Some(InsertTextFormat::SNIPPET))
            } else {
                (Some(name.clone()), Some(InsertTextFormat::PLAIN_TEXT))
            };
            CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some(format!(
                    "({}) -> {}",
                    param_list,
                    format_spar_type(&entry.ret)
                )),
                insert_text,
                insert_text_format,
                ..Default::default()
            }
        })
        .collect()
}

/// `Devices::` and `EdgeInsect::` completions — enum variant access and
/// function-group member access are both `::`-namespaced in Spar (see
/// `Devices::Android`, `EdgeInsect::only()`), never `.`, so this lives
/// alongside `section_field_completions` rather than `member_completion_items`.
fn enum_or_group_path_completions(symbols: &SymbolTable, name: &str) -> Option<Vec<CompletionItem>> {
    if let Some(entry) = symbols.enums.get(name) {
        return Some(
            entry
                .variants
                .iter()
                .map(|variant| CompletionItem {
                    label: variant.clone(),
                    kind: Some(CompletionItemKind::ENUM_MEMBER),
                    ..Default::default()
                })
                .collect(),
        );
    }
    if let Some(group) = symbols.function_groups.get(name) {
        return Some(function_completion_items(&group.functions, false));
    }
    None
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

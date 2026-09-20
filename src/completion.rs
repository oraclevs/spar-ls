// ── Completion builders ───────────────────────────────────────────────────────

fn keyword_items() -> Vec<CompletionItem> {
    [
        // declaration keywords
        "var", "export", "private", "import", "dynamic", "as", "struct", "type", "function",
        "task", "try", "catch",
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

fn top_level_start(item: &TopLevelItem) -> usize {
    match item {
        TopLevelItem::Import(d) => d.span.start,
        TopLevelItem::Var(d) => d.span.start,
        TopLevelItem::Dynamic(d) => d.span.start,
        TopLevelItem::Section(d) => d.span.start,
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

fn task_field_is_present(source: &str, body_start: usize, body_end: usize, name: &str) -> bool {
    let Some(body) = source.get(body_start..body_end) else {
        return false;
    };
    body.match_indices(name).any(|(offset, _)| {
        let before = &body[..offset];
        let line_prefix = before.rsplit_once('\n').map_or(before, |(_, line)| line);
        let prefix = line_prefix.trim_end();
        let after = &body[offset + name.len()..];
        (prefix.is_empty() || prefix.ends_with('{') || prefix.ends_with(';'))
            && after.trim_start().starts_with(':')
    })
}

fn task_metadata_completion_items(
    task: &spar::ast::TaskDecl,
    source: &str,
    body_start: usize,
    body_end: usize,
) -> Vec<CompletionItem> {
    [
        ("description", "Human-readable task description", "description: \"$1\";"),
        ("default", "Run when no task name is supplied", "default: ${1:true};"),
        ("quiet", "Command echoing (quiet by default; set false to show commands)", "quiet: ${1:false};"),
        ("private", "Hide the task from public listings", "private: ${1:true};"),
        ("group", "Group shown in task listings", "group: \"$1\";"),
        ("confirm", "Confirmation prompt before running", "confirm: \"$1\";"),
        ("dependsOn", "Tasks that must run first", "dependsOn: [$1];"),
        ("env", "Environment variables for commands", "env: {\n\t$0\n};"),
        ("cwd", "Working directory for commands", "cwd: \"$1\";"),
        ("shell", "Shell executable and arguments", "shell: [$1];"),
        ("run", "Shell commands executed by the task", "run {\n\t$0\n};"),
    ]
    .into_iter()
    .filter(|(name, _, _)| {
        if *name == "run" {
            // A task can now carry any number of labeled `run <os>` blocks
            // alongside at most one bare default — only offer the `run`
            // snippet while that default block is still missing.
            !task.run_blocks.iter().any(|block| block.os.is_none())
        } else {
            !task_field_is_present(source, body_start, body_end, name)
        }
    })
    .map(|(label, detail, insert_text)| CompletionItem {
        label: label.to_string(),
        kind: Some(CompletionItemKind::FIELD),
        detail: Some(detail.to_string()),
        insert_text: Some(insert_text.to_string()),
        insert_text_format: Some(InsertTextFormat::SNIPPET),
        ..Default::default()
    })
    .collect()
}

fn cursor_in_depends_on(source: &str, body_start: usize, offset: usize) -> bool {
    let Some(before_cursor) = source.get(body_start..offset) else {
        return false;
    };
    let Some(field_start) = before_cursor.rfind("dependsOn") else {
        return false;
    };
    let field = &before_cursor[field_start + "dependsOn".len()..];
    field.rfind('[').is_some_and(|open| !field[open + 1..].contains(']'))
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
    task.run_blocks.iter().find_map(|block| {
        block.commands.iter().find_map(|command| {
            command.parts.iter().find_map(|part| match part {
                spar::ast::ShellTemplatePart::Expr(expr) if offset_in_expr(expr, offset) => {
                    Some(expr)
                }
                _ => None,
            })
        })
    })
}

fn cursor_in_task_run(source: &str, body_start: usize, body_end: usize, offset: usize) -> bool {
    let Ok(tokens) = Lexer::new(source).tokenize() else {
        return false;
    };
    let mut run_start = None;
    for token in tokens {
        if token.span.start < body_start || token.span.start > body_end {
            continue;
        }
        match token.token {
            spar::Token::RunStart => run_start = Some(token.span.start),
            spar::Token::RunEnd => {
                if run_start.is_some_and(|start| start < offset && offset <= token.span.end) {
                    return true;
                }
                run_start = None;
            }
            _ => {}
        }
    }
    false
}

fn task_value_completion_items(task: &spar::ast::TaskDecl) -> Vec<CompletionItem> {
    task
        .params
        .iter()
        .map(|param| CompletionItem {
            label: param.name.clone(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some(format_spar_type(&param.ty)),
            ..Default::default()
        })
        .collect()
}

fn task_entry_value_completion_items(entry: &spar::resolver::TaskEntry) -> Vec<CompletionItem> {
    entry
        .params
        .iter()
        .map(|(name, ty)| CompletionItem {
            label: name.clone(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some(format_spar_type(ty)),
            ..Default::default()
        })
        .collect()
}

fn incomplete_task_interpolation_items(
    source: &str,
    symbols: &SymbolTable,
    offset: usize,
) -> Option<Vec<CompletionItem>> {
    if !source.get(..offset)?.trim_end().ends_with("${") {
        return None;
    }
    let (name, entry) = symbols
        .tasks
        .iter()
        .filter(|(_, entry)| entry.span.start <= offset)
        .max_by_key(|(_, entry)| entry.span.start)?;
    let task_source = source.get(entry.span.start..offset)?;
    let run_start = task_source.rfind("run")?;
    if !task_source[run_start + "run".len()..].contains('{') {
        return None;
    }
    symbols
        .tasks
        .get(name)
        .map(task_entry_value_completion_items)
}

fn task_completion_items(
    program: Option<&Program>,
    source: &str,
    symbols: &SymbolTable,
    offset: usize,
) -> Option<Vec<CompletionItem>> {
    let Some(program) = program else {
        return incomplete_task_interpolation_items(source, symbols, offset);
    };
    let (task, body_start, body_end) = task_at_offset(program, source, offset)?;
    if task_interpolation_at_offset(task, offset).is_some() {
        return Some(task_value_completion_items(task));
    }
    if cursor_in_depends_on(source, body_start, offset) {
        return Some(
            symbols
                .tasks
                .keys()
                .filter(|name| name.as_str() != task.name)
                .map(|name| CompletionItem {
                    label: name.clone(),
                    kind: Some(CompletionItemKind::REFERENCE),
                    detail: Some("task dependency".to_string()),
                    ..Default::default()
                })
                .collect(),
        );
    }
    if cursor_in_task_run(source, body_start, body_end, offset) {
        return Some(Vec::new());
    }
    Some(task_metadata_completion_items(task, source, body_start, body_end))
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

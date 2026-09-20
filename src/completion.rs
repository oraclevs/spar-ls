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

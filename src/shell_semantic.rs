// ── Native shell semantic intelligence ───────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandResolution {
    Builtin,
    Resolved,
    Unresolved,
}

#[derive(Debug, Clone)]
struct CommandResolver {
    paths: Vec<PathBuf>,
}

impl CommandResolver {
    fn with_path(path: Option<&std::ffi::OsStr>) -> Self {
        Self {
            paths: path.map(std::env::split_paths).map(Iterator::collect).unwrap_or_default(),
        }
    }

    fn resolve(&self, name: &str) -> CommandResolution {
        if shell_builtin_names().contains(&name) {
            return CommandResolution::Builtin;
        }
        let candidate = std::path::Path::new(name);
        if candidate.components().count() > 1 {
            return if is_executable_file(candidate) { CommandResolution::Resolved } else { CommandResolution::Unresolved };
        }
        if self.paths.iter().map(|dir| dir.join(name)).any(|path| is_executable_file(&path)) {
            CommandResolution::Resolved
        } else {
            CommandResolution::Unresolved
        }
    }
}

fn shell_builtin_names() -> &'static [&'static str] {
    &[
        "cd", "pwd", "dirs", "pushd", "popd", "alias", "unalias", "export", "unset",
        "source", ".", "reload", "exit", "logout", "exec", "history", "jobs", "fg", "bg",
        "wait", "disown", "kill", "which", "type", "command", "builtin", "hash", "echo",
        "printf", "read", "umask", "ulimit", "help", "path", "deactivate",
    ]
}

fn is_executable_file(path: &std::path::Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else { return false; };
    if !metadata.is_file() { return false; }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn raw_token_from_bytes(source: &str, start: usize, end: usize, token_type: u32, modifiers: u32) -> Option<RawToken> {
    if end <= start || start >= source.len() { return None; }
    let end = end.min(source.len());
    let (line, start_char) = byte_to_lsp_pos(source, start);
    let (_, end_char) = byte_to_lsp_pos(source, end);
    Some(RawToken {
        line,
        start_char,
        length: end_char.saturating_sub(start_char).max(1),
        token_type,
        modifiers,
    })
}

fn shell_word_expr_spans(word: &spar::ast::ShellWord) -> Vec<Span> {
    let mut spans = word.parts.iter().filter_map(|part| match part {
        spar::ast::ShellWordPart::Expr(expr) => Some(expr_span(expr).clone()),
        spar::ast::ShellWordPart::CommandSubstitution(shell) => Some(shell.span.clone()),
        _ => None,
    }).collect::<Vec<_>>();
    spans.sort_by_key(|span| span.start);
    spans
}

fn push_interpolation_delimiters(source: &str, span: &Span, out: &mut Vec<RawToken>) {
    if span.start >= 2 && source.get(span.start - 2..span.start) == Some("${") {
        if let Some(token) = raw_token_from_bytes(source, span.start - 2, span.start, TT_SHELL_INTERPOLATION, MOD_NONE) {
            out.push(token);
        }
        if source.get(span.end..span.end + 1) == Some("}") {
            if let Some(token) = raw_token_from_bytes(source, span.end, span.end + 1, TT_SHELL_INTERPOLATION, MOD_NONE) {
                out.push(token);
            }
        }
    }
}

fn collect_shell_word_tokens(
    word: &spar::ast::ShellWord,
    source: &str,
    kinds: &SemanticKinds,
    token_type: u32,
    modifiers: u32,
    out: &mut Vec<RawToken>,
) {
    let spans = shell_word_expr_spans(word);
    if spans.is_empty() {
        if let Some(token) = raw_token_from_bytes(source, word.span.start, word.span.end, token_type, modifiers) {
            out.push(token);
        }
    } else {
        let mut cursor = word.span.start;
        for span in &spans {
            let delimiter_start = if span.start >= 2 && source.get(span.start - 2..span.start) == Some("${") {
                span.start - 2
            } else {
                span.start
            };
            if cursor < delimiter_start {
                if let Some(token) = raw_token_from_bytes(source, cursor, delimiter_start, token_type, modifiers) {
                    out.push(token);
                }
            }
            cursor = span.end;
            if source.get(cursor..cursor + 1) == Some("}") { cursor += 1; }
        }
        if cursor < word.span.end {
            if let Some(token) = raw_token_from_bytes(source, cursor, word.span.end, token_type, modifiers) {
                out.push(token);
            }
        }
    }

    for part in &word.parts {
        match part {
            spar::ast::ShellWordPart::Expr(expr) => {
                push_interpolation_delimiters(source, expr_span(expr), out);
                collect_expr_tokens(expr, source, kinds, out);
            }
            spar::ast::ShellWordPart::CommandSubstitution(shell) => {
                collect_shell_semantic_tokens_with_resolver(shell, source, kinds, &CommandResolver::with_path(std::env::var_os("PATH").as_deref()), out);
            }
            spar::ast::ShellWordPart::Literal(_) | spar::ast::ShellWordPart::Environment(_) => {}
        }
    }
}

fn redirect_operator_range(source: &str, redirect: &spar::ast::ShellRedirect) -> Option<(usize, usize)> {
    let start = redirect.span.start.min(source.len());
    let end = redirect.target.span.start.min(source.len());
    (end > start).then_some((start, end))
}

fn command_bounds(command: &spar::ast::ShellCommandExpr) -> (usize, usize) {
    (command.span.start, command.span.end)
}

fn collect_shell_command_tokens(
    command: &spar::ast::ShellCommandExpr,
    source: &str,
    kinds: &SemanticKinds,
    resolver: &CommandResolver,
    out: &mut Vec<RawToken>,
) {
    for env in &command.environment {
        if let Some(token) = raw_token_from_bytes(source, env.span.start, env.span.end, TT_SHELL_ENVIRONMENT, MOD_NONE) {
            out.push(token);
        }
    }

    let resolution = resolver.resolve(&command.program.text);
    let (program_type, program_modifiers) = match resolution {
        CommandResolution::Builtin => (TT_SHELL_BUILTIN, MOD_DEFAULT_LIBRARY | MOD_RESOLVED),
        CommandResolution::Resolved => (TT_SHELL_COMMAND, MOD_RESOLVED),
        CommandResolution::Unresolved => (TT_SHELL_COMMAND, MOD_UNRESOLVED),
    };
    collect_shell_word_tokens(&command.program, source, kinds, program_type, program_modifiers, out);

    for arg in &command.args {
        let token_type = if arg.text.starts_with('-') { TT_SHELL_FLAG } else { TT_SHELL_ARGUMENT };
        collect_shell_word_tokens(arg, source, kinds, token_type, MOD_NONE, out);
    }

    for redirect in [&command.stdin, &command.stdout, &command.stderr].into_iter().flatten() {
        if let Some((start, end)) = redirect_operator_range(source, redirect) {
            if let Some(token) = raw_token_from_bytes(source, start, end, TT_SHELL_REDIRECT, MOD_NONE) {
                out.push(token);
            }
        }
        collect_shell_word_tokens(&redirect.target, source, kinds, TT_SHELL_ARGUMENT, MOD_NONE, out);
    }
    for redirect in &command.redirections {
        if let Some(token) = raw_token_from_bytes(source, redirect.span.start, redirect.span.end, TT_SHELL_REDIRECT, MOD_NONE) {
            out.push(token);
        }
    }
}

fn collect_operator_between(source: &str, start: usize, end: usize, out: &mut Vec<RawToken>) {
    if end <= start || end > source.len() { return; }
    let slice = &source[start..end];
    for operator in ["&&", "||", "|", ";", "&"] {
        if let Some(relative) = slice.find(operator) {
            if let Some(token) = raw_token_from_bytes(source, start + relative, start + relative + operator.len(), TT_SHELL_OPERATOR, MOD_NONE) {
                out.push(token);
            }
            return;
        }
    }
}

fn collect_shell_semantic_tokens_with_resolver(
    shell: &spar::ast::ShellExpr,
    source: &str,
    kinds: &SemanticKinds,
    resolver: &CommandResolver,
    out: &mut Vec<RawToken>,
) {
    collect_stmts_tokens(&shell.statements, source, kinds, out);
    let mut previous_end = None;
    for (_, step) in &shell.steps {
        let commands: Vec<&spar::ast::ShellCommandExpr> = match step {
            spar::ast::ShellStep::Command(command) => vec![command.as_ref()],
            spar::ast::ShellStep::Pipeline(commands) => commands.iter().collect(),
        };
        for command in commands {
            let (start, end) = command_bounds(command);
            if let Some(previous) = previous_end {
                collect_operator_between(source, previous, start, out);
            }
            collect_shell_command_tokens(command, source, kinds, resolver, out);
            previous_end = Some(end);
        }
    }
}

fn collect_shell_semantic_tokens(
    shell: &spar::ast::ShellExpr,
    source: &str,
    kinds: &SemanticKinds,
    out: &mut Vec<RawToken>,
) {
    // `shell bash { ... }` (and future foreign shells) is intentionally not
    // interpreted as Spar's native command grammar. Thin editor clients can
    // embed the matching foreign grammar without spar-ls emitting misleading
    // command/flag classifications for syntax it does not own.
    if shell.foreign_shell.is_some() {
        return;
    }
    let path = std::env::var_os("PATH");
    let resolver = CommandResolver::with_path(path.as_deref());
    collect_shell_semantic_tokens_with_resolver(shell, source, kinds, &resolver, out);
}

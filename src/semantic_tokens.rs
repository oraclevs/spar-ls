// ── Semantic token infrastructure ────────────────────────────────────────────

const TOKEN_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::VARIABLE,  // 0
    SemanticTokenType::FUNCTION,  // 1
    SemanticTokenType::PARAMETER, // 2
    SemanticTokenType::PROPERTY,  // 3
    SemanticTokenType::NAMESPACE, // 4
    SemanticTokenType::TYPE,      // 5
    SemanticTokenType::new("section"), // 6
    SemanticTokenType::new("task"), // 7
    SemanticTokenType::new("taskField"), // 8
    SemanticTokenType::KEYWORD,   // 9
    SemanticTokenType::ENUM,      // 10
    SemanticTokenType::new("functionGroup"), // 11
    SemanticTokenType::new("shellCommand"), // 12
    SemanticTokenType::new("shellBuiltin"), // 13
    SemanticTokenType::new("shellArgument"), // 14
    SemanticTokenType::new("shellFlag"), // 15
    SemanticTokenType::new("shellOperator"), // 16
    SemanticTokenType::new("shellRedirect"), // 17
    SemanticTokenType::new("shellEnvironment"), // 18
    SemanticTokenType::new("shellInterpolation"), // 19
    SemanticTokenType::TYPE_PARAMETER, // 20
    SemanticTokenType::new("declarationKeyword"), // 21
    SemanticTokenType::new("structuredPipe"), // 22
];

const TOKEN_MODIFIERS: &[SemanticTokenModifier] = &[
    SemanticTokenModifier::DECLARATION, // bit 0 = 1
    SemanticTokenModifier::new("resolved"), // bit 1 = 2
    SemanticTokenModifier::new("unresolved"), // bit 2 = 4
    SemanticTokenModifier::new("defaultLibrary"), // bit 3 = 8
];

const TT_VARIABLE: u32 = 0;
const TT_FUNCTION: u32 = 1;
const TT_PARAMETER: u32 = 2;
const TT_PROPERTY: u32 = 3;
const TT_NAMESPACE: u32 = 4;
const TT_TYPE: u32 = 5;
const TT_SECTION: u32 = 6;
const TT_TASK: u32 = 7;
const TT_TASK_FIELD: u32 = 8;
const TT_KEYWORD: u32 = 9;
const TT_ENUM: u32 = 10;
const TT_FUNCTION_GROUP: u32 = 11;
const TT_SHELL_COMMAND: u32 = 12;
const TT_SHELL_BUILTIN: u32 = 13;
const TT_SHELL_ARGUMENT: u32 = 14;
const TT_SHELL_FLAG: u32 = 15;
const TT_SHELL_OPERATOR: u32 = 16;
const TT_SHELL_REDIRECT: u32 = 17;
const TT_SHELL_ENVIRONMENT: u32 = 18;
const TT_SHELL_INTERPOLATION: u32 = 19;
const TT_TYPE_PARAMETER: u32 = 20;
const TT_DECLARATION_KEYWORD: u32 = 21;
const TT_STRUCTURED_PIPE: u32 = 22;
const MOD_NONE: u32 = 0;
const MOD_DECLARATION: u32 = 1;
const MOD_RESOLVED: u32 = 1 << 1;
const MOD_UNRESOLVED: u32 = 1 << 2;
const MOD_DEFAULT_LIBRARY: u32 = 1 << 3;

#[derive(Debug, Clone, PartialEq)]
struct RawToken {
    line: u32,
    start_char: u32,
    length: u32,
    token_type: u32,
    modifiers: u32,
}

fn is_shell_semantic_type(token_type: u32) -> bool {
    matches!(
        token_type,
        TT_SHELL_COMMAND
            | TT_SHELL_BUILTIN
            | TT_SHELL_ARGUMENT
            | TT_SHELL_FLAG
            | TT_SHELL_OPERATOR
            | TT_SHELL_REDIRECT
            | TT_SHELL_ENVIRONMENT
            | TT_SHELL_INTERPOLATION
    )
}

fn is_symbolic_semantic_type(token_type: u32) -> bool {
    token_type == TT_STRUCTURED_PIPE || is_shell_semantic_type(token_type)
}

/// Drop tokens that cannot be valid for `source`: zero-length, out of range,
/// not identifier-like (for non-shell types), or overlapping an earlier token.
/// Spliced std items keep spans from other files and produce such tokens.
fn sanitize_raw_tokens(source: &str, raw: Vec<RawToken>) -> Vec<RawToken> {
    let lines: Vec<&str> = source.split('\n').collect();
    let mut valid: Vec<RawToken> = raw
        .into_iter()
        .filter(|token| {
            if token.length == 0 {
                return false;
            }
            let Some(line) = lines.get(token.line as usize) else {
                return false;
            };
            let line = line.strip_suffix('\r').unwrap_or(line);
            let start = lsp_pos_to_byte_offset(
                line,
                Position::new(0, token.start_char),
            );
            let end = lsp_pos_to_byte_offset(
                line,
                Position::new(0, token.start_char + token.length),
            );
            if start >= end || end > line.len() {
                return false;
            }
            let text = &line[start..end];
            if text.encode_utf16().count() != token.length as usize {
                return false;
            }
            if is_symbolic_semantic_type(token.token_type) {
                return !text.trim().is_empty();
            }
            text.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        })
        .collect();
    // Stable sort: earlier-pushed (AST) tokens win over language-word tokens at
    // the same start; longer tokens win over shorter ones starting together.
    valid.sort_by_key(|token| (token.line, token.start_char, std::cmp::Reverse(token.length)));
    let mut out: Vec<RawToken> = Vec::with_capacity(valid.len());
    for token in valid {
        if let Some(previous) = out.last() {
            if previous.line == token.line && token.start_char < previous.start_char + previous.length {
                continue;
            }
        }
        out.push(token);
    }
    out
}

/// Blank (with spaces, keeping every byte offset and newline) the statement that
/// contains byte `at`: from just after the previous `;`, `{` or `}` through the next
/// `;` (inclusive) or up to the next `}`.
/// How many broken statements `repair_source` will blank before giving up; a
/// task-runner file can have one bad line per task.
const REPAIR_ATTEMPTS: usize = 48;

fn blank_statement_around(text: &str, at: usize) -> String {
    let bytes = text.as_bytes();
    let at = at.min(bytes.len());
    let mut start = at;
    while start > 0 && !matches!(bytes[start - 1], b';' | b'{' | b'}') {
        start -= 1;
    }
    let mut end = at;
    while end < bytes.len() && !matches!(bytes[end], b';' | b'}') {
        end += 1;
    }
    if end < bytes.len() && bytes[end] == b';' {
        end += 1;
    }
    let mut out = String::with_capacity(text.len());
    let mut index = 0usize;
    for ch in text.chars() {
        let width = ch.len_utf8();
        if index >= start && index < end && ch != '\n' {
            out.extend(std::iter::repeat_n(' ', width));
        } else {
            out.push(ch);
        }
        index += width;
    }
    out
}

/// Parse `source`; if it does not parse (a line is mid-edit), blank the statement the
/// parser complains about and try again, a few times. Offsets never move, so the
/// resulting AST can be used against the original text: one broken statement leaves
/// the rest of the file fully highlighted.
fn parse_with_statement_repair(source: &str) -> Option<Program> {
    let text = repair_source(source)?;
    let tokens = Lexer::new(&text).tokenize().ok()?;
    Parser::new(tokens).parse().ok()
}

/// The text of `source` after blanking, in place, each statement the lexer or
/// parser rejects, until the rest lexes and parses. `None` when it cannot be
/// made to parse. Offsets never move.
pub(crate) fn repair_source(source: &str) -> Option<String> {
    let mut text = source.to_string();
    for _ in 0..REPAIR_ATTEMPTS {
        let failure = match Lexer::new(&text).tokenize() {
            Ok(tokens) => match Parser::new(tokens).parse() {
                Ok(_) => return Some(text),
                Err(SparError::ParseError { span, .. }) => (false, span.start),
                Err(SparError::LexError { span, .. }) => (true, span.start),
                Err(_) => return None,
            },
            Err(SparError::LexError { span, .. }) => (true, span.start),
            Err(SparError::ParseError { span, .. }) => (false, span.start),
            Err(_) => return None,
        };
        // A lexical error can sit inside a string or a `#{...}` escape, where
        // statement boundaries (`;`, `{`, `}`) are unreliable: blank the whole
        // physical line. A parse error blanks the statement around it.
        let repaired = if failure.0 {
            blank_line_around(&text, failure.1)
        } else {
            blank_statement_around(&text, failure.1)
        };
        if repaired == text {
            return None;
        }
        text = repaired;
    }
    None
}

/// Replaces the physical line containing byte offset `at` with spaces.
fn blank_line_around(text: &str, at: usize) -> String {
    let at = at.min(text.len());
    let start = text[..at].rfind('\n').map_or(0, |index| index + 1);
    let end = text[at..].find('\n').map_or(text.len(), |index| at + index);
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..start]);
    out.extend(text[start..end].chars().map(|_| ' '));
    out.push_str(&text[end..]);
    out
}

fn build_semantic_raw_tokens(state: &DocumentState) -> Vec<RawToken> {
    let mut raw = Vec::new();
    // Build tokens from this file's OWN parse. The compiled AST has imported items
    // spliced in with their original files' byte positions, which would paint tokens
    // onto unrelated lines here. It is used only for meaning: which imported names
    // are functions vs types, and which functions are bundled std.
    match parse_with_statement_repair(&state.source) {
        Some(own) => {
            let kinds = SemanticKinds::from_program(state.ast.as_ref().unwrap_or(&own));
            collect_tokens_with_kinds(&own, &kinds, state.ast.as_ref(), &state.source, &mut raw);
        }
        // The file does not parse: keep keywords/types colored from the lexer.
        None => collect_language_words(&state.source, &mut raw),
    }
    sanitize_raw_tokens(&state.source, raw)
}

fn encode_semantic_tokens(raw: &[RawToken]) -> Vec<SemanticToken> {
    let mut data = Vec::with_capacity(raw.len());
    let mut previous_line = 0u32;
    let mut previous_char = 0u32;
    for token in raw {
        let delta_line = token.line - previous_line;
        let delta_start = if delta_line == 0 {
            token.start_char - previous_char
        } else {
            token.start_char
        };
        data.push(SemanticToken {
            delta_line,
            delta_start,
            length: token.length,
            token_type: token.token_type,
            token_modifiers_bitset: token.modifiers,
        });
        previous_line = token.line;
        previous_char = token.start_char;
    }
    data
}

struct SemanticKinds {
    enums: HashSet<String>,
    function_groups: HashSet<String>,
    /// Built-in and bundled-std function names, colored with `defaultLibrary`.
    library_functions: HashSet<String>,
}

const BUILTIN_FUNCTION_NAMES: &[&str] = &["env", "int", "float", "str", "bool", "len", "print", "println", "assert"];

impl SemanticKinds {
    fn from_program(program: &Program) -> Self {
        use spar::ast::TopLevelItem;
        let mut kinds = Self {
            enums: HashSet::new(),
            function_groups: HashSet::new(),
            library_functions: BUILTIN_FUNCTION_NAMES.iter().map(|name| name.to_string()).collect(),
        };
        for item in &program.items {
            match item {
                TopLevelItem::Enum(decl) => {
                    kinds.enums.insert(decl.name.clone());
                }
                TopLevelItem::FunctionGroup(decl) => {
                    kinds.function_groups.insert(decl.name.clone());
                }
                TopLevelItem::Function(decl) if decl.trusted_native => {
                    kinds.library_functions.insert(decl.name.clone());
                }
                TopLevelItem::Function(decl) => {
                    // A user function shadowing a builtin name is user code.
                    kinds.library_functions.remove(&decl.name);
                }
                _ => {}
            }
        }
        kinds
    }

    fn call_modifiers(&self, name: &str) -> u32 {
        let member = name.rsplit("::").next().unwrap_or(name);
        if self.library_functions.contains(member) { MOD_DEFAULT_LIBRARY } else { MOD_NONE }
    }

    fn named_type_token(&self, name: &str) -> u32 {
        if self.enums.contains(name) {
            TT_ENUM
        } else {
            TT_TYPE
        }
    }

    fn qualifier_token(&self, name: &str) -> Option<u32> {
        if self.enums.contains(name) {
            Some(TT_ENUM)
        } else if self.function_groups.contains(name) {
            Some(TT_FUNCTION_GROUP)
        } else {
            None
        }
    }
}

fn byte_to_lsp_pos(source: &str, byte_offset: usize) -> (u32, u32) {
    let position = byte_offset_to_lsp_position(source, byte_offset);
    (position.line, position.character)
}

fn raw_from_span(span: &Span, token_type: u32, modifiers: u32) -> RawToken {
    RawToken {
        line: span.line.saturating_sub(1),
        start_char: span.col.saturating_sub(1),
        length: (span.end.saturating_sub(span.start)) as u32,
        token_type,
        modifiers,
    }
}

/// Find `name` as a whole identifier (word-boundary) starting from `from_byte`.
/// Returns the byte offset of the match in `source`, or None.
fn find_ident_byte(source: &str, from_byte: usize, name: &str) -> Option<usize> {
    if from_byte >= source.len() {
        return None;
    }
    let haystack = &source[from_byte..];
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut search = 0usize;
    while search < haystack.len() {
        let rel = haystack[search..].find(name)?;
        let abs_rel = search + rel;
        let before_ok = abs_rel == 0
            || !haystack[..abs_rel]
                .chars()
                .last()
                .map(is_ident)
                .unwrap_or(false);
        let after_pos = abs_rel + name.len();
        let after_ok = after_pos >= haystack.len()
            || !haystack[after_pos..]
                .chars()
                .next()
                .map(is_ident)
                .unwrap_or(false);
        if before_ok && after_ok {
            return Some(from_byte + abs_rel);
        }
        search = abs_rel + 1;
    }
    None
}

fn find_ident_token(
    source: &str,
    from_byte: usize,
    name: &str,
    token_type: u32,
    modifiers: u32,
) -> Option<RawToken> {
    let byte_off = find_ident_byte(source, from_byte, name)?;
    let (line, col) = byte_to_lsp_pos(source, byte_off);
    Some(RawToken {
        line,
        start_char: col,
        length: name.len() as u32,
        token_type,
        modifiers,
    })
}

fn find_qualifier_token_before(
    source: &str,
    member_start: usize,
    qualifier: &str,
    token_type: u32,
) -> Option<RawToken> {
    let member_start = member_start.min(source.len());
    let line_start = source[..member_start]
        .rfind('\n')
        .map_or(0, |position| position + 1);
    let relative = source[line_start..member_start].rfind(qualifier)?;
    let byte_off = line_start + relative;
    let (line, start_char) = byte_to_lsp_pos(source, byte_off);
    Some(RawToken {
        line,
        start_char,
        length: qualifier.len() as u32,
        token_type,
        modifiers: MOD_NONE,
    })
}

fn collect_expr_tokens(
    expr: &spar::ast::Expr,
    source: &str,
    kinds: &SemanticKinds,
    out: &mut Vec<RawToken>,
) {
    use spar::ast::{Expr, StringPart};
    match expr {
        Expr::Call {
            name,
            name_span,
            args,
            ..
        } => {
            out.push(raw_from_span(name_span, TT_FUNCTION, kinds.call_modifiers(name)));
            if let Some((qualifier, _)) = name.split_once("::") {
                if let Some(token_type) = kinds.qualifier_token(qualifier) {
                    if let Some(tok) = find_qualifier_token_before(
                        source,
                        name_span.start,
                        qualifier,
                        token_type,
                    ) {
                        out.push(tok);
                    }
                }
            }
            for arg in args {
                collect_expr_tokens(&arg.value, source, kinds, out);
            }
        }
        Expr::FnCall(fc) => {
            let member = fc.name.rsplit("::").next().unwrap_or(&fc.name);
            if let Some(tok) = find_ident_token(source, fc.span.start, member, TT_FUNCTION, kinds.call_modifiers(&fc.name)) {
                out.push(tok);
            }
            if let Some((qualifier, _)) = fc.name.split_once("::") {
                if let Some(token_type) = kinds.qualifier_token(qualifier) {
                    if let Some(tok) = find_qualifier_token_before(
                        source,
                        fc.span.start,
                        qualifier,
                        token_type,
                    ) {
                        out.push(tok);
                    }
                }
            }
            for arg in &fc.args {
                collect_expr_tokens(arg, source, kinds, out);
            }
        }
        Expr::NamespaceRef(nr) if nr.segments.len() == 1 => {
            if let Some(tok) = find_ident_token(
                source,
                nr.span.start,
                &nr.segments[0],
                TT_VARIABLE,
                MOD_NONE,
            ) {
                out.push(tok);
            }
        }
        Expr::NamespaceRef(nr) => {
            if let Some(qualifier) = nr.segments.first() {
                if let Some(token_type) = kinds.qualifier_token(qualifier) {
                    if let Some(tok) = find_ident_token(
                        source,
                        nr.span.start,
                        qualifier,
                        token_type,
                        MOD_NONE,
                    ) {
                        out.push(tok);
                    }
                }
            }
        }
        Expr::BinaryOp(b) => {
            collect_expr_tokens(&b.lhs, source, kinds, out);
            collect_expr_tokens(&b.rhs, source, kinds, out);
        }
        Expr::Unary { operand, .. } => collect_expr_tokens(operand, source, kinds, out),
        Expr::Await { value, .. } => collect_expr_tokens(value, source, kinds, out),
        Expr::List(items, _) => {
            for item in items {
                collect_expr_tokens(item, source, kinds, out);
            }
        }
        Expr::Grouped(inner, _) => collect_expr_tokens(inner, source, kinds, out),
        Expr::Comprehension {
            var_name_span,
            source: comp_src,
            body,
            ..
        } => {
            out.push(raw_from_span(var_name_span, TT_VARIABLE, MOD_DECLARATION));
            collect_expr_tokens(comp_src, source, kinds, out);
            collect_expr_tokens(body, source, kinds, out);
        }
        Expr::Index {
            source: src_expr,
            index,
            ..
        } => {
            collect_expr_tokens(src_expr, source, kinds, out);
            collect_expr_tokens(index, source, kinds, out);
        }
        Expr::String(s) => {
            for part in &s.parts {
                if let StringPart::Expr(e) = part {
                    collect_expr_tokens(e, source, kinds, out);
                }
            }
        }
        Expr::Object(items, _) => {
            use spar::ast::{FieldValue, SectionItem};
            // Minimal, structurally-correct recursion — no new semantic-token
            // classification for object-literal field names here; that's
            // deferred LSP/highlighter work, tracked separately.
            for item in items {
                match item {
                    SectionItem::Field(f) => {
                        if let Some(FieldValue::Expr(e)) = &f.value {
                            collect_expr_tokens(e, source, kinds, out);
                        }
                    }
                    SectionItem::Spread(sp) => collect_expr_tokens(&sp.expr, source, kinds, out),
                }
            }
        }
        Expr::FieldAccess {
            base, field_span, ..
        } => {
            collect_expr_tokens(base, source, kinds, out);
            out.push(raw_from_span(field_span, TT_PROPERTY, MOD_NONE));
        }
        Expr::Closure { params, return_type, body, span } => {
            for param in params {
                if let Some(token) = find_ident_token(
                    source,
                    param.span.start,
                    &param.name,
                    TT_PARAMETER,
                    MOD_DECLARATION,
                ) {
                    out.push(token);
                }
                if let Some(ty) = &param.ty {
                    collect_named_type_token(ty, source, param.span.start, kinds, out);
                }
            }
            if let Some(ret) = return_type {
                collect_named_type_token(ret, source, span.start, kinds, out);
            }
            match body {
                spar::ast::ClosureBody::Expr(body) => collect_expr_tokens(body, source, kinds, out),
                spar::ast::ClosureBody::Block(body) => {
                    collect_stmts_tokens(&body.stmts, source, kinds, out)
                }
            }
        }
        Expr::MethodCall { receiver, method_span, args, .. } => {
            collect_expr_tokens(receiver, source, kinds, out);
            out.push(raw_from_span(method_span, TT_FUNCTION, MOD_NONE));
            for arg in args {
                collect_expr_tokens(arg, source, kinds, out);
            }
        }
        Expr::StructuredPipe { input, stage, span } => {
            collect_expr_tokens(input, source, kinds, out);

            // The operator is part of the typed Spar language, not a shell
            // operator. Tag the outermost `|>` in this expression separately
            // so editors can distinguish structured flow from Unix `|`.
            let start = span.start.min(source.len());
            let end = span.end.min(source.len());
            if start < end {
                if let Some(relative) = source[start..end].rfind("|>") {
                    let byte = start + relative;
                    let (line, start_char) = byte_to_lsp_pos(source, byte);
                    out.push(RawToken {
                        line,
                        start_char,
                        length: 2,
                        token_type: TT_STRUCTURED_PIPE,
                        modifiers: MOD_NONE,
                    });
                }
            }

            // A bare callable value on the RHS is a pipeline stage, so present
            // it as a function in this context instead of a plain variable.
            if let Expr::NamespaceRef(reference) = stage.as_ref() {
                if reference.segments.len() == 1 {
                    if let Some(token) = find_ident_token(
                        source,
                        reference.span.start,
                        &reference.segments[0],
                        TT_FUNCTION,
                        kinds.call_modifiers(&reference.segments[0]),
                    ) {
                        out.push(token);
                    }
                } else {
                    collect_expr_tokens(stage, source, kinds, out);
                }
            } else {
                collect_expr_tokens(stage, source, kinds, out);
            }
        }
        Expr::Shell(shell) | Expr::ExecShell(shell) => {
            let keyword = source
                .get(shell.span.start..)
                .filter(|tail| tail.starts_with("command"))
                .map_or("shell", |_| "command");
            if let Some(token) =
                find_ident_token(source, shell.span.start, keyword, TT_KEYWORD, MOD_NONE)
            {
                out.push(token);
            }
            collect_shell_semantic_tokens(shell, source, kinds, out);
        }
        Expr::CommandSubstitution(shell) => {
            collect_shell_semantic_tokens(shell, source, kinds, out);
        }
        Expr::Literal(_) => {}
    }
}

fn collect_stmts_tokens(
    stmts: &[FuncStmt],
    source: &str,
    kinds: &SemanticKinds,
    out: &mut Vec<RawToken>,
) {
    use spar::ast::{FuncStmt as FS, ReturnValue};
    for stmt in stmts {
        match stmt {
            FS::LocalVar(lv) => {
                if let Some(tok) = find_ident_token(
                    source,
                    lv.span.start,
                    &lv.name,
                    TT_VARIABLE,
                    MOD_DECLARATION,
                ) {
                    out.push(tok);
                }
                collect_expr_tokens(&lv.value, source, kinds, out);
            }
            FS::Assignment { name, value, span } => {
                if let Some(token) = find_ident_token(source, span.start, name, TT_VARIABLE, MOD_NONE)
                {
                    out.push(token);
                }
                collect_expr_tokens(value, source, kinds, out);
            }
            FS::FieldAssignment { base, fields, value, span } => {
                if let Some(token) =
                    find_ident_token(source, span.start, base, TT_VARIABLE, MOD_NONE)
                {
                    out.push(token);
                }
                let mut search_from = span.start;
                for field in fields {
                    if let Some(byte) = find_ident_byte(source, search_from, field) {
                        let (line, col) = byte_to_lsp_pos(source, byte);
                        out.push(RawToken {
                            line,
                            start_char: col,
                            length: field.len() as u32,
                            token_type: TT_PROPERTY,
                            modifiers: MOD_NONE,
                        });
                        search_from = byte + field.len();
                    }
                }
                collect_expr_tokens(value, source, kinds, out);
            }
            FS::Expression(expression, _) => collect_expr_tokens(expression, source, kinds, out),
            FS::Break(_) | FS::Continue(_) => {}
            FS::Return(rv, _) => match rv {
                ReturnValue::Void => {}
                ReturnValue::Expr(e) => collect_expr_tokens(e, source, kinds, out),
                ReturnValue::SectionBlock(fields) => {
                    for rf in fields {
                        collect_expr_tokens(&rf.value, source, kinds, out);
                    }
                }
            },
            FS::If(if_stmt) => {
                collect_expr_tokens(&if_stmt.condition, source, kinds, out);
                collect_stmts_tokens(&if_stmt.then_stmts, source, kinds, out);
                collect_stmts_tokens(&if_stmt.else_stmts, source, kinds, out);
            }
            FS::For(statement) => {
                match &statement.binding {
                    spar::ast::ForBinding::Value { name, span } => {
                        if let Some(token) = find_ident_token(
                            source,
                            span.start,
                            name,
                            TT_VARIABLE,
                            MOD_DECLARATION,
                        ) {
                            out.push(token);
                        }
                    }
                    spar::ast::ForBinding::Indexed {
                        index_name,
                        index_span,
                        value_name,
                        value_span,
                    } => {
                        out.push(raw_from_span(index_span, TT_VARIABLE, MOD_DECLARATION));
                        out.push(raw_from_span(value_span, TT_VARIABLE, MOD_DECLARATION));
                        let _ = (index_name, value_name);
                    }
                }
                collect_expr_tokens(&statement.iterable, source, kinds, out);
                collect_stmts_tokens(&statement.body, source, kinds, out);
            }
            FS::Try(statement) => {
                collect_stmts_tokens(&statement.body, source, kinds, out);
                if let Some(name) = &statement.catch_name {
                    if let Some(token) = find_ident_token(
                        source,
                        statement.catch_span.start,
                        name,
                        TT_VARIABLE,
                        MOD_DECLARATION,
                    ) {
                        out.push(token);
                    }
                }
                collect_stmts_tokens(&statement.handler, source, kinds, out);
            }
        }
    }
}

fn collect_section_items_tokens(
    items: &[spar::ast::SectionItem],
    source: &str,
    kinds: &SemanticKinds,
    out: &mut Vec<RawToken>,
) {
    use spar::ast::{FieldValue, SectionItem};
    for item in items {
        match item {
            SectionItem::Field(fd) => {
                if let Some(tok) = find_ident_token(
                    source,
                    fd.span.start,
                    &fd.name,
                    TT_PROPERTY,
                    MOD_DECLARATION,
                ) {
                    out.push(tok);
                }
                if let Some(ty) = &fd.ty {
                    collect_named_type_token(ty, source, fd.span.start, kinds, out);
                }
                match &fd.value {
                    Some(FieldValue::Expr(e)) => collect_expr_tokens(e, source, kinds, out),
                    Some(FieldValue::Nested(nested)) => {
                        collect_section_items_tokens(nested, source, kinds, out)
                    }
                    None => {}
                }
            }
            SectionItem::Spread(ss) => collect_expr_tokens(&ss.expr, source, kinds, out),
        }
    }
}

fn collect_named_type_token(
    ty: &SparType,
    source: &str,
    from_byte: usize,
    kinds: &SemanticKinds,
    out: &mut Vec<RawToken>,
) {
    collect_type_tokens_from(ty, source, from_byte, kinds, out);
}

/// Emit tokens for every user-facing identifier in `ty`, scanning forward so a
/// repeated name (`Map<Thing, Thing>`) resolves to successive occurrences.
/// Returns the byte offset just after the last identifier consumed.
fn collect_type_tokens_from(
    ty: &SparType,
    source: &str,
    from_byte: usize,
    kinds: &SemanticKinds,
    out: &mut Vec<RawToken>,
) -> usize {
    const BUILTIN_GENERICS: &[&str] = &["List", "Map", "Promise"];
    fn emit(
        source: &str,
        name: &str,
        token_type: u32,
        modifiers: u32,
        from: usize,
        out: &mut Vec<RawToken>,
    ) -> usize {
        match find_ident_byte(source, from, name) {
            Some(byte) => {
                if let Some(token) =
                    raw_token_from_bytes(source, byte, byte + name.len(), token_type, modifiers)
                {
                    out.push(token);
                }
                byte + name.len()
            }
            None => from,
        }
    }
    match ty {
        SparType::Named(name) => emit(source, name, kinds.named_type_token(name), MOD_NONE, from_byte, out),
        SparType::TypeParameter(name) => emit(source, name, TT_TYPE_PARAMETER, MOD_NONE, from_byte, out),
        SparType::List(inner) => collect_type_tokens_from(inner, source, from_byte, kinds, out),
        SparType::Applied { name, arguments } => {
            let modifiers = if BUILTIN_GENERICS.contains(&name.as_str()) { MOD_DEFAULT_LIBRARY } else { MOD_NONE };
            let mut cursor = emit(source, name, kinds.named_type_token(name), modifiers, from_byte, out);
            for argument in arguments {
                cursor = collect_type_tokens_from(argument, source, cursor, kinds, out);
            }
            cursor
        }
        SparType::Function { params, return_type } => {
            let mut cursor = from_byte;
            for param in params {
                cursor = collect_type_tokens_from(param, source, cursor, kinds, out);
            }
            collect_type_tokens_from(return_type, source, cursor, kinds, out)
        }
        _ => from_byte,
    }
}

fn find_task_field_token(
    source: &str,
    body_start: usize,
    body_end: usize,
    name: &str,
) -> Option<RawToken> {
    let body = source.get(body_start..body_end)?;
    let offset = body.match_indices(name).find_map(|(offset, _)| {
        let before = &body[..offset];
        let line_prefix = before.rsplit_once('\n').map_or(before, |(_, line)| line);
        let prefix = line_prefix.trim_end();
        let after = &body[offset + name.len()..];
        ((prefix.is_empty() || prefix.ends_with('{') || prefix.ends_with(';'))
            && after
                .trim_start()
                .starts_with(if name == "run" { '{' } else { ':' }))
        .then_some(offset)
    })?;
    let (line, start_char) = byte_to_lsp_pos(source, body_start + offset);
    Some(RawToken {
        line,
        start_char,
        length: name.len() as u32,
        token_type: TT_TASK_FIELD,
        modifiers: MOD_DECLARATION,
    })
}

fn collect_task_tokens(
    program: &Program,
    index: usize,
    task: &spar::ast::TaskDecl,
    source: &str,
    kinds: &SemanticKinds,
    out: &mut Vec<RawToken>,
) {
    use spar::ast::ShellTemplatePart;

    out.push(raw_from_span(&task.name_span, TT_TASK, MOD_DECLARATION));
    for param in &task.params {
        if let Some(token) = find_ident_token(
            source,
            param.span.start,
            &param.name,
            TT_PARAMETER,
            MOD_DECLARATION,
        ) {
            out.push(token);
        }
        collect_named_type_token(&param.ty, source, param.span.start, kinds, out);
        if let Some(default) = &param.default {
            collect_expr_tokens(default, source, kinds, out);
        }
    }

    let expressions = [
        ("description", task.description.as_ref()),
        ("default", task.default.as_ref()),
        ("quiet", task.quiet.as_ref()),
        ("private", task.private.as_ref()),
        ("group", task.group.as_ref()),
        ("confirm", task.confirm.as_ref()),
        ("cwd", task.cwd.as_ref()),
    ];
    let Some((body_start, body_end)) = task_body_bounds(program, source, index, task) else {
        return;
    };
    // Task metadata is its own visual category, distinct from section properties.
    for (name, expression) in expressions {
        if let Some(expression) = expression {
            if let Some(token) = find_task_field_token(source, body_start, body_end, name) {
                out.push(token);
            }
            collect_expr_tokens(expression, source, kinds, out);
        }
    }
    for name in ["dependsOn", "env"] {
        if let Some(token) = find_task_field_token(source, body_start, body_end, name) {
            out.push(token);
        }
    }
    for dependency in &task.depends_on {
        out.push(raw_from_span(&dependency.span, TT_TASK, MOD_NONE));
    }
    let mut env_search = find_ident_byte(source, body_start, "env")
        .map(|start| start + "env".len())
        .unwrap_or(body_start);
    for (name, value) in &task.env {
        if let Some(token) = find_task_field_token(source, env_search, body_end, name) {
            if let Some(start) = find_ident_byte(source, env_search, name) {
                env_search = start + name.len();
            }
            out.push(RawToken {
                token_type: TT_PROPERTY,
                ..token
            });
        }
        collect_expr_tokens(value, source, kinds, out);
    }
    for block in &task.run_blocks {
        // `run` itself is a task field; the optional shell/OS words after it
        // are keywords.
        out.push(raw_from_span(&block.span, TT_TASK_FIELD, MOD_DECLARATION));
        for word in [&block.shell_span, &block.os_span].into_iter().flatten() {
            out.push(raw_from_span(word, TT_KEYWORD, MOD_NONE));
        }
        let native;
        let mut interpolations: Vec<&spar::ast::Expr> = Vec::new();
        match &block.body {
            spar::ast::RunBody::Bash(commands) => {
                for command in commands {
                    for part in &command.parts {
                        if let ShellTemplatePart::Expr(expression) = part {
                            interpolations.push(expression);
                        }
                    }
                }
            }
            spar::ast::RunBody::Native(shell) => {
                native = spar::ast::Expr::Shell(shell.clone());
                interpolations.push(&native);
            }
        }
        for expression in interpolations {
            let token_start = out.len();
            collect_expr_tokens(expression, source, kinds, out);
            for token in &mut out[token_start..] {
                if token.token_type != TT_VARIABLE {
                    continue;
                }
                let line = source.lines().nth(token.line as usize).unwrap_or_default();
                let start = token.start_char as usize;
                let end = start + token.length as usize;
                if line
                    .get(start..end)
                    .is_some_and(|name| task.params.iter().any(|param| param.name == name))
                {
                    token.token_type = TT_PARAMETER;
                }
            }
        }
    }
}

#[cfg(test)]
fn collect_tokens_from_program(program: &Program, source: &str, out: &mut Vec<RawToken>) {
    let kinds = SemanticKinds::from_program(program);
    collect_tokens_with_kinds(program, &kinds, None, source, out);
}

/// Classify the names of a selective import (`import { a, b as c } from ...`).
/// `resolved` is the compiled program (imports spliced in) used to tell functions
/// from types; when it is unavailable, PascalCase names are assumed to be types.
fn collect_import_item_tokens(
    decl: &spar::ast::ImportDecl,
    resolved: Option<&Program>,
    source: &str,
    out: &mut Vec<RawToken>,
) {
    use spar::ast::{ImportKind, TopLevelItem};
    let (ImportKind::Selective(items) | ImportKind::TypeSelective(items)) = &decl.kind else {
        return;
    };
    for item in items {
        let found = resolved.and_then(|program| {
            program.items.iter().find_map(|candidate| match candidate {
                TopLevelItem::Function(f) if f.name == item.name => Some((
                    TT_FUNCTION,
                    if f.trusted_native { MOD_DEFAULT_LIBRARY } else { MOD_NONE },
                )),
                TopLevelItem::Type(t) if t.name == item.name => Some((TT_TYPE, MOD_NONE)),
                TopLevelItem::Enum(e) if e.name == item.name => Some((TT_ENUM, MOD_NONE)),
                TopLevelItem::FunctionGroup(g) if g.name == item.name => Some((TT_FUNCTION_GROUP, MOD_NONE)),
                TopLevelItem::Var(v) if v.name == item.name => Some((TT_VARIABLE, MOD_NONE)),
                _ => None,
            })
        });
        let (token_type, modifiers) = found.unwrap_or_else(|| {
            if item.name.chars().next().is_some_and(char::is_uppercase) {
                (TT_TYPE, MOD_NONE)
            } else {
                (TT_FUNCTION, MOD_NONE)
            }
        });
        if let Some(token) = raw_token_from_bytes(source, item.name_span.start, item.name_span.end, token_type, modifiers) {
            out.push(token);
        }
        if let Some(alias) = &item.alias {
            if let Some(token) = find_ident_token(source, item.name_span.end, alias, token_type, modifiers) {
                out.push(token);
            }
        }
    }
}

fn collect_tokens_with_kinds(
    program: &Program,
    kinds: &SemanticKinds,
    resolved: Option<&Program>,
    source: &str,
    out: &mut Vec<RawToken>,
) {
    use spar::ast::TopLevelItem as TL;
    for (index, item) in program.items.iter().enumerate() {
        match item {
            TL::Var(vd) => {
                if let Some(tok) = find_ident_token(
                    source,
                    vd.span.start,
                    &vd.name,
                    TT_VARIABLE,
                    MOD_DECLARATION,
                ) {
                    out.push(tok);
                }
                collect_named_type_token(&vd.ty, source, vd.span.start, kinds, out);
                if let Some(expr) = &vd.value {
                    collect_expr_tokens(expr, source, kinds, out);
                }
            }
            TL::Dynamic(dd) => {
                if let Some(tok) = find_ident_token(
                    source,
                    dd.span.start,
                    &dd.name,
                    TT_VARIABLE,
                    MOD_DECLARATION,
                ) {
                    out.push(tok);
                }
                if let Some(expr) = &dd.value {
                    collect_expr_tokens(expr, source, kinds, out);
                }
            }
            TL::Section(sd) => {
                let mut search_from = sd.span.start;
                for seg in &sd.path {
                    if let Some(byte_off) = find_ident_byte(source, search_from, seg) {
                        let (line, col) = byte_to_lsp_pos(source, byte_off);
                        out.push(RawToken {
                            line,
                            start_char: col,
                            length: seg.len() as u32,
                            token_type: TT_SECTION,
                            modifiers: MOD_DECLARATION,
                        });
                        search_from = byte_off + seg.len();
                    }
                }
                // `[Section] -> TypeName { ... }` — TypeName gets its own token.
                if let Some(binding) = &sd.type_binding {
                    out.push(raw_from_span(&binding.span, TT_TYPE, MOD_NONE));
                }
                collect_section_items_tokens(&sd.items, source, kinds, out);
            }
            TL::Impl(imp) => {
                collect_named_type_token(&imp.target, source, imp.span.start, kinds, out);
                for type_parameter in &imp.type_parameters {
                    out.push(raw_from_span(
                        &type_parameter.span,
                        TT_TYPE_PARAMETER,
                        MOD_DECLARATION,
                    ));
                }
                for method in &imp.methods {
                    let function = &method.function;
                    out.push(raw_from_span(
                        &function.name_span,
                        TT_FUNCTION,
                        MOD_DECLARATION,
                    ));
                    if let Some(receiver) = &method.receiver {
                        if let Some(token) = find_ident_token(
                            source,
                            receiver.span.start,
                            "self",
                            TT_PARAMETER,
                            MOD_DECLARATION,
                        ) {
                            out.push(token);
                        }
                    }
                    for type_parameter in &function.type_parameters {
                        out.push(raw_from_span(
                            &type_parameter.span,
                            TT_TYPE_PARAMETER,
                            MOD_DECLARATION,
                        ));
                    }
                    for param in &function.params {
                        if let Some(token) = find_ident_token(
                            source,
                            param.span.start,
                            &param.name,
                            TT_PARAMETER,
                            MOD_DECLARATION,
                        ) {
                            out.push(token);
                        }
                        collect_named_type_token(&param.ty, source, param.span.start, kinds, out);
                    }
                    collect_named_type_token(
                        &function.ret,
                        source,
                        function.ret_span.start,
                        kinds,
                        out,
                    );
                    collect_stmts_tokens(&function.body.stmts, source, kinds, out);
                }
            }
            TL::Function(fd) => {
                let library = if fd.trusted_native { MOD_DEFAULT_LIBRARY } else { MOD_NONE };
                out.push(raw_from_span(&fd.name_span, TT_FUNCTION, MOD_DECLARATION | library));
                for type_parameter in &fd.type_parameters {
                    out.push(raw_from_span(&type_parameter.span, TT_TYPE_PARAMETER, MOD_DECLARATION));
                }
                for param in &fd.params {
                    if let Some(tok) = find_ident_token(
                        source,
                        param.span.start,
                        &param.name,
                        TT_PARAMETER,
                        MOD_DECLARATION,
                    ) {
                        out.push(tok);
                    }
                    collect_named_type_token(&param.ty, source, param.span.start, kinds, out);
                }
                collect_named_type_token(&fd.ret, source, fd.ret_span.start, kinds, out);
                collect_stmts_tokens(&fd.body.stmts, source, kinds, out);
            }
            TL::Task(td) => {
                collect_task_tokens(program, index, td, source, kinds, out);
            }
            TL::Type(td) => {
                out.push(raw_from_span(&td.name_span, TT_TYPE, MOD_DECLARATION));
                for type_parameter in &td.type_parameters {
                    out.push(raw_from_span(&type_parameter.span, TT_TYPE_PARAMETER, MOD_DECLARATION));
                }
                collect_type_fields_tokens(&td.fields, source, kinds, out);
            }
            TL::SchemaSection(sd) => {
                if let Some(tok) =
                    find_ident_token(source, sd.span.start, &sd.name, TT_TYPE, MOD_DECLARATION)
                {
                    out.push(tok);
                }
                collect_schema_fields_tokens(&sd.fields, source, out);
            }
            TL::SchemaFrom(sf) => {
                if let Some(tok) =
                    find_ident_token(source, sf.span.start, &sf.name, TT_TYPE, MOD_DECLARATION)
                {
                    out.push(tok);
                }
                out.push(raw_from_span(&sf.source_type_span, TT_TYPE, MOD_NONE));
            }
            TL::Import(decl) => collect_import_item_tokens(decl, resolved, source, out),
            TL::Enum(ed) => {
                out.push(raw_from_span(&ed.name_span, TT_ENUM, MOD_DECLARATION));
            }
            TL::FunctionGroup(gd) => {
                if let Some(tok) = find_ident_token(
                    source,
                    gd.span.start,
                    &gd.name,
                    TT_FUNCTION_GROUP,
                    MOD_DECLARATION,
                ) {
                    out.push(tok);
                }
                for f in &gd.functions {
                    out.push(raw_from_span(&f.name_span, TT_FUNCTION, MOD_DECLARATION));
                    for type_parameter in &f.type_parameters {
                        out.push(raw_from_span(&type_parameter.span, TT_TYPE_PARAMETER, MOD_DECLARATION));
                    }
                    for param in &f.params {
                        if let Some(tok) = find_ident_token(
                            source,
                            param.span.start,
                            &param.name,
                            TT_PARAMETER,
                            MOD_DECLARATION,
                        ) {
                            out.push(tok);
                        }
                        collect_named_type_token(
                            &param.ty,
                            source,
                            param.span.start,
                            kinds,
                            out,
                        );
                    }
                    collect_named_type_token(&f.ret, source, f.ret_span.start, kinds, out);
                    collect_stmts_tokens(&f.body.stmts, source, kinds, out);
                }
            }
            TL::Statement(statement) => {
                collect_stmts_tokens(std::slice::from_ref(statement), source, kinds, out);
            }
        }
    }
    collect_language_words(source, out);
}

fn collect_language_words(source: &str, out: &mut Vec<RawToken>) {
    use spar::token::Token;
    // Soft keywords the lexer emits as plain identifiers.
    const SOFT_KEYWORDS: &[&str] = &["pkg", "from", "task", "type", "schema", "functionGroup"];
    // Built-in generic/type identifiers (not keywords in the lexer).
    const BUILTIN_TYPE_IDENTS: &[&str] = &["List", "Map", "Promise"];
    // Scanning tokens (not raw text) means words inside strings, comments and
    // native shell words are never classified as keywords or types.
    let Ok(tokens) = Lexer::new(source).tokenize() else {
        return;
    };
    let mut after_hash_bracket = false;
    for spanned in &tokens {
        let (token_type, modifiers) = match &spanned.token {
            Token::HashBracket => {
                after_hash_bracket = true;
                (TT_DECLARATION_KEYWORD, MOD_NONE)
            }
            Token::Ident(_) if after_hash_bracket => {
                after_hash_bracket = false;
                (TT_DECLARATION_KEYWORD, MOD_NONE)
            }
            Token::Var | Token::KwMut | Token::Export | Token::Import | Token::As
            | Token::Dynamic | Token::Private | Token::KwAsync | Token::KwFunction
            | Token::KwStruct => (TT_DECLARATION_KEYWORD, MOD_NONE),
            Token::KwAwait | Token::KwReturn | Token::KwIf | Token::KwElse | Token::KwFor
            | Token::KwIn | Token::KwBreak | Token::KwContinue | Token::KwTry
            | Token::KwCatch | Token::KwCommand | Token::KwExec => (TT_KEYWORD, MOD_NONE),
            Token::TypeStr | Token::TypeInt | Token::TypeFloat | Token::TypeBool
            | Token::TypeSection | Token::TypeVoid | Token::TypeShell => {
                (TT_TYPE, MOD_DEFAULT_LIBRARY)
            }
            Token::Ident(word) if SOFT_KEYWORDS.contains(&word.as_str()) => {
                (TT_DECLARATION_KEYWORD, MOD_NONE)
            }
            Token::Ident(word) if BUILTIN_TYPE_IDENTS.contains(&word.as_str()) => {
                (TT_TYPE, MOD_DEFAULT_LIBRARY)
            }
            _ => continue,
        };
        if let Some(token) = raw_token_from_bytes(
            source,
            spanned.span.start,
            spanned.span.end,
            token_type,
            modifiers,
        ) {
            out.push(token);
        }
    }
}

/// A `type [Name]{ ... }` declaration's own fields — property names, and a
/// `Named(OtherType)` shape reference gets its own type/enum token (found by
/// text search from the field's span, same "search near a known offset"
/// pattern the rest of this file already uses — TypeField carries no
/// dedicated span for just the type-name portion of `field: OtherType;`).
fn collect_type_fields_tokens(
    fields: &[spar::ast::TypeField],
    source: &str,
    kinds: &SemanticKinds,
    out: &mut Vec<RawToken>,
) {
    use spar::ast::TypeFieldShape;
    for f in fields {
        if let Some(tok) =
            find_ident_token(source, f.span.start, &f.name, TT_PROPERTY, MOD_DECLARATION)
        {
            out.push(tok);
        }
        match &f.shape {
            TypeFieldShape::Primitive(_) => {}
            TypeFieldShape::Named(other) => {
                if let Some(tok) = find_ident_token(
                    source,
                    f.span.start,
                    other,
                    kinds.named_type_token(other),
                    MOD_NONE,
                ) {
                    out.push(tok);
                }
            }
            TypeFieldShape::TypeParameter(name) => {
                if let Some(tok) = find_ident_token(
                    source,
                    f.span.start,
                    name,
                    kinds.named_type_token(name),
                    MOD_NONE,
                ) {
                    out.push(tok);
                }
            }
            TypeFieldShape::Applied { name, .. } => {
                if let Some(tok) = find_ident_token(
                    source,
                    f.span.start,
                    name,
                    kinds.named_type_token(name),
                    MOD_NONE,
                ) {
                    out.push(tok);
                }
            }
            TypeFieldShape::Section(nested) => {
                collect_type_fields_tokens(nested, source, kinds, out)
            }
        }
    }
}

/// Same idea as `collect_type_fields_tokens`, for `schema Name { ... }`
/// field bodies (`SchemaFieldShape` has no `Named` variant, so there's no
/// type-reference token to emit — just property names, recursively).
fn collect_schema_fields_tokens(
    fields: &[spar::ast::SchemaField],
    source: &str,
    out: &mut Vec<RawToken>,
) {
    use spar::ast::SchemaFieldShape;
    for f in fields {
        if let Some(tok) =
            find_ident_token(source, f.span.start, &f.name, TT_PROPERTY, MOD_DECLARATION)
        {
            out.push(tok);
        }
        if let SchemaFieldShape::Section(nested) = &f.shape {
            collect_schema_fields_tokens(nested, source, out);
        }
    }
}

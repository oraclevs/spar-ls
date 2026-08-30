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
];

const TOKEN_MODIFIERS: &[SemanticTokenModifier] = &[
    SemanticTokenModifier::DECLARATION, // bit 0 = 1
];

const TT_VARIABLE: u32 = 0;
const TT_FUNCTION: u32 = 1;
const TT_PARAMETER: u32 = 2;
const TT_PROPERTY: u32 = 3;
const TT_TYPE: u32 = 5;
const TT_SECTION: u32 = 6;
const TT_TASK: u32 = 7;
const TT_TASK_FIELD: u32 = 8;
const TT_KEYWORD: u32 = 9;
const TT_ENUM: u32 = 10;
const TT_FUNCTION_GROUP: u32 = 11;
const MOD_NONE: u32 = 0;
const MOD_DECLARATION: u32 = 1;

struct RawToken {
    line: u32,
    start_char: u32,
    length: u32,
    token_type: u32,
    modifiers: u32,
}

struct SemanticKinds {
    enums: HashSet<String>,
    function_groups: HashSet<String>,
}

impl SemanticKinds {
    fn from_program(program: &Program) -> Self {
        use spar::ast::TopLevelItem;
        let mut kinds = Self {
            enums: HashSet::new(),
            function_groups: HashSet::new(),
        };
        for item in &program.items {
            match item {
                TopLevelItem::Enum(decl) => {
                    kinds.enums.insert(decl.name.clone());
                }
                TopLevelItem::FunctionGroup(decl) => {
                    kinds.function_groups.insert(decl.name.clone());
                }
                _ => {}
            }
        }
        kinds
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
    let off = byte_offset.min(source.len());
    let before = &source[..off];
    let line = before.bytes().filter(|&b| b == b'\n').count() as u32;
    let col = (off - before.rfind('\n').map(|p| p + 1).unwrap_or(0)) as u32;
    (line, col)
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
            out.push(raw_from_span(name_span, TT_FUNCTION, MOD_NONE));
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
            if let Some(tok) = find_ident_token(source, fc.span.start, member, TT_FUNCTION, MOD_NONE) {
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
            FS::Return(rv, _) => match rv {
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
            FS::For {
                var_name,
                iterable,
                body,
                span,
            } => {
                if let Some(tok) =
                    find_ident_token(source, span.start, var_name, TT_VARIABLE, MOD_DECLARATION)
                {
                    out.push(tok);
                }
                collect_expr_tokens(iterable, source, kinds, out);
                collect_stmts_tokens(body, source, kinds, out);
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
    match ty {
        SparType::Named(name) => {
            if let Some(token) = find_ident_token(
                source,
                from_byte,
                name,
                kinds.named_type_token(name),
                MOD_NONE,
            ) {
                out.push(token);
            }
        }
        SparType::List(inner) => collect_named_type_token(inner, source, from_byte, kinds, out),
        _ => {}
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
        ("shell", task.shell.as_ref()),
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
    for name in ["dependsOn", "env", "run"] {
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
        for command in &block.commands {
            for part in &command.parts {
                if let ShellTemplatePart::Expr(expression) = part {
                    let token_start = out.len();
                    collect_expr_tokens(expression, source, kinds, out);
                    for token in &mut out[token_start..] {
                        if token.token_type != TT_VARIABLE {
                            continue;
                        }
                        let line = source.lines().nth(token.line as usize).unwrap_or_default();
                        let start = token.start_char as usize;
                        let end = start + token.length as usize;
                        if line.get(start..end).is_some_and(|name| {
                            task.params.iter().any(|param| param.name == name)
                        }) {
                            token.token_type = TT_PARAMETER;
                        }
                    }
                }
            }
        }
    }
}

fn collect_tokens_from_program(program: &Program, source: &str, out: &mut Vec<RawToken>) {
    use spar::ast::TopLevelItem as TL;
    let kinds = SemanticKinds::from_program(program);
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
                collect_named_type_token(&vd.ty, source, vd.span.start, &kinds, out);
                if let Some(expr) = &vd.value {
                    collect_expr_tokens(expr, source, &kinds, out);
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
                    collect_expr_tokens(expr, source, &kinds, out);
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
                collect_section_items_tokens(&sd.items, source, &kinds, out);
            }
            TL::Function(fd) => {
                out.push(raw_from_span(&fd.name_span, TT_FUNCTION, MOD_DECLARATION));
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
                    collect_named_type_token(&param.ty, source, param.span.start, &kinds, out);
                }
                collect_named_type_token(&fd.ret, source, fd.ret_span.start, &kinds, out);
                collect_stmts_tokens(&fd.body.stmts, source, &kinds, out);
            }
            TL::Task(td) => {
                collect_task_tokens(program, index, td, source, &kinds, out);
            }
            TL::Type(td) => {
                out.push(raw_from_span(&td.name_span, TT_TYPE, MOD_DECLARATION));
                collect_type_fields_tokens(&td.fields, source, &kinds, out);
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
            TL::Import(_) => {}
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
                            &kinds,
                            out,
                        );
                    }
                    collect_named_type_token(&f.ret, source, f.ret_span.start, &kinds, out);
                    collect_stmts_tokens(&f.body.stmts, source, &kinds, out);
                }
            }
        }
    }
    collect_language_words(source, out);
}

fn collect_language_words(source: &str, out: &mut Vec<RawToken>) {
    const KEYWORDS: &[&str] = &[
        "var", "export", "import", "dynamic", "as", "private", "if", "else", "for",
        "in", "return", "function", "task", "type", "Schema", "SchemaFrom", "asPartOf", "from",
    ];
    const BUILTIN_TYPES: &[&str] = &["int", "float", "str", "bool", "section"];
    let bytes = source.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &source[start..i];
            let token_type = if KEYWORDS.contains(&word) {
                Some(TT_KEYWORD)
            } else if BUILTIN_TYPES.contains(&word) {
                Some(TT_TYPE)
            } else {
                None
            };
            if let Some(token_type) = token_type {
                let (line, col) = byte_to_lsp_pos(source, start);
                if out.iter().any(|token| {
                    token.line == line
                        && token.start_char == col
                        && token.length == word.len() as u32
                }) {
                    continue;
                }
                out.push(RawToken {
                    line,
                    start_char: col,
                    length: word.len() as u32,
                    token_type,
                    modifiers: MOD_NONE,
                });
            }
        } else {
            i += 1;
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
            TypeFieldShape::Section(nested) => {
                collect_type_fields_tokens(nested, source, kinds, out)
            }
        }
    }
}

/// Same idea as `collect_type_fields_tokens`, for `Schema [Name]{ ... }`
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

// ── AST hover helpers ─────────────────────────────────────────────────────────

fn expr_span(expr: &spar::ast::Expr) -> &Span {
    use spar::ast::Expr;
    match expr {
        Expr::Literal(l) => match l { spar::ast::Literal::Int(_) | spar::ast::Literal::Float(_) | spar::ast::Literal::Bool(_) => panic!("literal span is carried by its parent") },
        Expr::String(s) => &s.span,
        Expr::NamespaceRef(n) => &n.span,
        Expr::FnCall(f) => &f.span,
        Expr::BinaryOp(b) => &b.span,
        Expr::List(_, s) | Expr::Grouped(_, s) | Expr::Object(_, s) => s,
        Expr::Call { span, .. } | Expr::Unary { span, .. } | Expr::Comprehension { span, .. }
        | Expr::Index { span, .. } | Expr::FieldAccess { span, .. } => span,
        Expr::Shell(shell) | Expr::ExecShell(shell) => &shell.span,
    }
}

/// Find the smallest spanned expression containing the cursor. Literal nodes
/// have no independent span in the AST, so their enclosing expression is used.
fn find_expression_at_offset(program: &Program, offset: usize) -> Option<&spar::ast::Expr> {
    use spar::ast::{Expr, FieldValue, ReturnValue, SectionItem, StringPart};
    fn search(e: &Expr, off: usize) -> Option<&Expr> {
        let contains = !matches!(e, Expr::Literal(_)) && expr_span(e).start <= off && off <= expr_span(e).end;
        if !contains { return None; }
        let child = match e {
            Expr::BinaryOp(b) => search(&b.lhs, off).or_else(|| search(&b.rhs, off)),
            Expr::Unary { operand, .. } | Expr::Grouped(operand, _) => search(operand, off),
            Expr::List(xs, _) => xs.iter().find_map(|x| search(x, off)),
            Expr::FnCall(f) => f.args.iter().find_map(|x| search(x, off)),
            Expr::Call { args, .. } => args.iter().find_map(|x| search(&x.value, off)),
            Expr::Comprehension { source, body, .. } => search(source, off).or_else(|| search(body, off)),
            Expr::Index { source, index, .. } => search(source, off).or_else(|| search(index, off)),
            Expr::FieldAccess { base, .. } => search(base, off),
            Expr::String(s) => s.parts.iter().find_map(|p| if let StringPart::Expr(x)=p { search(x,off) } else { None }),
            Expr::Object(items, _) => items.iter().find_map(|i| match i { SectionItem::Field(f) => match &f.value { Some(FieldValue::Expr(x)) => search(x,off), _=>None }, SectionItem::Spread(s)=>search(&s.expr,off) }),
            Expr::Literal(_) | Expr::NamespaceRef(_) | Expr::Shell(_) | Expr::ExecShell(_) => None,
        };
        child.or(Some(e))
    }
    fn stmts(ss: &[FuncStmt], off: usize) -> Option<&Expr> {
        ss.iter().find_map(|s| match s {
            FuncStmt::LocalVar(v) => search(&v.value,off),
            FuncStmt::Assignment { value, .. } => search(value, off),
            FuncStmt::Expression(e, _) => search(e, off),
            FuncStmt::Break(_) | FuncStmt::Continue(_) => None,
            FuncStmt::Return(ReturnValue::Void, _) => None,
            FuncStmt::Return(ReturnValue::Expr(e),_) => search(e,off),
            FuncStmt::Return(ReturnValue::SectionBlock(fs),_) => fs.iter().find_map(|f|search(&f.value,off)),
            FuncStmt::If(i) => search(&i.condition,off).or_else(||stmts(&i.then_stmts,off)).or_else(||stmts(&i.else_stmts,off)),
            FuncStmt::For(statement) => search(&statement.iterable,off).or_else(||stmts(&statement.body,off)),
            FuncStmt::Try(_) => None,
        })
    }
    program.items.iter().find_map(|item| match item {
        TopLevelItem::Var(v) => v.value.as_ref().and_then(|e|search(e,offset)),
        TopLevelItem::Dynamic(v) => v.value.as_ref().and_then(|e|search(e,offset)),
        TopLevelItem::Section(s) => s.items.iter().find_map(|i| match i { SectionItem::Field(f)=>match &f.value {Some(FieldValue::Expr(e))=>search(e,offset), _=>None}, SectionItem::Spread(s)=>search(&s.expr,offset)}),
        TopLevelItem::Function(f) => stmts(&f.body.stmts,offset),
        TopLevelItem::FunctionGroup(g) => g.functions.iter().find_map(|f|stmts(&f.body.stmts,offset)),
        TopLevelItem::Statement(statement) => stmts(std::slice::from_ref(statement), offset),
        _ => None,
    })
}

/// Walk all function bodies in `program` and return `Some(has_else)` if any
/// `IfStmt` whose span contains `offset` is found.
pub fn find_if_at_offset(program: &Program, offset: usize) -> Option<bool> {
    for item in &program.items {
        if let TopLevelItem::Function(fdecl) = item {
            if let Some(has_else) = search_stmts_for_if(&fdecl.body.stmts, offset) {
                return Some(has_else);
            }
        }
    }
    None
}

fn search_stmts_for_if(stmts: &[FuncStmt], offset: usize) -> Option<bool> {
    for stmt in stmts {
        if let FuncStmt::If(if_stmt) = stmt {
            if if_stmt.span.start <= offset && offset <= if_stmt.span.end {
                return Some(!if_stmt.else_stmts.is_empty());
            }
            if let Some(found) = search_stmts_for_if(&if_stmt.then_stmts, offset) {
                return Some(found);
            }
            if let Some(found) = search_stmts_for_if(&if_stmt.else_stmts, offset) {
                return Some(found);
            }
        }
    }
    None
}

/// Walk top-level var declarations and function bodies to find an `Index`
/// expression whose span contains `offset`. Returns the element type of the
/// indexed list when the source is a known global variable.
pub fn find_index_elem_type_at_offset(
    program: &Program,
    symbols: &SymbolTable,
    offset: usize,
) -> Option<SparType> {
    use spar::ast::Expr;

    fn expr_index_elem(expr: &Expr, symbols: &SymbolTable, offset: usize) -> Option<SparType> {
        match expr {
            Expr::Index {
                source,
                index,
                span,
            } => {
                if span.start <= offset && offset <= span.end {
                    // Resolve source to a List type
                    let elem_ty = match source.as_ref() {
                        Expr::NamespaceRef(nr) if nr.segments.len() == 1 => {
                            match symbols.globals.get(&nr.segments[0]) {
                                Some(spar::resolver::GlobalEntry::Var {
                                    ty: SparType::List(elem),
                                    ..
                                }) => Some(*elem.clone()),
                                _ => None,
                            }
                        }
                        _ => None,
                    };
                    if elem_ty.is_some() {
                        return elem_ty;
                    }
                }
                expr_index_elem(source, symbols, offset)
                    .or_else(|| expr_index_elem(index, symbols, offset))
            }
            Expr::BinaryOp(b) => expr_index_elem(&b.lhs, symbols, offset)
                .or_else(|| expr_index_elem(&b.rhs, symbols, offset)),
            Expr::Unary { operand, .. } => expr_index_elem(operand, symbols, offset),
            Expr::List(items, _) => items
                .iter()
                .find_map(|e| expr_index_elem(e, symbols, offset)),
            Expr::Grouped(inner, _) => expr_index_elem(inner, symbols, offset),
            Expr::FnCall(fc) => fc
                .args
                .iter()
                .find_map(|a| expr_index_elem(a, symbols, offset)),
            Expr::Call { args, .. } => args
                .iter()
                .find_map(|a| expr_index_elem(&a.value, symbols, offset)),
            Expr::Comprehension { source, body, .. } => expr_index_elem(source, symbols, offset)
                .or_else(|| expr_index_elem(body, symbols, offset)),
            Expr::String(s) => {
                use spar::ast::StringPart;
                s.parts.iter().find_map(|p| {
                    if let StringPart::Expr(e) = p {
                        expr_index_elem(e, symbols, offset)
                    } else {
                        None
                    }
                })
            }
            Expr::Object(items, _) => {
                use spar::ast::{FieldValue, SectionItem};
                items.iter().find_map(|item| match item {
                    SectionItem::Field(f) => match &f.value {
                        Some(FieldValue::Expr(e)) => expr_index_elem(e, symbols, offset),
                        _ => None,
                    },
                    SectionItem::Spread(sp) => expr_index_elem(&sp.expr, symbols, offset),
                })
            }
            Expr::FieldAccess { base, .. } => expr_index_elem(base, symbols, offset),
            Expr::Literal(_) | Expr::NamespaceRef(_) | Expr::Shell(_) | Expr::ExecShell(_) => None,
        }
    }

    fn stmts_index_elem(
        stmts: &[FuncStmt],
        symbols: &SymbolTable,
        offset: usize,
    ) -> Option<SparType> {
        use spar::ast::ReturnValue;
        for stmt in stmts {
            match stmt {
                FuncStmt::LocalVar(lv) => {
                    if let Some(t) = expr_index_elem(&lv.value, symbols, offset) {
                        return Some(t);
                    }
                }
                FuncStmt::Return(rv, _) => match rv {
                    ReturnValue::Void => {}
                    ReturnValue::Expr(e) => {
                        if let Some(t) = expr_index_elem(e, symbols, offset) {
                            return Some(t);
                        }
                    }
                    ReturnValue::SectionBlock(fields) => {
                        for rf in fields {
                            if let Some(t) = expr_index_elem(&rf.value, symbols, offset) {
                                return Some(t);
                            }
                        }
                    }
                },
                FuncStmt::If(if_stmt) => {
                    if let Some(t) = expr_index_elem(&if_stmt.condition, symbols, offset) {
                        return Some(t);
                    }
                    if let Some(t) = stmts_index_elem(&if_stmt.then_stmts, symbols, offset) {
                        return Some(t);
                    }
                    if let Some(t) = stmts_index_elem(&if_stmt.else_stmts, symbols, offset) {
                        return Some(t);
                    }
                }
                FuncStmt::For(statement) => {
                    if let Some(t) = expr_index_elem(&statement.iterable, symbols, offset) {
                        return Some(t);
                    }
                    if let Some(t) = stmts_index_elem(&statement.body, symbols, offset) {
                        return Some(t);
                    }
                }
                FuncStmt::Assignment { value, .. } | FuncStmt::Expression(value, _) => {
                    if let Some(t) = expr_index_elem(value, symbols, offset) {
                        return Some(t);
                    }
                }
                FuncStmt::Break(_) | FuncStmt::Continue(_) => {}
                FuncStmt::Try(_) => {}
            }
        }
        None
    }

    for item in &program.items {
        match item {
            TopLevelItem::Var(vd) => {
                if let Some(v) = &vd.value {
                    if let Some(t) = expr_index_elem(v, symbols, offset) {
                        return Some(t);
                    }
                }
            }
            TopLevelItem::Function(fd) => {
                if let Some(t) = stmts_index_elem(&fd.body.stmts, symbols, offset) {
                    return Some(t);
                }
            }
            _ => {}
        }
    }
    None
}

// ── Hover formatters ──────────────────────────────────────────────────────────

fn format_hover_global(name: &str, entry: &GlobalEntry) -> String {
    let ty_str = match entry {
        GlobalEntry::Var {
            ty,
            optional,
            exported,
            ..
        } => {
            let base = format_spar_type(ty);
            let opt_marker = if *optional { "?" } else { "" };
            let export_prefix = if *exported { "export " } else { "" };
            format!("{export_prefix}{}{opt_marker}", base)
        }
        GlobalEntry::Dynamic { optional, .. } => {
            if *optional {
                "dynamic?".to_string()
            } else {
                "dynamic".to_string()
            }
        }
    };
    format!("```spar\n(var) {}: {}\n```", name, ty_str)
}

fn format_hover_section(symbols: &SymbolTable, path: &[String], section: &SectionEntry) -> String {
    let section_label = path.join(".");
    let field_list: String = section
        .fields
        .iter()
        .map(|(name, fentry)| {
            if fentry.ty == Some(SparType::Section) {
                format!("  {}: section  // → {}::{}", name, section_label, name)
            } else {
                let ty_str = fentry
                    .ty
                    .as_ref()
                    .map(format_spar_type)
                    .unwrap_or_else(|| resolve_field_type_display(symbols, path, name));
                format!("  {}: {}", name, ty_str)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("```spar\n[{}]{{\n{}\n}}\n```", section_label, field_list)
}

fn format_hover_function(name: &str, entry: &FunctionEntry) -> String {
    let params_str = entry
        .params
        .iter()
        .map(|(pname, pty)| format!("{}: {}", pname, format_spar_type(pty)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "```spar\nfunction {}({}) -> {}\n```",
        name,
        params_str,
        format_spar_type(&entry.ret)
    )
}

fn format_hover_type(name: &str, entry: &TypeEntry) -> String {
    let field_list: String = entry
        .fields
        .iter()
        .map(|f| {
            format!(
                "  {}{}: {}",
                f.name,
                if f.optional { "?" } else { "" },
                format_type_field_shape(&f.shape)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("```spar\ntype [{name}] {{\n{field_list}\n}}\n```")
}

fn format_hover_enum(name: &str, entry: &EnumEntry) -> String {
    format!(
        "```spar\nenum {name} {{ {} }}\n```",
        entry.variants.join(", ")
    )
}

fn format_hover_enum_variant(enum_name: &str, variant: &str) -> String {
    format!("```spar\n{enum_name}::{variant}  // variant of enum {enum_name}\n```")
}

/// `type [Name]`/`enum Name`/`functionGroup Name` — bare declaration hover,
/// or a qualified `Name::member` reference (`EnumName::Variant`,
/// `GroupName::function`). Pulled out of the main `hover()` dispatch so it's
/// directly unit-testable without a `SparLanguageServer`/`Client` instance.
fn hover_type_enum_group(
    symbols: &SymbolTable,
    source: &str,
    pos: Position,
    word: &str,
) -> Option<String> {
    if let Some(entry) = symbols.types.get(word) {
        return Some(format_hover_type(word, entry));
    }
    if let Some(entry) = symbols.enums.get(word) {
        return Some(format_hover_enum(word, entry));
    }
    if let Some(entry) = symbols.function_groups.get(word) {
        return Some(format_hover_function_group(word, entry));
    }
    let prefix = path_prefix_before_word(source, pos)?;
    if prefix.len() != 1 {
        return None;
    }
    if let Some(entry) = symbols.enums.get(&prefix[0]) {
        if entry.variants.iter().any(|v| v == word) {
            return Some(format_hover_enum_variant(&prefix[0], word));
        }
    }
    if let Some(entry) = symbols.function_groups.get(&prefix[0]) {
        if let Some(member) = entry.functions.get(word) {
            return Some(format_hover_function(word, member));
        }
    }
    None
}

fn format_hover_function_group(name: &str, entry: &FunctionGroupEntry) -> String {
    let mut member_names: Vec<&String> = entry.functions.keys().collect();
    member_names.sort();
    let members: String = member_names
        .iter()
        .map(|member_name| {
            let f = &entry.functions[member_name.as_str()];
            let params = f
                .params
                .iter()
                .map(|(pname, pty)| format!("{}: {}", pname, format_spar_type(pty)))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "  function {member_name}({params}) -> {}",
                format_spar_type(&f.ret)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("```spar\nfunctionGroup {name} {{\n{members}\n}}\n```")
}

fn formatted_task_param(param: &spar::ast::TaskParam) -> String {
    let task = spar::ast::TaskDecl {
        name: "Hover".to_string(),
        name_span: param.span.clone(),
        params: vec![param.clone()],
        description: None,
        default: None,
        quiet: None,
        private: None,
        group: None,
        confirm: None,
        depends_on: Vec::new(),
        env: Vec::new(),
        cwd: None,
        shell: None,
        run_blocks: Vec::new(),
        span: param.span.clone(),
        field_spans: Vec::new(),
        closing_span: param.span.clone(),
    };
    let program = Program {
        is_schema_file: false,
        load_env: None,
        shebang: None,
        items: vec![TopLevelItem::Task(Box::new(task.clone()))],
    };
    let formatted = format_program(&program, &FormatConfig::default());
    formatted
        .lines()
        .next()
        .and_then(|line| line.split_once('('))
        .and_then(|(_, rest)| rest.rsplit_once(')'))
        .map(|(param, _)| param.to_string())
        .unwrap_or_else(|| {
            let variadic = if param.variadic { "*" } else { "" };
            format!("{variadic}{}: {}", param.name, format_spar_type(&param.ty))
        })
}

fn format_hover_task(task: &spar::ast::TaskDecl) -> String {
    let params = task
        .params
        .iter()
        .map(formatted_task_param)
        .collect::<Vec<_>>()
        .join(", ");
    let mut value = format!("```spar\ntask [{}]({})\n```", task.name, params);
    if task.description.is_some() {
        let mut description_task = task.clone();
        description_task.params.clear();
        description_task.run_blocks.clear();
        let program = Program {
            is_schema_file: false,
            load_env: None,
            shebang: None,
            items: vec![TopLevelItem::Task(Box::new(description_task))],
        };
        let formatted = format_program(&program, &FormatConfig::default());
        if let Some(description) = formatted.lines().find_map(|line| {
            line.trim_start()
                .strip_prefix("description: ")
                .map(|text| text.trim_end_matches(';').trim_matches('"'))
                .filter(|text| !text.is_empty())
        }) {
            value.push_str("\n\n");
            value.push_str(description);
        }
    }
    if !task.depends_on.is_empty() {
        value.push_str("\n\nDepends on: `");
        value.push_str(
            &task
                .depends_on
                .iter()
                .map(|dependency| dependency.name.as_str())
                .collect::<Vec<_>>()
                .join("`, `"),
        );
        value.push('`');
    }
    value
}

fn format_hover_task_param(param: &spar::ast::TaskParam) -> String {
    format!(
        "```spar\n(task parameter) {}\n```",
        formatted_task_param(param)
    )
}

fn cursor_on_task_param_declaration(
    source: &str,
    param: &spar::ast::TaskParam,
    offset: usize,
) -> bool {
    find_ident_byte(source, param.span.start, &param.name)
        .is_some_and(|start| start <= offset && offset <= start + param.name.len())
}

fn task_hover_at_offset(
    program: &Program,
    source: &str,
    offset: usize,
    word: &str,
) -> Option<String> {
    for item in &program.items {
        let TopLevelItem::Task(task) = item else {
            continue;
        };
        if task.name == word && task.name_span.start <= offset && offset <= task.name_span.end {
            return Some(format_hover_task(task));
        }
        if let Some(param) = task.params.iter().find(|param| {
            param.name == word && cursor_on_task_param_declaration(source, param, offset)
        }) {
            return Some(format_hover_task_param(param));
        }
        // Hovering a `dependsOn: [Build]` entry shows the referenced task's
        // own signature, same as hovering its declaration would.
        if let Some(dep) = task
            .depends_on
            .iter()
            .find(|dep| dep.name == word && dep.span.start <= offset && offset <= dep.span.end)
        {
            if let Some(referenced) = program.items.iter().find_map(|it| match it {
                TopLevelItem::Task(t) if t.name == dep.name => Some(t.as_ref()),
                _ => None,
            }) {
                return Some(format_hover_task(referenced));
            }
        }
    }

    let (task, _, _) = task_at_offset(program, source, offset)?;
    let spar::ast::Expr::NamespaceRef(reference) = task_interpolation_at_offset(task, offset)? else {
        return None;
    };
    if reference.segments.len() != 1 || reference.segments[0] != word {
        return None;
    }
    task.params
        .iter()
        .find(|param| param.name == word)
        .map(format_hover_task_param)
}

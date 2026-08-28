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
            Expr::Literal(_) | Expr::NamespaceRef(_) => None,
        };
        child.or(Some(e))
    }
    fn stmts(ss: &[FuncStmt], off: usize) -> Option<&Expr> {
        ss.iter().find_map(|s| match s {
            FuncStmt::LocalVar(v) => search(&v.value,off),
            FuncStmt::Return(ReturnValue::Expr(e),_) => search(e,off),
            FuncStmt::Return(ReturnValue::SectionBlock(fs),_) => fs.iter().find_map(|f|search(&f.value,off)),
            FuncStmt::If(i) => search(&i.condition,off).or_else(||stmts(&i.then_stmts,off)).or_else(||stmts(&i.else_stmts,off)),
            FuncStmt::For { iterable, body, .. } => search(iterable,off).or_else(||stmts(body,off)),
        })
    }
    program.items.iter().find_map(|item| match item {
        TopLevelItem::Var(v) => v.value.as_ref().and_then(|e|search(e,offset)),
        TopLevelItem::Dynamic(v) => v.value.as_ref().and_then(|e|search(e,offset)),
        TopLevelItem::Section(s) => s.items.iter().find_map(|i| match i { SectionItem::Field(f)=>match &f.value {Some(FieldValue::Expr(e))=>search(e,offset), _=>None}, SectionItem::Spread(s)=>search(&s.expr,offset)}),
        TopLevelItem::Function(f) => stmts(&f.body.stmts,offset),
        TopLevelItem::FunctionGroup(g) => g.functions.iter().find_map(|f|stmts(&f.body.stmts,offset)),
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
            Expr::Literal(_) | Expr::NamespaceRef(_) => None,
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
                FuncStmt::For { iterable, body, .. } => {
                    if let Some(t) = expr_index_elem(iterable, symbols, offset) {
                        return Some(t);
                    }
                    if let Some(t) = stmts_index_elem(body, symbols, offset) {
                        return Some(t);
                    }
                }
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

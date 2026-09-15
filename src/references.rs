// ── Find references ─────────────────────────────────────────────────────────
//
// Walks the AST looking for uses of the symbol under the cursor. Same-file
// references match the bare identifier; cross-file references are found by
// walking the *transitive* reverse import graph in `importers` (a file that
// imports a file that re-exports the target via `asPartOf` can still see
// it, so one hop isn't enough), and for each candidate file, resolving
// whatever alias (if any) that file uses for the file the symbol is
// defined in.
//
// Precision limits, deliberate (same latitude given to `definition_at`'s
// own section-field fallback): a `.field` access matches by field name
// alone, not by base-expression type, so an unrelated object with a field
// of the same name is not ruled out. An `AsPartOf` import splices every
// export of the target file in bare, so those files are always searched as
// bare-word matches too. A `Selective`/`TypeSelective` import only searches
// a file bare when its import statement actually requested that specific
// name (see `SpliceRelationship::Named`) — an aliased request
// (`import { foo as bar }`) is still matched against the *original* name
// `foo`, even though occurrences in that file were renamed to `bar` at
// splice time, so a locally-aliased selective import's usages are a known
// gap (a false miss, not a false match). Type references (a `type [Foo]`
// used as a field's type elsewhere) are not walked — this covers the same
// value/task symbol kinds `definition_at` resolves, not structural type
// usages.

#[derive(Clone, Copy)]
enum RefTarget<'a> {
    /// Same-file (or import-spliced) bare identifier.
    Bare(&'a str),
    /// `alias::...::word` — a namespace chain through this specific alias,
    /// ending in `word`.
    Aliased(&'a str, &'a str),
}

impl RefTarget<'_> {
    fn word(&self) -> &str {
        match self {
            RefTarget::Bare(w) | RefTarget::Aliased(_, w) => w,
        }
    }
}

fn collect_expr_refs(expr: &spar::ast::Expr, target: RefTarget, out: &mut Vec<Span>) {
    use spar::ast::{Expr, StringPart};
    match expr {
        // A namespace-qualified call (`alias::make(...)`) parses its whole
        // `name` as one compound `"alias::make"` string — not as
        // `NamespaceRef` segments the way a bare reference does — with
        // `name_span` already pointing at just the trailing segment
        // ("make"), which is exactly the location a reference should
        // point at.
        Expr::Call { name, name_span, args, .. } => {
            let segments: Vec<&str> = name.split("::").collect();
            let matches = match target {
                RefTarget::Bare(word) => segments.len() == 1 && segments[0] == word,
                RefTarget::Aliased(alias, word) => {
                    segments.len() >= 2
                        && segments.first() == Some(&alias)
                        && segments.last() == Some(&word)
                }
            };
            if matches {
                out.push(name_span.clone());
            }
            for arg in args {
                collect_expr_refs(&arg.value, target, out);
            }
        }
        Expr::FnCall(fc) => {
            let matches = match target {
                RefTarget::Bare(word) => fc.name == word,
                RefTarget::Aliased(..) => false,
            };
            if matches {
                out.push(fc.span.clone());
            }
            for arg in &fc.args {
                collect_expr_refs(arg, target, out);
            }
        }
        Expr::NamespaceRef(nr) => match target {
            RefTarget::Bare(word) => {
                if nr.segments.len() == 1 && nr.segments[0] == word {
                    out.push(nr.span.clone());
                }
            }
            RefTarget::Aliased(alias, word) => {
                if nr.segments.len() >= 2
                    && nr.segments.first().map(String::as_str) == Some(alias)
                    && nr.segments.last().map(String::as_str) == Some(word)
                {
                    out.push(nr.span.clone());
                }
            }
        },
        Expr::BinaryOp(b) => {
            collect_expr_refs(&b.lhs, target, out);
            collect_expr_refs(&b.rhs, target, out);
        }
        Expr::Unary { operand, .. } => collect_expr_refs(operand, target, out),
        Expr::List(items, _) => {
            for item in items {
                collect_expr_refs(item, target, out);
            }
        }
        Expr::Grouped(inner, _) => collect_expr_refs(inner, target, out),
        Expr::Comprehension { source, body, .. } => {
            collect_expr_refs(source, target, out);
            collect_expr_refs(body, target, out);
        }
        Expr::Index { source, index, .. } => {
            collect_expr_refs(source, target, out);
            collect_expr_refs(index, target, out);
        }
        Expr::String(s) => {
            for part in &s.parts {
                if let StringPart::Expr(e) = part {
                    collect_expr_refs(e, target, out);
                }
            }
        }
        Expr::Object(items, _) => {
            use spar::ast::SectionItem;
            for item in items {
                match item {
                    SectionItem::Field(f) => {
                        if let Some(spar::ast::FieldValue::Expr(e)) = &f.value {
                            collect_expr_refs(e, target, out);
                        }
                    }
                    SectionItem::Spread(sp) => collect_expr_refs(&sp.expr, target, out),
                }
            }
        }
        Expr::FieldAccess { base, field, field_span, .. } => {
            collect_expr_refs(base, target, out);
            if field == target.word() {
                out.push(field_span.clone());
            }
        }
        Expr::Literal(_) | Expr::Shell(_) | Expr::ExecShell(_) => {}
    }
}

fn collect_section_item_refs(items: &[spar::ast::SectionItem], target: RefTarget, out: &mut Vec<Span>) {
    use spar::ast::{FieldValue, SectionItem};
    for item in items {
        match item {
            SectionItem::Field(fd) => match &fd.value {
                Some(FieldValue::Expr(e)) => collect_expr_refs(e, target, out),
                Some(FieldValue::Nested(nested)) => collect_section_item_refs(nested, target, out),
                None => {}
            },
            SectionItem::Spread(ss) => collect_expr_refs(&ss.expr, target, out),
        }
    }
}

fn collect_stmts_refs(stmts: &[FuncStmt], target: RefTarget, out: &mut Vec<Span>) {
    use spar::ast::ReturnValue;
    for stmt in stmts {
        match stmt {
            FuncStmt::LocalVar(v) => collect_expr_refs(&v.value, target, out),
            FuncStmt::Assignment { value, .. } | FuncStmt::Expression(value, _) => {
                collect_expr_refs(value, target, out)
            }
            FuncStmt::Break(_) | FuncStmt::Continue(_) => {}
            FuncStmt::Return(ReturnValue::Void, _) => {}
            FuncStmt::Return(ReturnValue::Expr(e), _) => collect_expr_refs(e, target, out),
            FuncStmt::Return(ReturnValue::SectionBlock(fields), _) => {
                for f in fields {
                    collect_expr_refs(&f.value, target, out);
                }
            }
            FuncStmt::If(i) => {
                collect_expr_refs(&i.condition, target, out);
                collect_stmts_refs(&i.then_stmts, target, out);
                collect_stmts_refs(&i.else_stmts, target, out);
            }
            FuncStmt::For(statement) => {
                collect_expr_refs(&statement.iterable, target, out);
                collect_stmts_refs(&statement.body, target, out);
            }
        }
    }
}

fn collect_program_refs(program: &Program, target: RefTarget, out: &mut Vec<Span>) {
    use spar::ast::{ShellTemplatePart, TopLevelItem as TL};
    for item in &program.items {
        match item {
            TL::Var(vd) => {
                if let Some(e) = &vd.value {
                    collect_expr_refs(e, target, out);
                }
            }
            TL::Dynamic(dd) => {
                if let Some(e) = &dd.value {
                    collect_expr_refs(e, target, out);
                }
            }
            TL::Section(sd) => collect_section_item_refs(&sd.items, target, out),
            TL::Function(fd) => collect_stmts_refs(&fd.body.stmts, target, out),
            TL::FunctionGroup(gd) => {
                for f in &gd.functions {
                    collect_stmts_refs(&f.body.stmts, target, out);
                }
            }
            TL::Statement(statement) => {
                collect_stmts_refs(std::slice::from_ref(statement), target, out)
            }
            TL::Task(td) => {
                for param in &td.params {
                    if let Some(default) = &param.default {
                        collect_expr_refs(default, target, out);
                    }
                }
                for e in [
                    &td.description,
                    &td.default,
                    &td.quiet,
                    &td.private,
                    &td.group,
                    &td.confirm,
                    &td.cwd,
                    &td.shell,
                ]
                .into_iter()
                .flatten()
                {
                    collect_expr_refs(e, target, out);
                }
                if let RefTarget::Bare(word) = target {
                    for dependency in &td.depends_on {
                        if dependency.name == word {
                            out.push(dependency.span.clone());
                        }
                    }
                }
                for (_, value) in &td.env {
                    collect_expr_refs(value, target, out);
                }
                for block in &td.run_blocks {
                    for command in &block.commands {
                        for part in &command.parts {
                            if let ShellTemplatePart::Expr(e) = part {
                                collect_expr_refs(e, target, out);
                            }
                        }
                    }
                }
            }
            TL::Enum(_) | TL::Type(_) | TL::SchemaSection(_) | TL::SchemaFrom(_) | TL::Import(_) => {}
        }
    }
}

/// The name of the local `functionGroup` declaring `member` as one of its
/// functions, if any — lets same-file `Group::member()` call sites be found
/// even though the search word itself is just `member` (bare), since
/// `RefTarget::Bare` alone never matches a qualified `Group::member` call.
fn local_function_group_owning<'a>(program: &'a Program, member: &str) -> Option<&'a str> {
    program.items.iter().find_map(|item| {
        let TopLevelItem::FunctionGroup(group) = item else { return None };
        group
            .functions
            .iter()
            .any(|f| f.name == member)
            .then_some(group.name.as_str())
    })
}

/// Every file transitively reachable by walking `importers` backwards from
/// `defining_file` (i.e. every file that imports it, directly or through a
/// chain of `asPartOf`/aliased/selective imports) — including
/// `defining_file` itself.
fn reachable_files(
    importers: &HashMap<PathBuf, HashSet<PathBuf>>,
    defining_file: &std::path::Path,
) -> HashSet<PathBuf> {
    let mut seen = HashSet::new();
    let mut queue = vec![defining_file.to_path_buf()];
    seen.insert(defining_file.to_path_buf());
    while let Some(current) = queue.pop() {
        if let Some(next) = importers.get(&current) {
            for file in next {
                if seen.insert(file.clone()) {
                    queue.push(file.clone());
                }
            }
        }
    }
    seen
}

/// How `program` (the AST of `file_uri`) reaches `defining_file`'s symbols
/// bare, if at all.
enum SpliceRelationship {
    /// Not spliced (e.g. this file doesn't import `defining_file`, or does
    /// so only via `Aliased`/`Schema`).
    None,
    /// `Selective`/`TypeSelective` — every *originally-declared* name it
    /// requested (before any `as` alias renames it locally). Only a word
    /// that's actually in this list was truly brought into scope here;
    /// anything else is an unrelated same-named local symbol, not a
    /// reference to the thing being searched for.
    Named(Vec<String>),
    /// `AsPartOf` — every export is implicitly pulled in, so any bare word
    /// occurring in this file is fair game to search.
    Everything,
}

/// Whichever alias (if any) `state`'s file uses to import `defining_file`,
/// plus how (if at all) it splices the target's symbols in bare
/// (`Selective`/`TypeSelective`/`AsPartOf`).
///
/// `Aliased`/`Schema` imports are read off `state.ast` — `expand_imports`
/// leaves those import statements in place. `Selective`/`TypeSelective`/
/// `AsPartOf` imports are spliced away entirely (replaced by their target's
/// items) by the time `state.ast` exists, so those are read off
/// `state.spliced_import_decls` instead — a fresh, pre-splice parse kept
/// around for exactly this purpose (see `DocumentState`).
fn import_relationship(
    state: &DocumentState,
    file_uri: &Url,
    defining_file: &std::path::Path,
) -> (Option<String>, SpliceRelationship) {
    use spar::ast::ImportKind;
    let Some(base) = file_uri.to_file_path().ok().and_then(|p| p.parent().map(|p| p.to_path_buf())) else {
        return (None, SpliceRelationship::None);
    };
    let Ok(defining_canon) = defining_file.canonicalize() else {
        return (None, SpliceRelationship::None);
    };
    if let Some(program) = &state.ast {
        for item in &program.items {
            let TopLevelItem::Import(decl) = item else { continue };
            let ImportKind::Aliased(explicit) = &decl.kind else { continue };
            let Ok(candidate) = base.join(&decl.path).canonicalize() else { continue };
            if candidate != defining_canon {
                continue;
            }
            let derived = std::path::Path::new(&decl.path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            return (Some(explicit.clone().unwrap_or(derived)), SpliceRelationship::None);
        }
    }
    for decl in &state.spliced_import_decls {
        let Ok(candidate) = base.join(&decl.path).canonicalize() else { continue };
        if candidate != defining_canon {
            continue;
        }
        return match &decl.kind {
            ImportKind::Selective(items) | ImportKind::TypeSelective(items) => (
                None,
                SpliceRelationship::Named(items.iter().map(|i| i.name.clone()).collect()),
            ),
            ImportKind::AsPartOf => (None, SpliceRelationship::Everything),
            ImportKind::Aliased(_) | ImportKind::Schema => (None, SpliceRelationship::None),
        };
    }
    (None, SpliceRelationship::None)
}

/// The synchronous core: given who's being asked about (`word`, resolved
/// via `definition_at` into `defining_location`/`defining_source`) and a
/// snapshot of the reverse import graph, find every reference. No `Client`/
/// mutex/async machinery needed here, so this is directly unit-testable —
/// `SparLanguageServer::references_at` below is a thin async wrapper that
/// just does the locking and delegates here.
fn compute_references(
    word: &str,
    defining_location: Location,
    defining_source: String,
    importers: &HashMap<PathBuf, HashSet<PathBuf>>,
    include_declaration: bool,
) -> Vec<Location> {
    let Ok(defining_file) = defining_location.uri.to_file_path() else { return Vec::new() };
    let candidates = reachable_files(importers, &defining_file);

    let mut results = Vec::new();
    for candidate_path in candidates {
        let is_defining_file = candidate_path == defining_file;
        let (source, base_dir) = if is_defining_file {
            (defining_source.clone(), candidate_path.parent().map(|p| p.to_path_buf()))
        } else {
            let Ok(src) = std::fs::read_to_string(&candidate_path) else { continue };
            (src, candidate_path.parent().map(|p| p.to_path_buf()))
        };
        let Some(base_dir) = base_dir else { continue };
        let Ok(candidate_uri) = Url::from_file_path(&candidate_path) else { continue };

        let state = SparLanguageServer::analyze(&source, &base_dir);
        let Some(program) = &state.ast else { continue };

        let mut spans = Vec::new();
        if is_defining_file {
            collect_program_refs(program, RefTarget::Bare(word), &mut spans);
            if let Some(group_name) = local_function_group_owning(program, word) {
                collect_program_refs(program, RefTarget::Aliased(group_name, word), &mut spans);
            }
        } else {
            let (alias, splice) = import_relationship(&state, &candidate_uri, &defining_file);
            if let Some(alias) = &alias {
                collect_program_refs(program, RefTarget::Aliased(alias, word), &mut spans);
            }
            let is_spliced = match &splice {
                SpliceRelationship::Everything => true,
                SpliceRelationship::Named(names) => names.iter().any(|n| n == word),
                SpliceRelationship::None => false,
            };
            if is_spliced {
                collect_program_refs(program, RefTarget::Bare(word), &mut spans);
            }
            if alias.is_none() && !is_spliced {
                // Not actually related to the defining file through any
                // import this file declares (reached only via a longer
                // chain elsewhere in `importers`), or a Selective import
                // exists but never actually requested this symbol —
                // nothing to search.
                continue;
            }
        }

        for span in spans {
            results.push(span_location(candidate_uri.clone(), &source, &span));
        }
    }

    if include_declaration {
        results.push(defining_location);
    }
    results
}

impl SparLanguageServer {
    async fn references_at(&self, uri: &Url, pos: Position, include_declaration: bool) -> Vec<Location> {
        let (word, defining_location, defining_source) = {
            let docs = self.documents.lock().await;
            let Some(state) = docs.get(uri) else { return Vec::new() };
            let word = word_at_position(&state.source, pos);
            if word.is_empty() {
                return Vec::new();
            }
            let Some(location) = definition_at(uri, state, pos) else { return Vec::new() };
            let defining_source = if location.uri == *uri {
                state.source.clone()
            } else {
                let Ok(path) = location.uri.to_file_path() else { return Vec::new() };
                let Ok(src) = std::fs::read_to_string(path) else { return Vec::new() };
                src
            };
            (word, location, defining_source)
        };

        let importers_snapshot = self.importers.lock().await.clone();
        compute_references(&word, defining_location, defining_source, &importers_snapshot, include_declaration)
    }
}

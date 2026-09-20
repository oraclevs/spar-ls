// ── Find references ─────────────────────────────────────────────────────────
//
// Walks the AST looking for uses of the symbol under the cursor. Same-file
// references match the bare identifier; cross-file references are found by
// walking the *transitive* reverse import graph in `importers` (a file that
// imports a file that re-exports the target can still see it, so one hop
// isn't enough), and for each candidate file, resolving
// whatever alias (if any) that file uses for the file the symbol is
// defined in.
//
// Precision limits, deliberate (same latitude given to `definition_at`'s
// own section-field fallback): a `.field` access matches by field name
// alone, not by base-expression type, so an unrelated object with a field
// of the same name is not ruled out. A `Selective`/`TypeSelective` import only searches
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

fn collect_shell_word_refs(word: &spar::ast::ShellWord, target: RefTarget, out: &mut Vec<Span>) {
    for part in &word.parts {
        match part {
            spar::ast::ShellWordPart::Expr(expr) => collect_expr_refs(expr, target, out),
            spar::ast::ShellWordPart::CommandSubstitution(shell) => {
                collect_shell_expr_refs(shell, target, out)
            }
            spar::ast::ShellWordPart::Literal(_) | spar::ast::ShellWordPart::Environment(_) => {}
        }
    }
}

fn collect_shell_command_expr_refs(
    command: &spar::ast::ShellCommandExpr,
    target: RefTarget,
    out: &mut Vec<Span>,
) {
    collect_shell_word_refs(&command.program, target, out);
    for arg in &command.args {
        collect_shell_word_refs(arg, target, out);
    }
    for redirect in [&command.stdin, &command.stdout, &command.stderr]
        .into_iter()
        .flatten()
    {
        collect_shell_word_refs(&redirect.target, target, out);
    }
    for redirect in &command.redirections {
        if let spar::ast::ShellFdRedirectTarget::File(file) = &redirect.target {
            collect_shell_word_refs(&file.target, target, out);
        }
    }
}

fn collect_shell_expr_refs(shell: &spar::ast::ShellExpr, target: RefTarget, out: &mut Vec<Span>) {
    collect_stmts_refs(&shell.statements, target, out);
    for (_, step) in &shell.steps {
        match step {
            spar::ast::ShellStep::Command(command) => {
                collect_shell_command_expr_refs(command, target, out)
            }
            spar::ast::ShellStep::Pipeline(commands) => {
                for command in commands {
                    collect_shell_command_expr_refs(command, target, out);
                }
            }
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
        Expr::Await { value, .. } => collect_expr_refs(value, target, out),
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
        Expr::Shell(shell) | Expr::ExecShell(shell) | Expr::CommandSubstitution(shell) => {
            collect_shell_expr_refs(shell, target, out)
        }
        Expr::Literal(_) => {}
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
            FuncStmt::Try(_) => {}
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
                    match &block.body {
                        spar::ast::RunBody::Bash(commands) => {
                            for command in commands {
                                for part in &command.parts {
                                    if let ShellTemplatePart::Expr(e) = part {
                                        collect_expr_refs(e, target, out);
                                    }
                                }
                            }
                        }
                        spar::ast::RunBody::Native(shell) => {
                            collect_expr_refs(&spar::ast::Expr::Shell(shell.clone()), target, out);
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
/// chain of aliased/selective imports) — including
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
}

/// Whichever alias (if any) `state`'s file uses to import `defining_file`,
/// plus how (if at all) it splices the target's symbols in bare
/// (`Selective`/`TypeSelective`).
///
/// `Aliased`/`Schema` imports are read off `state.ast` — `expand_imports`
/// leaves those import statements in place. `Selective`/`TypeSelective`
/// imports are spliced away entirely (replaced by their target's items) by
/// the time `state.ast` exists, so those are read off
/// `state.spliced_import_decls` instead — a fresh, pre-splice parse kept
/// around for exactly this purpose (see `DocumentState`).
fn import_relationship(
    state: &DocumentState,
    _file_uri: &Url,
    defining_file: &std::path::Path,
) -> (Option<String>, SpliceRelationship) {
    use spar::ast::ImportKind;
    let Ok(defining_canon) = defining_file.canonicalize() else {
        return (None, SpliceRelationship::None);
    };
    if let Some(program) = &state.ast {
        for item in &program.items {
            let TopLevelItem::Import(decl) = item else { continue };
            let ImportKind::Aliased(explicit) = &decl.kind else { continue };
            let derived = std::path::Path::new(&decl.path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let alias = explicit.clone().unwrap_or(derived);
            let Some(resolved) = state.import_paths.get(&alias) else { continue };
            let Ok(candidate) = resolved.canonicalize() else { continue };
            if candidate != defining_canon {
                continue;
            }
            return (Some(alias), SpliceRelationship::None);
        }
    }
    for decl in &state.spliced_import_decls {
        let Some(resolved) = state.spliced_import_paths.get(&decl.path) else { continue };
        let Ok(candidate) = resolved.canonicalize() else { continue };
        if candidate != defining_canon {
            continue;
        }
        return match &decl.kind {
            ImportKind::Selective(items) | ImportKind::TypeSelective(items) => (
                None,
                SpliceRelationship::Named(items.iter().map(|i| i.name.clone()).collect()),
            ),
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
        let mut locations = compute_references(
            &word,
            defining_location,
            defining_source,
            &importers_snapshot,
            include_declaration,
        );
        // The AST walk above misses uses that live only in type annotations
        // (`List<Human>`); add this document's semantic occurrences, deduplicated.
        let docs = self.documents.lock().await;
        if let Some(state) = docs.get(uri) {
            let index = self.workspace_index.lock().await;
            if let Some(target) = semantic_target_at(uri, state, pos, &index) {
                for occurrence in semantic_occurrences_in_document(uri, state, &target) {
                    if !include_declaration && occurrence.role == SemanticOccurrenceRole::Declaration {
                        continue;
                    }
                    let location = Location { uri: uri.clone(), range: occurrence.range };
                    if !locations.iter().any(|existing| same_location_key(existing, &location)) {
                        locations.push(location);
                    }
                }
            }
        }
        locations.sort_by_key(|location| (location.uri.to_string(), location.range.start.line, location.range.start.character));
        locations
    }
}

// ── Shared semantic identity for highlights/rename ───────────────────────────

#[derive(Debug, Clone)]
struct SemanticTarget {
    id: SymbolId,
    name: String,
    declaration: Location,
    definition_key: Location,
    local_decl_byte: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SemanticOccurrenceRole {
    Read,
    Write,
    Declaration,
}

#[derive(Debug, Clone)]
struct SemanticOccurrence {
    range: Range,
    role: SemanticOccurrenceRole,
}

#[derive(Debug, Clone)]
struct LocalBinding {
    name: String,
    decl_start: usize,
    decl_end: usize,
    scope_start: usize,
    scope_end: usize,
}

fn find_ident_bytes_after(source: &str, start: usize, name: &str) -> Option<(usize, usize)> {
    let start = start.min(source.len());
    let end = (start + 256).min(source.len());
    let haystack = &source[start..end];
    for (relative, _) in haystack.match_indices(name) {
        let at = start + relative;
        let before_ok = at == 0 || !source.as_bytes()[at - 1].is_ascii_alphanumeric() && source.as_bytes()[at - 1] != b'_';
        let after = at + name.len();
        let after_ok = after >= source.len() || !source.as_bytes()[after].is_ascii_alphanumeric() && source.as_bytes()[after] != b'_';
        if before_ok && after_ok {
            return Some((at, after));
        }
    }
    None
}

fn matching_brace(masked: &str, open: usize) -> Option<usize> {
    let bytes = masked.as_bytes();
    if bytes.get(open) != Some(&b'{') { return None; }
    let mut depth = 0usize;
    for (index, byte) in bytes.iter().enumerate().skip(open) {
        match *byte {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 { return Some(index); }
            }
            _ => {}
        }
    }
    None
}

fn next_block_bounds(masked: &str, from: usize) -> Option<(usize, usize)> {
    let relative = masked.get(from..)?.find('{')?;
    let open = from + relative;
    Some((open + 1, matching_brace(masked, open)?))
}

fn enclosing_block_bounds(masked: &str, offset: usize) -> Option<(usize, usize)> {
    let bytes = masked.as_bytes();
    let mut stack = Vec::new();
    let mut best = None;
    for (index, byte) in bytes.iter().enumerate() {
        if index > offset { break; }
        match *byte {
            b'{' => stack.push(index),
            b'}' => { stack.pop(); }
            _ => {}
        }
    }
    if let Some(open) = stack.last().copied() {
        best = matching_brace(masked, open).map(|close| (open + 1, close));
    }
    best
}

fn collect_local_bindings_from_stmts(
    source: &str,
    masked: &str,
    stmts: &[FuncStmt],
    out: &mut Vec<LocalBinding>,
) {
    for stmt in stmts {
        match stmt {
            FuncStmt::LocalVar(local) => {
                if let Some((decl_start, decl_end)) = find_ident_bytes_after(source, local.span.start, &local.name) {
                    let (scope_start, scope_end) = enclosing_block_bounds(masked, decl_start)
                        .unwrap_or((decl_start, source.len()));
                    out.push(LocalBinding { name: local.name.clone(), decl_start, decl_end, scope_start, scope_end });
                }
            }
            FuncStmt::If(if_stmt) => {
                collect_local_bindings_from_stmts(source, masked, &if_stmt.then_stmts, out);
                collect_local_bindings_from_stmts(source, masked, &if_stmt.else_stmts, out);
            }
            FuncStmt::For(statement) => {
                let binding_start = match &statement.binding {
                    spar::ast::ForBinding::Value { span, .. } => span.start,
                    spar::ast::ForBinding::Indexed { index_span, .. } => index_span.start,
                };
                let (scope_start, scope_end) = next_block_bounds(masked, binding_start)
                    .unwrap_or((binding_start, source.len()));
                match &statement.binding {
                    spar::ast::ForBinding::Value { name, span } => {
                        if let Some((decl_start, decl_end)) = find_ident_bytes_after(source, span.start, name) {
                            out.push(LocalBinding { name: name.clone(), decl_start, decl_end, scope_start, scope_end });
                        }
                    }
                    spar::ast::ForBinding::Indexed { index_name, index_span, value_name, value_span } => {
                        if let Some((decl_start, decl_end)) = find_ident_bytes_after(source, index_span.start, index_name) {
                            out.push(LocalBinding { name: index_name.clone(), decl_start, decl_end, scope_start, scope_end });
                        }
                        if let Some((decl_start, decl_end)) = find_ident_bytes_after(source, value_span.start, value_name) {
                            out.push(LocalBinding { name: value_name.clone(), decl_start, decl_end, scope_start, scope_end });
                        }
                    }
                }
                collect_local_bindings_from_stmts(source, masked, &statement.body, out);
            }
            FuncStmt::Try(statement) => {
                collect_local_bindings_from_stmts(source, masked, &statement.body, out);
                if let Some(name) = &statement.catch_name {
                    if let Some((decl_start, decl_end)) = find_ident_bytes_after(source, statement.catch_span.start, name) {
                        let (scope_start, scope_end) = next_block_bounds(masked, decl_end)
                            .unwrap_or((decl_end, source.len()));
                        out.push(LocalBinding { name: name.clone(), decl_start, decl_end, scope_start, scope_end });
                    }
                }
                collect_local_bindings_from_stmts(source, masked, &statement.handler, out);
            }
            FuncStmt::Assignment { .. }
            | FuncStmt::Expression(_, _)
            | FuncStmt::Return(_, _)
            | FuncStmt::Break(_)
            | FuncStmt::Continue(_) => {}
        }
    }
}

fn collect_function_local_bindings(
    source: &str,
    masked: &str,
    function: &spar::ast::FunctionDecl,
    out: &mut Vec<LocalBinding>,
) {
    let Some((scope_start, scope_end)) = next_block_bounds(masked, function.name_span.end) else {
        return;
    };
    for param in &function.params {
        if let Some((decl_start, decl_end)) = find_ident_bytes_after(source, param.span.start, &param.name) {
            out.push(LocalBinding {
                name: param.name.clone(),
                decl_start,
                decl_end,
                scope_start,
                scope_end,
            });
        }
    }
    collect_local_bindings_from_stmts(source, masked, &function.body.stmts, out);
}

fn local_bindings(state: &DocumentState) -> Vec<LocalBinding> {
    let Some(program) = raw_program_for_source(&state.source) else { return Vec::new(); };
    let masked = masked_code(&state.source);
    let mut out = Vec::new();
    for item in &program.items {
        match item {
            TopLevelItem::Function(function) => {
                collect_function_local_bindings(&state.source, &masked, function, &mut out);
            }
            TopLevelItem::FunctionGroup(group) => {
                for function in &group.functions {
                    collect_function_local_bindings(&state.source, &masked, function, &mut out);
                }
            }
            _ => {}
        }
    }
    out
}

fn local_binding_at_offset(state: &DocumentState, name: &str, offset: usize) -> Option<LocalBinding> {
    let mut candidates = local_bindings(state)
        .into_iter()
        .filter(|binding| binding.name == name)
        .filter(|binding| {
            (binding.decl_start <= offset && offset <= binding.decl_end)
                || (binding.scope_start <= offset && offset <= binding.scope_end && binding.decl_start <= offset)
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|binding| (binding.scope_end.saturating_sub(binding.scope_start), std::cmp::Reverse(binding.decl_start)));
    candidates.into_iter().next()
}

fn identifier_occurrences(source: &str, name: &str) -> Vec<(usize, Range)> {
    let masked = masked_code(source);
    let bytes = masked.as_bytes();
    let name_bytes = name.as_bytes();
    if name_bytes.is_empty() { return Vec::new(); }
    let mut out = Vec::new();
    let mut index = 0usize;
    while index + name_bytes.len() <= bytes.len() {
        if &bytes[index..index + name_bytes.len()] == name_bytes {
            let before_ok = index == 0 || (!bytes[index - 1].is_ascii_alphanumeric() && bytes[index - 1] != b'_');
            let after_at = index + name_bytes.len();
            let after_ok = after_at == bytes.len() || (!bytes[after_at].is_ascii_alphanumeric() && bytes[after_at] != b'_');
            if before_ok && after_ok {
                out.push((index, Range {
                    start: byte_offset_to_lsp_position(source, index),
                    end: byte_offset_to_lsp_position(source, after_at),
                }));
                index = after_at;
                continue;
            }
        }
        index += 1;
    }
    out
}

fn semantic_identifier_occurrences(state: &DocumentState, name: &str) -> Vec<(usize, Range)> {
    let mut out = identifier_occurrences(&state.source, name);

    // `masked_code` deliberately hides string contents. Spar expressions
    // embedded in strings/native-shell words are real semantic references,
    // though, so recover those from the compiler AST rather than exposing
    // arbitrary literal text to rename/highlight.
    if let Some(program) = &state.ast {
        let mut spans = Vec::new();
        collect_program_refs(program, RefTarget::Bare(name), &mut spans);
        for span in spans {
            let start = span.start.min(state.source.len());
            let end = span.end.min(state.source.len());
            let range = if state.source.get(start..end) == Some(name) {
                Range {
                    start: byte_offset_to_lsp_position(&state.source, start),
                    end: byte_offset_to_lsp_position(&state.source, end),
                }
            } else if let Some((ident_start, ident_end)) = find_ident_bytes_after(&state.source, start, name) {
                if ident_start >= end.saturating_add(1) {
                    continue;
                }
                Range {
                    start: byte_offset_to_lsp_position(&state.source, ident_start),
                    end: byte_offset_to_lsp_position(&state.source, ident_end),
                }
            } else {
                continue;
            };
            let byte = lsp_pos_to_byte_offset(&state.source, range.start);
            if !out.iter().any(|(existing, _)| *existing == byte) {
                out.push((byte, range));
            }
        }
    }

    out.sort_by_key(|(byte, _)| *byte);
    out
}

fn same_location_key(a: &Location, b: &Location) -> bool {
    a.uri == b.uri && a.range.start == b.range.start
}

fn semantic_target_at(
    uri: &Url,
    state: &DocumentState,
    pos: Position,
    index: &WorkspaceIndex,
) -> Option<SemanticTarget> {
    let name = word_at_position(&state.source, pos);
    if name.is_empty() { return None; }
    let offset = lsp_pos_to_byte_offset(&state.source, pos);
    if let Some(binding) = local_binding_at_offset(state, &name, offset) {
        let range = Range {
            start: byte_offset_to_lsp_position(&state.source, binding.decl_start),
            end: byte_offset_to_lsp_position(&state.source, binding.decl_end),
        };
        let declaration = Location { uri: uri.clone(), range };
        return Some(SemanticTarget {
            id: SymbolId(format!("{}#local:{}:{}", uri, name, binding.decl_start)),
            name,
            declaration: declaration.clone(),
            definition_key: declaration,
            local_decl_byte: Some(binding.decl_start),
        });
    }

    let definition_key = definition_at(uri, state, pos)?;
    let indexed = index
        .find_by_name(&name)
        .into_iter()
        .find(|symbol| symbol.uri == definition_key.uri);
    let (id, declaration) = if let Some(symbol) = indexed {
        (
            symbol.id.clone(),
            Location { uri: symbol.uri.clone(), range: symbol.selection_range },
        )
    } else {
        (
            SymbolId(format!("{}#definition:{}:{}:{}", definition_key.uri, name, definition_key.range.start.line, definition_key.range.start.character)),
            definition_key.clone(),
        )
    };
    Some(SemanticTarget { id, name, declaration, definition_key, local_decl_byte: None })
}

fn is_assignment_write(source: &str, range: Range) -> bool {
    let end = lsp_pos_to_byte_offset(source, range.end);
    let rest = source.get(end..).unwrap_or("").trim_start();
    rest.starts_with('=') && !rest.starts_with("==")
}

fn semantic_occurrences_in_document(
    uri: &Url,
    state: &DocumentState,
    target: &SemanticTarget,
) -> Vec<SemanticOccurrence> {
    let mut out = Vec::new();
    for (offset, range) in semantic_identifier_occurrences(state, &target.name) {
        let matches = if let Some(target_decl) = target.local_decl_byte {
            uri == &target.declaration.uri
                && local_binding_at_offset(state, &target.name, offset)
                    .is_some_and(|binding| binding.decl_start == target_decl)
        } else {
            let pos = range.start;
            definition_at(uri, state, pos)
                .is_some_and(|location| same_location_key(&location, &target.definition_key))
        };
        if !matches { continue; }
        let role = if uri == &target.declaration.uri && range.start == target.declaration.range.start {
            SemanticOccurrenceRole::Declaration
        } else if is_assignment_write(&state.source, range) {
            SemanticOccurrenceRole::Write
        } else {
            SemanticOccurrenceRole::Read
        };
        out.push(SemanticOccurrence { range, role });
    }
    out
}

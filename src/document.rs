// ── Document state ────────────────────────────────────────────────────────────

struct DocumentState {
    source: String,
    ast: Option<Program>,
    symbols: Option<SymbolTable>,
    import_symbols: HashMap<String, SymbolTable>,
    /// The file's own `Selective`/`TypeSelective`/`AsPartOf` import
    /// declarations, from a fresh (pre-splice) parse of `source` — the
    /// compiled `ast` no longer has these as distinct items, since
    /// `expand_imports` splices their targets' items directly into it.
    /// Lets `definition_at`/references redirect a spliced-in symbol to its
    /// true origin file instead of trusting the local (possibly retagged)
    /// span baked into `ast`/`symbols`.
    spliced_import_decls: Vec<spar::ast::ImportDecl>,
    /// One resolved `SymbolTable` per `spliced_import_decls` entry, keyed
    /// by that import's raw path string — the same shape `import_symbols`
    /// uses for `Aliased` imports, computed the same way.
    spliced_import_symbols: HashMap<String, SymbolTable>,
    #[allow(dead_code)]
    result: Option<EvalResult>,
    errors: Vec<SparError>,
    last_good_symbols: Option<SymbolTable>,
    last_good_import_symbols: HashMap<String, SymbolTable>,
}

impl DocumentState {
    fn diagnostics(&self) -> Vec<Diagnostic> {
        self.errors.iter().map(spar_error_to_diagnostic).collect()
    }

    fn effective_symbols(&self) -> Option<&SymbolTable> {
        self.symbols.as_ref().or(self.last_good_symbols.as_ref())
    }

    fn effective_import_symbols(&self) -> &HashMap<String, SymbolTable> {
        if !self.import_symbols.is_empty() {
            &self.import_symbols
        } else {
            &self.last_good_import_symbols
        }
    }
}

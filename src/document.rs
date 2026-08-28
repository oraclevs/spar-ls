// ── Document state ────────────────────────────────────────────────────────────

struct DocumentState {
    source: String,
    ast: Option<Program>,
    symbols: Option<SymbolTable>,
    import_symbols: HashMap<String, SymbolTable>,
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

// ── LSP Server ────────────────────────────────────────────────────────────────

struct SparLanguageServer {
    client: Client,
    documents: Mutex<HashMap<Url, DocumentState>>,
    importers: Mutex<HashMap<PathBuf, HashSet<PathBuf>>>,
    workspace_root: Mutex<Option<PathBuf>>,
}

impl SparLanguageServer {
    fn analyze(source: &str, base_dir: &std::path::Path) -> DocumentState {
        let compilation = Compiler::new(CompileOptions {
            base_dir: base_dir.to_path_buf(),
            ..CompileOptions::default()
        })
        .compile(source);
        Self::document_state(source, base_dir, compilation)
    }

    fn document_state(
        source: &str,
        base_dir: &std::path::Path,
        compilation: Compilation,
    ) -> DocumentState {
        let mut import_symbols = HashMap::new();
        for (alias, loaded) in &compilation.imports {
            let full_path = base_dir.join(&loaded.path);
            let Ok(import_source) = std::fs::read_to_string(full_path) else {
                continue;
            };
            let imported = Compiler::new(CompileOptions {
                base_dir: base_dir.to_path_buf(),
                evaluate: false,
                ..CompileOptions::default()
            })
            .compile(&import_source);
            if let Some(symbols) = imported.symbols {
                import_symbols.insert(alias.clone(), symbols);
            }
        }

        // A fresh, pre-splice parse of `source` to recover the
        // Selective/TypeSelective/AsPartOf import declarations
        // `expand_imports` already consumed out of `compilation.program` —
        // needed so go-to-definition/references can redirect a spliced-in
        // symbol to its real origin file/line instead of the local
        // (possibly retagged) span baked into the compiled AST.
        let mut spliced_import_decls = Vec::new();
        let mut spliced_import_symbols = HashMap::new();
        if let Ok(tokens) = Lexer::new(source).tokenize() {
            if let Ok(raw_program) = Parser::new(tokens).parse() {
                for item in raw_program.items {
                    let spar::ast::TopLevelItem::Import(decl) = item else {
                        continue;
                    };
                    if matches!(
                        decl.kind,
                        spar::ast::ImportKind::Aliased(_) | spar::ast::ImportKind::Schema
                    ) {
                        continue;
                    }
                    let full_path = base_dir.join(&decl.path);
                    if let Ok(target_source) = std::fs::read_to_string(&full_path) {
                        let target = Compiler::new(CompileOptions {
                            base_dir: base_dir.to_path_buf(),
                            evaluate: false,
                            ..CompileOptions::default()
                        })
                        .compile(&target_source);
                        if let Some(symbols) = target.symbols {
                            spliced_import_symbols.insert(decl.path.clone(), symbols);
                        }
                    }
                    spliced_import_decls.push(decl);
                }
            }
        }

        DocumentState {
            source: source.to_string(),
            ast: compilation.program,
            symbols: compilation.symbols,
            import_symbols,
            spliced_import_decls,
            spliced_import_symbols,
            result: compilation.result,
            errors: compilation.errors,
            last_good_symbols: None,
            last_good_import_symbols: HashMap::new(),
        }
    }

    #[allow(dead_code)]
    fn analyze_legacy(source: &str, base_dir: &std::path::Path) -> DocumentState {
        let mut all_errors: Vec<SparError> = Vec::new();

        let tokens = match Lexer::new(source).tokenize() {
            Ok(t) => t,
            Err(e) => {
                all_errors.push(e);
                return DocumentState {
                    source: source.to_string(),
                    ast: None,
                    symbols: None,
                    import_symbols: HashMap::new(),
                    spliced_import_decls: Vec::new(),
                    spliced_import_symbols: HashMap::new(),
                    result: None,
                    errors: all_errors,
                    last_good_symbols: None,
                    last_good_import_symbols: HashMap::new(),
                };
            }
        };

        let mut program = match Parser::new(tokens).parse() {
            Ok(p) => p,
            Err(e) => {
                all_errors.push(e);
                return DocumentState {
                    source: source.to_string(),
                    ast: None,
                    symbols: None,
                    import_symbols: HashMap::new(),
                    spliced_import_decls: Vec::new(),
                    spliced_import_symbols: HashMap::new(),
                    result: None,
                    errors: all_errors,
                    last_good_symbols: None,
                    last_good_import_symbols: HashMap::new(),
                };
            }
        };

        // Splice selective / import type / asPartOf imports into local scope
        // before anything else touches `program` — same ordering as the CLI.
        let mut expand_loader = ImportLoader::new(base_dir);
        if let Err(e) = expand_imports(&mut program, &mut expand_loader) {
            all_errors.extend(e);
            return analyze_single_file(source, program, all_errors);
        }

        let mut loader = ImportLoader::new(base_dir);
        let imports = match collect_imports(&program, &mut loader) {
            Ok(i) => i,
            Err(e) => {
                all_errors.extend(e);
                return analyze_single_file(source, program, all_errors);
            }
        };

        // Resolve each imported file's full symbols for hover and completion.
        // Done here (before main resolver) so it's populated even if main resolve fails.
        let mut import_symbols: HashMap<String, SymbolTable> = HashMap::new();
        for (alias, loaded) in &imports {
            let full_path = base_dir.join(&loaded.path);
            if let Ok(imp_src) = std::fs::read_to_string(&full_path) {
                if let Ok(tokens) = Lexer::new(&imp_src).tokenize() {
                    if let Ok(prog) = Parser::new(tokens).parse() {
                        if let Ok(isym) = Resolver::new().resolve(&prog, &[]) {
                            import_symbols.insert(alias.clone(), isym);
                        }
                    }
                }
            }
        }

        let sym = match Resolver::resolve_with_imports(&program, &imports) {
            Ok(s) => s,
            Err(e) => {
                all_errors.extend(e);
                return DocumentState {
                    source: source.to_string(),
                    ast: Some(program),
                    symbols: None,
                    import_symbols,
                    spliced_import_decls: Vec::new(),
                    spliced_import_symbols: HashMap::new(),
                    result: None,
                    errors: all_errors,
                    last_good_symbols: None,
                    last_good_import_symbols: HashMap::new(),
                };
            }
        };

        let schema_bindings = match validate_schema_imports(&program, base_dir) {
            Ok(b) => b,
            Err(e) => {
                all_errors.extend(e);
                HashMap::new()
            }
        };

        if let Err(e) = TypeChecker::check_with_schema(&program, &sym, schema_bindings) {
            all_errors.extend(e);
        }

        let eval_result = if all_errors.is_empty() {
            match Evaluator::evaluate_with_imports_and_base(&program, &sym, &imports, base_dir) {
                Ok(r) => Some(r),
                Err(e) => {
                    all_errors.extend(e);
                    None
                }
            }
        } else {
            None
        };

        DocumentState {
            source: source.to_string(),
            ast: Some(program),
            symbols: Some(sym),
            import_symbols,
            spliced_import_decls: Vec::new(),
            spliced_import_symbols: HashMap::new(),
            result: eval_result,
            errors: all_errors,
            last_good_symbols: None,
            last_good_import_symbols: HashMap::new(),
        }
    }

    async fn publish_diagnostics(&self, uri: Url, diags: Vec<Diagnostic>) {
        self.client.publish_diagnostics(uri, diags, None).await;
    }

    /// Re-analyse all direct importers of `canon` and push fresh diagnostics.
    /// Called from both did_save and did_change (when parse succeeds).
    async fn propagate_to_importers(&self, canon: &PathBuf) {
        let importers_snapshot = {
            let map = self.importers.lock().await;
            map.get(canon).cloned().unwrap_or_default()
        };
        for importer_path in importers_snapshot {
            let Ok(src2) = std::fs::read_to_string(&importer_path) else {
                continue;
            };
            let base2 = importer_path.parent().unwrap_or(std::path::Path::new("."));
            let state2 = Self::analyze(&src2, base2);
            let Ok(uri2) = Url::from_file_path(&importer_path) else {
                continue;
            };
            let diags2 = state2.diagnostics();
            self.publish_diagnostics(uri2, diags2).await;
        }
    }

    /// Updates the importers reverse map when `file_path` is opened/saved.
    /// `file_path` is the canonical absolute path of the file being analyzed.
    /// `imports` is the list of files it imports (from `extract_imported_paths`).
    async fn update_importers(&self, file_path: &PathBuf, imports: &[PathBuf]) {
        let mut map = self.importers.lock().await;
        // Remove this file from all existing importer sets (stale entries).
        for set in map.values_mut() {
            set.remove(file_path);
        }
        // Add fresh entries.
        for imported in imports {
            map.entry(imported.clone())
                .or_default()
                .insert(file_path.clone());
        }
    }

    async fn scan_workspace(&self, root: &std::path::Path) {
        use std::fs;
        fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().and_then(|e| e.to_str()) == Some("spar") {
                    out.push(path);
                }
            }
        }
        let mut files = Vec::new();
        walk(root, &mut files);
        for path in files {
            let Ok(src) = std::fs::read_to_string(&path) else {
                continue;
            };
            let base = path.parent().unwrap_or(root);
            let state = Self::analyze(&src, base);
            if let Some(program) = &state.ast {
                if let Ok(canon) = path.canonicalize() {
                    let imports = extract_imported_paths(program, base);
                    self.update_importers(&canon, &imports).await;
                }
            }
            let Ok(uri) = Url::from_file_path(&path) else {
                continue;
            };
            let diags = state.diagnostics();
            self.publish_diagnostics(uri, diags).await;
        }
    }
}


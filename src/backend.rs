// ── LSP Server ────────────────────────────────────────────────────────────────

struct SparLanguageServer {
    client: Client,
    documents: Mutex<HashMap<Url, DocumentState>>,
    importers: Mutex<HashMap<PathBuf, HashSet<PathBuf>>>,
    workspace_root: Mutex<Option<PathBuf>>,
}

impl SparLanguageServer {
    fn attach_package_locator(options: &mut CompileOptions, base_dir: &std::path::Path) {
        let project_dir = base_dir
            .ancestors()
            .find(|directory| directory.join(spar::package::PACKAGE_MANIFEST_FILE).is_file());
        if let Some(project_dir) = project_dir {
            let lock_path = project_dir.join(spar::package::PACKAGE_LOCK_FILE);
            if let Ok(lockfile) = spar::package::Lockfile::read(&lock_path) {
                let store =
                    spar::package::PackageStore::new(spar::package::StorePaths::from_env());
                options.locator = Some(spar::package::ModuleLocator::for_root(lockfile, store));
            }
        }
    }

    fn compile_options(base_dir: &std::path::Path) -> CompileOptions {
        let mut options = CompileOptions {
            base_dir: base_dir.to_path_buf(),
            ..CompileOptions::default()
        };
        Self::attach_package_locator(&mut options, base_dir);
        options
    }

    fn compile_options_for_path(path: &std::path::Path) -> CompileOptions {
        let mut options = CompileOptions::for_path(path);
        Self::attach_package_locator(
            &mut options,
            path.parent().unwrap_or(std::path::Path::new(".")),
        );
        options
    }

    fn analyze(source: &str, base_dir: &std::path::Path) -> DocumentState {
        let compilation = Compiler::new(Self::compile_options(base_dir)).compile(source);
        Self::document_state(source, base_dir, compilation)
    }

    fn analyze_path(source: &str, path: &std::path::Path) -> DocumentState {
        let base_dir = path.parent().unwrap_or(std::path::Path::new("."));
        let compilation = Compiler::new(Self::compile_options_for_path(path)).compile(source);
        Self::document_state(source, base_dir, compilation)
    }

    fn document_state(
        source: &str,
        base_dir: &std::path::Path,
        compilation: Compilation,
    ) -> DocumentState {
        let mut import_symbols = HashMap::new();
        let mut import_paths = HashMap::new();
        for (alias, loaded) in &compilation.imports {
            import_paths.insert(alias.clone(), loaded.resolved_path.clone());
            let full_path = &loaded.resolved_path;
            let Ok(import_source) = std::fs::read_to_string(full_path) else {
                continue;
            };
            let mut options = CompileOptions::for_path(full_path);
            options.evaluate = false;
            options.locator = loaded.locator.clone();
            let imported = Compiler::new(options).compile(&import_source);
            if let Some(symbols) = imported.symbols {
                import_symbols.insert(alias.clone(), symbols);
            }
        }

        // A fresh, pre-splice parse of `source` to recover the
        // Selective/TypeSelective import declarations
        // `expand_imports` already consumed out of `compilation.program` —
        // needed so go-to-definition/references can redirect a spliced-in
        // symbol to its real origin file/line instead of the local
        // (possibly retagged) span baked into the compiled AST.
        let mut spliced_import_decls = Vec::new();
        let mut spliced_import_symbols = HashMap::new();
        let mut spliced_import_paths = HashMap::new();
        let resolution_options = Self::compile_options(base_dir);
        let mut resolution_loader = ImportLoader::new(base_dir);
        if let Some(locator) = resolution_options.locator {
            resolution_loader = resolution_loader.with_locator(locator);
        }
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
                    if let Ok(resolved) = resolution_loader.resolve_import(&decl) {
                        let full_path = resolved.path;
                        spliced_import_paths.insert(decl.path.clone(), full_path.clone());
                        if let Ok(target_source) = std::fs::read_to_string(&full_path) {
                            let mut options = CompileOptions::for_path(&full_path);
                            options.evaluate = false;
                            options.locator = resolved.locator;
                            let target = Compiler::new(options).compile(&target_source);
                            if let Some(symbols) = target.symbols {
                                spliced_import_symbols.insert(decl.path.clone(), symbols);
                            }
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
            import_paths,
            spliced_import_decls,
            spliced_import_symbols,
            spliced_import_paths,
            result: compilation.result,
            errors: compilation.errors,
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
            let state2 = Self::analyze_path(&src2, &importer_path);
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
            let state = Self::analyze_path(&src, &path);
            if let Some(program) = &state.ast {
                if let Ok(canon) = path.canonicalize() {
                    let imports = extract_imported_paths(&state);
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

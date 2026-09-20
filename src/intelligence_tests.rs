use super::*;

#[test]
fn minimal_client_support_does_not_assume_optional_resolve_features() {
    let params = InitializeParams::default();
    let support = ClientFeatureSupport::from_initialize(&params);
    assert!(!support.completion_resolve_documentation);
    assert!(!support.hierarchical_document_symbols);
    assert!(!support.workspace_symbol_resolve);
    assert!(!support.code_action_resolve);
    assert!(!support.watched_files_dynamic_registration);
}

#[test]
fn rich_client_support_detects_standard_lsp_optional_capabilities() {
    let capabilities = serde_json::json!({
        "textDocument": {
            "completion": {
                "completionItem": {
                    "snippetSupport": true,
                    "resolveSupport": { "properties": ["documentation", "detail"] }
                }
            },
            "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
            "codeAction": { "resolveSupport": { "properties": ["edit"] } }
        },
        "workspace": {
            "symbol": { "resolveSupport": { "properties": ["location"] } },
            "didChangeWatchedFiles": { "dynamicRegistration": true }
        }
    });
    let params = InitializeParams {
        capabilities: serde_json::from_value(capabilities).expect("capabilities"),
        ..InitializeParams::default()
    };
    let support = ClientFeatureSupport::from_initialize(&params);
    assert!(support.completion_resolve_documentation);
    assert!(support.completion_resolve_detail);
    assert!(support.completion_snippets);
    assert!(support.hierarchical_document_symbols);
    assert!(support.workspace_symbol_resolve);
    assert!(support.code_action_resolve);
    assert!(support.watched_files_dynamic_registration);
}

#[test]
fn workspace_index_preserves_exports_and_callable_metadata() {
    let source = concat!(
        "function build(input: str, mode: str = \"debug\") -> int { return 0; };\n",
        "private function secret() -> int { return 1; };\n",
        "export type Config { name: str; };\n",
        "export enum Mode { Fast, Safe };\n",
    );
    let uri = Url::parse("file:///workspace/main.spar").unwrap();
    let state = SparLanguageServer::analyze(source, std::path::Path::new("/workspace"));
    let mut index = WorkspaceIndex::default();
    index.replace_document(&uri, &state);
    let exports = index.exports_for_uri(&uri);
    assert!(exports.iter().any(|symbol| symbol.name == "build"));
    assert!(!exports.iter().any(|symbol| symbol.name == "secret"));
    let build = exports.iter().find(|symbol| symbol.name == "build").unwrap();
    let signature = build.signature.as_ref().unwrap();
    assert_eq!(signature.params[1].name, "mode");
    assert!(signature.params[1].has_default);
    assert_eq!(signature.params[1].default_repr.as_deref(), Some("\"debug\""));
}

fn context_from_marked(source: &str) -> EditorContext {
    let offset = source.find('|').expect("cursor marker");
    let clean = source.replacen('|', "", 1);
    editor_context(&clean, None, offset)
}

#[test]
fn editor_context_recovers_selective_imports_and_paths() {
    assert!(matches!(
        context_from_marked("import { rea| } from \"std/fs\";"),
        EditorContext::SelectiveImport { type_only: false, .. }
    ));
    assert!(matches!(
        context_from_marked("import type { Con| } from \"./types.spar\";"),
        EditorContext::SelectiveImport { type_only: true, .. }
    ));
    assert!(matches!(
        context_from_marked("import pkg { x } from \"std/f|\";"),
        EditorContext::ImportPath { package: true, .. }
    ));
    assert!(matches!(context_from_marked("// import { | }"), EditorContext::Suppressed));
}

#[test]
fn editor_context_recovers_incomplete_call_arguments() {
    let context = context_from_marked("vidShrink(input: file, |");
    let EditorContext::CallArguments { callee, supplied, active_parameter } = context else {
        panic!("expected call context");
    };
    assert_eq!(callee, "vidShrink");
    assert_eq!(supplied, vec!["input"]);
    assert_eq!(active_parameter, 1);
}

#[test]
fn import_completion_filters_type_only_private_and_duplicates() {
    let temp = tempfile::tempdir().unwrap();
    let module = temp.path().join("types.spar");
    std::fs::write(&module, concat!(
        "export type Config { name: str; };\n",
        "export enum Mode { Fast, Safe };\n",
        "function build(input: str) -> int { return 0; };\n",
        "private function secret() -> int { return 0; };\n",
    )).unwrap();
    let mut already = HashSet::new();
    already.insert("Mode".to_string());
    let items = selective_import_completion_items(temp.path(), "types.spar", false, true, &already).unwrap();
    let labels = items.into_iter().map(|item| item.label).collect::<Vec<_>>();
    assert!(labels.contains(&"Config".to_string()));
    assert!(!labels.contains(&"Mode".to_string()));
    assert!(!labels.contains(&"build".to_string()));
    assert!(!labels.contains(&"secret".to_string()));
}

#[test]
fn selective_import_completion_exposes_only_target_owned_exports() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("dep.spar"),
        "function helper() -> int { return 1; };\n",
    )
    .unwrap();
    std::fs::write(
        temp.path().join("api.spar"),
        concat!(
            "import { helper } from \"dep.spar\";\n",
            "function build() -> int { return helper(); };\n",
        ),
    )
    .unwrap();

    let items = selective_import_completion_items(
        temp.path(),
        "api.spar",
        false,
        false,
        &HashSet::new(),
    )
    .unwrap();
    let labels = items.into_iter().map(|item| item.label).collect::<Vec<_>>();
    assert!(labels.contains(&"build".to_string()));
    assert!(
        !labels.contains(&"helper".to_string()),
        "transitively imported symbols must not be re-exported by completion"
    );
}

#[test]
fn package_selective_import_completion_uses_lockfile_exports() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    let package = temp.path().join("toolkit");
    std::fs::create_dir_all(package.join("src")).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join(spar::package::PACKAGE_MANIFEST_FILE), "// fixture\n").unwrap();
    std::fs::write(
        package.join("src/lib.spar"),
        concat!(
            "function readText(path: str) -> str { return path; };\n",
            "private function secret() -> int { return 0; };\n",
            "export type Options { encoding: str; };\n",
        ),
    )
    .unwrap();

    let package_id = "path-toolkit".to_string();
    let mut lockfile = spar::package::Lockfile::default();
    lockfile.root.insert("toolkit".to_string(), package_id.clone());
    lockfile.packages.insert(
        package_id,
        spar::package::LockedPackage {
            name: "toolkit".to_string(),
            version: "1.0.0".to_string(),
            source: spar::package::LockedSource::Path {
                path: package.to_string_lossy().into_owned(),
            },
            integrity: None,
            entry: "src/lib.spar".to_string(),
            dependencies: std::collections::BTreeMap::new(),
        },
    );
    lockfile
        .write_atomically(&project.join(spar::package::PACKAGE_LOCK_FILE))
        .unwrap();

    let values = selective_import_completion_items(
        &project,
        "toolkit",
        true,
        false,
        &HashSet::new(),
    )
    .unwrap();
    let labels = values.into_iter().map(|item| item.label).collect::<Vec<_>>();
    assert!(labels.contains(&"readText".to_string()));
    assert!(labels.contains(&"Options".to_string()));
    assert!(!labels.contains(&"secret".to_string()));
}

#[test]
fn import_path_completion_lists_only_spar_modules() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("utils.spar"), "var x: int = 1;").unwrap();
    std::fs::write(temp.path().join("utility.txt"), "no").unwrap();
    let items = import_path_completion_items(temp.path(), "./ut", false);
    let labels = items.into_iter().map(|item| item.label).collect::<Vec<_>>();
    assert!(labels.contains(&"./utils.spar".to_string()));
    assert!(!labels.iter().any(|label| label.ends_with(".txt")));
}

#[test]
fn signature_help_tracks_required_and_default_parameters() {
    let declaration =
        "function vidShrink(input: str, output: str, targetMb: float = 9.3) -> shell { return shell {}; };\n";
    let source = "vidShrink(input: \"in\", ";
    let uri = Url::parse("file:///workspace/main.spar").unwrap();
    let state = SparLanguageServer::analyze(declaration, std::path::Path::new("/workspace"));
    let mut index = WorkspaceIndex::default();
    index.replace_document(&uri, &state);
    let help = signature_help_at(&state, &index, &uri, source, source.len()).expect("signature help");
    assert!(help.signatures[0].label.contains("targetMb: float = 9.3"));
    assert_eq!(help.active_parameter, Some(1));
}

#[test]
fn named_argument_completion_omits_supplied_and_sorts_required_first() {
    let valid = "function deploy(input: str, output: str, mode: str = \"debug\") -> int { return 0; };\n";
    let uri = Url::parse("file:///workspace/main.spar").unwrap();
    let state = SparLanguageServer::analyze(valid, std::path::Path::new("/workspace"));
    let mut index = WorkspaceIndex::default();
    index.replace_document(&uri, &state);
    let source = "deploy(input: \"x\", ";
    let items = named_argument_completion_items(&state, &index, &uri, source, source.len()).unwrap();
    assert_eq!(items[0].label, "output:");
    assert!(items.iter().any(|item| item.label == "mode:"));
    assert!(!items.iter().any(|item| item.label == "input:"));
}

#[test]
fn document_symbol_response_has_hierarchy_and_flat_fallback() {
    let source = concat!(
        "export type Config { name: str; };\n",
        "function build(input: str) -> int { return 0; };\n",
        "export enum Mode { Fast, Safe };\n",
    );
    let uri = Url::parse("file:///workspace/main.spar").unwrap();
    let state = SparLanguageServer::analyze(source, std::path::Path::new("/workspace"));
    let nested = document_symbol_response(&uri, &state, true);
    let DocumentSymbolResponse::Nested(symbols) = nested else { panic!("expected nested symbols"); };
    assert!(symbols.iter().any(|symbol| symbol.name == "Config" && symbol.children.as_ref().is_some_and(|children| children.iter().any(|child| child.name == "name"))));
    let flat = document_symbol_response(&uri, &state, false);
    let DocumentSymbolResponse::Flat(symbols) = flat else { panic!("expected flat symbols"); };
    assert!(symbols.iter().any(|symbol| symbol.name == "input" && symbol.container_name.as_deref() == Some("build")));
}

#[test]
fn workspace_symbol_query_is_cached_deterministic_and_private_filtered() {
    let uri = Url::parse("file:///workspace/main.spar").unwrap();
    let source = concat!(
        "function buildApp() -> int { return 0; };\n",
        "private function buildSecret() -> int { return 0; };\n",
    );
    let state = SparLanguageServer::analyze(source, std::path::Path::new("/workspace"));
    let mut index = WorkspaceIndex::default();
    index.replace_document(&uri, &state);
    let symbols = workspace_symbols(&index, "build");
    assert!(symbols.iter().any(|symbol| symbol.name == "buildApp"));
    assert!(!symbols.iter().any(|symbol| symbol.name == "buildSecret"));
}

#[test]
fn document_highlight_respects_local_shadowing() {
    let source = concat!(
        "export var value: int = 1;\n",
        "function first(value: int) -> int { return value; };\n",
        "function second() -> int { var value: int = 2; return value; };\n",
    );
    let uri = Url::parse("file:///workspace/main.spar").unwrap();
    let state = SparLanguageServer::analyze(source, std::path::Path::new("/workspace"));
    let mut index = WorkspaceIndex::default();
    index.replace_document(&uri, &state);
    let call_offset = source.find("return value; };\nfunction second").unwrap() + "return ".len();
    let pos = byte_offset_to_lsp_position(source, call_offset);
    let target = semantic_target_at(&uri, &state, pos, &index).expect("semantic target");
    assert!(target.local_decl_byte.is_some());
    let occurrences = semantic_occurrences_in_document(&uri, &state, &target);
    assert_eq!(occurrences.len(), 2, "parameter declaration + its return use only");
    assert!(occurrences.iter().any(|occurrence| occurrence.role == SemanticOccurrenceRole::Declaration));
    assert!(occurrences.iter().any(|occurrence| occurrence.role == SemanticOccurrenceRole::Read));
}

#[test]
fn document_highlight_tracks_selective_import_origin() {
    let temp = tempfile::tempdir().unwrap();
    let lib = temp.path().join("lib.spar");
    let main = temp.path().join("main.spar");
    std::fs::write(&lib, "function build() -> int { return 1; };\n").unwrap();
    let source = "import { build } from \"lib.spar\";\nfunction run() -> int { return build(); };\n";
    std::fs::write(&main, source).unwrap();
    let state = SparLanguageServer::analyze_path(source, &main);
    let lib_source = std::fs::read_to_string(&lib).unwrap();
    let lib_state = SparLanguageServer::analyze_path(&lib_source, &lib);
    let uri = Url::from_file_path(&main).unwrap();
    let lib_uri = Url::from_file_path(&lib).unwrap();
    let mut index = WorkspaceIndex::default();
    index.replace_document(&uri, &state);
    index.replace_document(&lib_uri, &lib_state);
    let offset = source.rfind("build()").unwrap();
    let target = semantic_target_at(&uri, &state, byte_offset_to_lsp_position(source, offset), &index).unwrap();
    assert_eq!(target.declaration.uri, lib_uri);
    let occurrences = semantic_occurrences_in_document(&uri, &state, &target);
    assert!(occurrences.len() >= 2, "import item and call should share identity");
}

#[test]
fn document_highlight_includes_native_shell_interpolation() {
    let source = concat!(
        "function show(input: str) -> shell {\n",
        "    return shell { echo \"${input}\"; };\n",
        "};\n",
    );
    let uri = Url::parse("file:///workspace/main.spar").unwrap();
    let state = SparLanguageServer::analyze(source, std::path::Path::new("/workspace"));
    let mut index = WorkspaceIndex::default();
    index.replace_document(&uri, &state);
    let use_byte = source.rfind("input").unwrap();
    let target = semantic_target_at(
        &uri,
        &state,
        byte_offset_to_lsp_position(source, use_byte),
        &index,
    )
    .expect("semantic target");
    let occurrences = semantic_occurrences_in_document(&uri, &state, &target);
    assert_eq!(occurrences.len(), 2, "parameter declaration + shell interpolation use");
}

#[test]
fn semantic_occurrences_follow_exported_symbol_across_files() {
    let temp = tempfile::tempdir().unwrap();
    let lib = temp.path().join("lib.spar");
    let main = temp.path().join("main.spar");
    let lib_source = "function build() -> int { return 1; };\n";
    let main_source = "import { build } from \"lib.spar\";\nfunction run() -> int { return build(); };\n";
    std::fs::write(&lib, lib_source).unwrap();
    std::fs::write(&main, main_source).unwrap();
    let lib_uri = Url::from_file_path(&lib).unwrap();
    let main_uri = Url::from_file_path(&main).unwrap();
    let lib_state = SparLanguageServer::analyze_path(lib_source, &lib);
    let main_state = SparLanguageServer::analyze_path(main_source, &main);
    let mut index = WorkspaceIndex::default();
    index.replace_document(&lib_uri, &lib_state);
    index.replace_document(&main_uri, &main_state);

    let call_byte = main_source.rfind("build").unwrap();
    let target = semantic_target_at(
        &main_uri,
        &main_state,
        byte_offset_to_lsp_position(main_source, call_byte),
        &index,
    )
    .expect("target");
    assert_eq!(target.declaration.uri, lib_uri);
    let lib_occurrences = semantic_occurrences_in_document(&lib_uri, &lib_state, &target);
    let main_occurrences = semantic_occurrences_in_document(&main_uri, &main_state, &target);
    assert_eq!(lib_occurrences.len(), 1, "declaration");
    assert_eq!(main_occurrences.len(), 2, "selective import + call");
}

#[test]
fn prepare_rename_rejects_bundled_stdlib_declaration() {
    let path = spar::resolve_bundled_stdlib_import("std/fs").expect("std/fs");
    let uri = Url::from_file_path(&path).unwrap();
    let target = SemanticTarget {
        id: SymbolId("std-readText".to_string()),
        name: "readText".to_string(),
        declaration: Location::new(uri.clone(), Range::default()),
        definition_key: Location::new(uri, Range::default()),
        local_decl_byte: None,
    };
    assert!(!declaration_is_editable(&target, Some(std::path::Path::new("/workspace"))));
}

#[test]
fn rename_name_validation_tracks_symbol_kind() {
    let uri = Url::parse("file:///workspace/main.spar").unwrap();
    let symbol = IndexedSymbol {
        id: SymbolId("type".into()),
        name: "Config".into(),
        semantic_path: "Config".into(),
        kind: IndexedSymbolKind::Type,
        uri,
        range: Range::default(),
        selection_range: Range::default(),
        container_name: None,
        exported: true,
        private: false,
        signature: None,
        detail: None,
    };
    assert!(is_valid_rename(Some(&symbol), "ServerConfig"));
    assert!(!is_valid_rename(Some(&symbol), "serverConfig"));
}

#[test]
fn auto_import_candidate_and_new_import_edit_are_portable() {
    let temp = tempfile::tempdir().unwrap();
    let lib = temp.path().join("lib.spar");
    let main = temp.path().join("main.spar");
    let lib_source = "function writeText(path: str, content: str) -> int { return 0; };\n";
    std::fs::write(&lib, lib_source).unwrap();
    std::fs::write(&main, "var result: int = writeText(path: \"x\", content: \"y\");\n").unwrap();
    let lib_uri = Url::from_file_path(&lib).unwrap();
    let main_uri = Url::from_file_path(&main).unwrap();
    let lib_state = SparLanguageServer::analyze_path(lib_source, &lib);
    let mut index = WorkspaceIndex::default();
    index.replace_document(&lib_uri, &lib_state);
    let candidates = auto_import_candidates(&index, "writeText", &main_uri, false);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].import_path, "./lib.spar");
    let edit = build_import_workspace_edit("var x: int = 1;\n", &main_uri, &candidates[0]);
    let mut changes = edit.changes.unwrap();
    let edits = changes.remove(&main_uri).unwrap();
    assert!(edits[0].new_text.contains("import { writeText } from \"./lib.spar\";"));
}

#[test]
fn auto_import_candidate_uses_stdlib_package_path_instead_of_filesystem_path() {
    let temp = tempfile::tempdir().unwrap();
    let main = temp.path().join("main.spar");
    std::fs::write(&main, "var x: str = readText(\"a.txt\");\n").unwrap();
    let main_uri = Url::from_file_path(&main).unwrap();

    let std_path = spar::resolve_bundled_stdlib_import("std/fs").expect("std/fs path");
    let std_source = std::fs::read_to_string(&std_path).unwrap();
    let std_uri = Url::from_file_path(&std_path).unwrap();
    let std_state = SparLanguageServer::analyze_path(&std_source, &std_path);
    let mut index = WorkspaceIndex::default();
    index.replace_document(&std_uri, &std_state);

    let candidates = auto_import_candidates(&index, "readText", &main_uri, false);
    assert!(candidates.iter().any(|candidate| {
        candidate.import_path == "std/fs" && candidate.package && candidate.name == "readText"
    }));
}

#[test]
fn auto_import_candidate_maps_dependency_submodule_to_package_request() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    let package = temp.path().join("toolkit");
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::create_dir_all(package.join("src")).unwrap();
    std::fs::write(project.join(spar::package::PACKAGE_MANIFEST_FILE), "// fixture\n").unwrap();
    std::fs::write(package.join("src/lib.spar"), "function root() -> int { return 0; };\n").unwrap();
    let module = package.join("src/io.spar");
    let module_source = "function writeText(path: str) -> int { return 0; };\n";
    std::fs::write(&module, module_source).unwrap();
    let current = project.join("src/main.spar");
    std::fs::write(&current, "var x: int = writeText(path: \"x\");\n").unwrap();

    let package_id = "path-toolkit".to_string();
    let mut lockfile = spar::package::Lockfile::default();
    lockfile.root.insert("toolkit".to_string(), package_id.clone());
    lockfile.packages.insert(
        package_id,
        spar::package::LockedPackage {
            name: "toolkit".to_string(),
            version: "1.0.0".to_string(),
            source: spar::package::LockedSource::Path {
                path: package.to_string_lossy().into_owned(),
            },
            integrity: None,
            entry: "src/lib.spar".to_string(),
            dependencies: std::collections::BTreeMap::new(),
        },
    );
    lockfile
        .write_atomically(&project.join(spar::package::PACKAGE_LOCK_FILE))
        .unwrap();

    let module_uri = Url::from_file_path(&module).unwrap();
    let current_uri = Url::from_file_path(&current).unwrap();
    let module_state = SparLanguageServer::analyze_path(module_source, &module);
    let mut index = WorkspaceIndex::default();
    index.replace_document(&module_uri, &module_state);

    let candidates = auto_import_candidates(&index, "writeText", &current_uri, false);
    assert!(candidates.iter().any(|candidate| {
        candidate.import_path == "toolkit/io" && candidate.package && candidate.name == "writeText"
    }));
}

#[test]
fn import_edit_merges_compatible_import_without_duplicate() {
    let uri = Url::parse("file:///workspace/main.spar").unwrap();
    let candidate = AutoImportCandidate {
        symbol_id: SymbolId("x".into()),
        import_path: "./lib.spar".into(),
        package: false,
        type_only: false,
        name: "writeText".into(),
    };
    let source = "import { readText } from \"./lib.spar\";\nvar x: int = 1;\n";
    let edit = build_import_workspace_edit(source, &uri, &candidate);
    let mut changes = edit.changes.unwrap();
    let edits = changes.remove(&uri).unwrap();
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].new_text, ", writeText");
}


#[test]
fn auto_import_code_action_requires_matching_unresolved_diagnostic() {
    let temp = tempfile::tempdir().unwrap();
    let lib = temp.path().join("lib.spar");
    let main = temp.path().join("main.spar");
    let lib_source = "function writeText(path: str, content: str) -> int { return 0; };\n";
    let main_source = "var result: int = writeText(path: \"x\", content: \"y\");\n";
    std::fs::write(&lib, lib_source).unwrap();
    std::fs::write(&main, main_source).unwrap();

    let lib_uri = Url::from_file_path(&lib).unwrap();
    let main_uri = Url::from_file_path(&main).unwrap();
    let lib_state = SparLanguageServer::analyze_path(lib_source, &lib);
    let main_state = SparLanguageServer::analyze_path(main_source, &main);
    let mut index = WorkspaceIndex::default();
    index.replace_document(&lib_uri, &lib_state);

    let start_byte = main_source.find("writeText").unwrap();
    let start = byte_offset_to_lsp_position(main_source, start_byte);
    let end = byte_offset_to_lsp_position(main_source, start_byte + "writeText".len());
    let range = Range { start, end };

    assert!(code_actions_for_unresolved(&main_state, &index, &main_uri, range, &[]).is_empty());

    let diagnostic = Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        message: "undefined function `writeText`".to_string(),
        ..Diagnostic::default()
    };
    let actions = code_actions_for_unresolved(
        &main_state,
        &index,
        &main_uri,
        range,
        &[diagnostic],
    );
    assert_eq!(actions.len(), 1);
    let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
        panic!("expected direct CodeAction");
    };
    assert!(action.command.is_none());
    assert!(action.edit.is_some());
}

#[test]
fn semantic_token_legend_keeps_existing_indices_and_appends_shell_types() {
    assert_eq!(TOKEN_TYPES[0], SemanticTokenType::VARIABLE);
    assert_eq!(TOKEN_TYPES[1], SemanticTokenType::FUNCTION);
    assert_eq!(TOKEN_TYPES[11], SemanticTokenType::new("functionGroup"));
    assert_eq!(TOKEN_TYPES[12], SemanticTokenType::new("shellCommand"));
    assert_eq!(TOKEN_TYPES[13], SemanticTokenType::new("shellBuiltin"));
    assert_eq!(TOKEN_TYPES[14], SemanticTokenType::new("shellArgument"));
    assert_eq!(TOKEN_TYPES[15], SemanticTokenType::new("shellFlag"));
    assert_eq!(TOKEN_TYPES[19], SemanticTokenType::new("shellInterpolation"));
}

#[test]
fn command_resolution_is_non_executing_and_path_aware() {
    let temp = tempfile::tempdir().unwrap();
    let tool = temp.path().join("fake-tool");
    std::fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&tool).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&tool, permissions).unwrap();
    }
    let path = std::env::join_paths([temp.path()]).unwrap();
    let resolver = CommandResolver::with_path(Some(path.as_os_str()));
    assert_eq!(resolver.resolve("cd"), CommandResolution::Builtin);
    assert_eq!(resolver.resolve("fake-tool"), CommandResolution::Resolved);
    assert_eq!(resolver.resolve("missing-tool"), CommandResolution::Unresolved);
}

#[test]
fn shell_semantic_tokens_classify_command_flags_and_interpolation() {
    let source = "function demo(input: str) -> shell { return shell { echo -n \"${input}\"; }; };\n";
    let program = raw_program_for_source(source).expect("program");
    let mut raw = Vec::new();
    collect_tokens_from_program(&program, source, &mut raw);
    assert!(raw.iter().any(|token| token.token_type == TT_SHELL_BUILTIN));
    assert!(raw.iter().any(|token| token.token_type == TT_SHELL_FLAG));
    assert!(raw.iter().any(|token| token.token_type == TT_SHELL_INTERPOLATION));
}

#[test]
fn signature_help_resolves_cross_file_function_group_member() {
    let temp = tempfile::tempdir().unwrap();
    let shared = temp.path().join("shared.spar");
    let main = temp.path().join("main.spar");
    std::fs::write(
        &shared,
        "functionGroup Tools { function run(input: str, mode: str = \"fast\") -> int { return 0; } };\n",
    )
    .unwrap();
    let source = "import \"shared.spar\" as shared;\nvar result: int = shared::Tools::run(input: \"x\", mode: \"fast\");\n";
    std::fs::write(&main, source).unwrap();
    let state = SparLanguageServer::analyze_path(source, &main);
    let index = WorkspaceIndex::default();
    let call = "shared::Tools::run(input: \"x\", ";
    let uri = Url::from_file_path(&main).unwrap();
    let help = signature_help_at(&state, &index, &uri, call, call.len()).expect("signature help");
    assert!(help.signatures[0].label.contains("run(input: str, mode: str ="));
    assert_eq!(help.active_parameter, Some(1));
}

#[test]
fn foreign_shell_does_not_emit_native_shell_semantic_classes() {
    let source = r#"function main() -> shell {
    return shell bash {
        printf "%s" "$HOME"
    };
};
"#;
    let program = raw_program_for_source(source).expect("program");
    let mut raw = Vec::new();
    collect_tokens_from_program(&program, source, &mut raw);
    assert!(
        !raw.iter().any(|token| (TT_SHELL_COMMAND..=TT_SHELL_INTERPOLATION).contains(&token.token_type)),
        "foreign-shell contents must be left to the editor's embedded grammar"
    );
}

#[test]
fn advertised_capabilities_cover_portable_intelligence_core() {
    let capabilities = advertised_server_capabilities();
    assert!(capabilities.completion_provider.is_some());
    assert!(capabilities.hover_provider.is_some());
    assert!(capabilities.definition_provider.is_some());
    assert!(capabilities.references_provider.is_some());
    assert!(capabilities.document_formatting_provider.is_some());
    assert!(capabilities.semantic_tokens_provider.is_some());
    assert!(capabilities.signature_help_provider.is_some());
    assert!(capabilities.document_highlight_provider.is_some());
    assert!(capabilities.document_symbol_provider.is_some());
    assert!(capabilities.workspace_symbol_provider.is_some());
    assert!(capabilities.rename_provider.is_some());
    assert!(capabilities.code_action_provider.is_some());
    assert_eq!(
        capabilities
            .completion_provider
            .as_ref()
            .and_then(|options| options.resolve_provider),
        Some(true)
    );
}

#[test]
fn minimal_client_completion_is_plain_and_rich_client_gets_required_arg_snippet() {
    let source = "function deploy(input: str, output: str, mode: str = \"debug\") -> int { return 0; };\n";
    let state = SparLanguageServer::analyze(source, std::path::Path::new("/workspace"));
    let symbols = state.effective_symbols().expect("symbols");

    let plain = function_completion_items(&symbols.functions, false);
    let plain_deploy = plain.iter().find(|item| item.label == "deploy").expect("deploy");
    assert_eq!(plain_deploy.insert_text.as_deref(), Some("deploy"));
    assert_eq!(plain_deploy.insert_text_format, Some(InsertTextFormat::PLAIN_TEXT));

    let snippets = function_completion_items(&symbols.functions, true);
    let snippet = snippets.iter().find(|item| item.label == "deploy").expect("deploy");
    assert_eq!(snippet.insert_text_format, Some(InsertTextFormat::SNIPPET));
    let inserted = snippet.insert_text.as_deref().expect("snippet text");
    assert!(inserted.contains("input: ${1}"));
    assert!(inserted.contains("output: ${2}"));
    assert!(!inserted.contains("mode:"), "defaulted args stay opt-in through Ctrl+Space");
}

#[test]
fn lsp_positions_use_utf16_not_utf8_bytes() {
    let source = "var emoji: str = \"😀\"; value";
    let value_byte = source.find("value").unwrap();
    let position = byte_offset_to_lsp_position(source, value_byte);
    assert_eq!(lsp_pos_to_byte_offset(source, position), value_byte);
    assert_eq!(word_at_position(source, position), "value");
}

#[test]
fn portable_payloads_have_no_editor_specific_commands_or_uris() {
    let uri = Url::parse("file:///workspace/main.spar").unwrap();
    let candidate = AutoImportCandidate {
        symbol_id: SymbolId("candidate".into()),
        import_path: "std/fs".into(),
        package: true,
        type_only: false,
        name: "readText".into(),
    };
    let edit = build_import_workspace_edit("var x: int = 1;\n", &uri, &candidate);
    let action = CodeActionOrCommand::CodeAction(CodeAction {
        title: "Import `readText` from `std/fs`".into(),
        kind: Some(CodeActionKind::QUICKFIX),
        diagnostics: None,
        edit: Some(edit),
        command: None,
        is_preferred: Some(true),
        disabled: None,
        data: None,
    });
    let serialized = serde_json::to_string(&action).unwrap();
    assert!(!serialized.to_ascii_lowercase().contains("vscode"));
    assert!(!serialized.contains("command"), "core auto-import uses direct WorkspaceEdit");
    assert!(serialized.contains("file:///workspace/main.spar"));
}

#[test]
fn completion_resolve_respects_negotiated_properties() {
    let uri = Url::parse("file:///workspace/main.spar").unwrap();
    let source = "function build(input: str) -> int { return 0; };\n";
    let state = SparLanguageServer::analyze(source, std::path::Path::new("/workspace"));
    let mut index = WorkspaceIndex::default();
    index.replace_document(&uri, &state);
    let symbol = index.find_by_name("build").into_iter().next().unwrap();
    let item = CompletionItem {
        label: "build".into(),
        data: Some(serde_json::json!({ "symbolId": symbol.id.0 })),
        ..Default::default()
    };
    let minimal = enrich_completion_from_index(item.clone(), &index, false, false);
    assert!(minimal.detail.is_none());
    assert!(minimal.documentation.is_none());
    let rich = enrich_completion_from_index(item, &index, true, true);
    assert!(rich.detail.is_some());
    assert!(rich.documentation.is_some());
}

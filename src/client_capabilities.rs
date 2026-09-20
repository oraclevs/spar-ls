// ── Portable LSP client capability normalization ─────────────────────────────
//
// Keep editor-brand checks out of the server. Optional behavior is derived only
// from standard InitializeParams.capabilities fields. JSON-pointer inspection is
// intentionally used here so spar-ls stays tolerant of lsp-types minor-version
// shape changes while still consuming only standard LSP capability names.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ClientFeatureSupport {
    completion_resolve_documentation: bool,
    completion_resolve_detail: bool,
    completion_snippets: bool,
    hierarchical_document_symbols: bool,
    // Normalized for protocol tests/future lazy enrichment. Core responses in
    // this milestone remain complete even when clients advertise resolve.
    #[allow(dead_code)]
    workspace_symbol_resolve: bool,
    #[allow(dead_code)]
    code_action_resolve: bool,
    watched_files_dynamic_registration: bool,
}

impl ClientFeatureSupport {
    fn from_initialize(params: &InitializeParams) -> Self {
        let value = serde_json::to_value(&params.capabilities).unwrap_or_default();

        let resolve_properties = value
            .pointer("/textDocument/completion/completionItem/resolveSupport/properties")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        let has_resolve_property = |name: &str| {
            resolve_properties
                .iter()
                .any(|value| value.as_str() == Some(name))
        };

        Self {
            completion_resolve_documentation: has_resolve_property("documentation"),
            completion_resolve_detail: has_resolve_property("detail"),
            completion_snippets: value
                .pointer("/textDocument/completion/completionItem/snippetSupport")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
            hierarchical_document_symbols: value
                .pointer("/textDocument/documentSymbol/hierarchicalDocumentSymbolSupport")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
            workspace_symbol_resolve: value
                .pointer("/workspace/symbol/resolveSupport/properties")
                .and_then(|value| value.as_array())
                .is_some_and(|properties| !properties.is_empty()),
            code_action_resolve: value
                .pointer("/textDocument/codeAction/resolveSupport/properties")
                .and_then(|value| value.as_array())
                .is_some_and(|properties| !properties.is_empty()),
            watched_files_dynamic_registration: value
                .pointer("/workspace/didChangeWatchedFiles/dynamicRegistration")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
        }
    }
}

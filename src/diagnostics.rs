use spar::{Span, SparError};
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

pub fn error_span(error: &SparError) -> &Span {
    match error {
        SparError::LexError { span, .. }
        | SparError::ParseError { span, .. }
        | SparError::ResolveError { span, .. }
        | SparError::TypeError { span, .. }
        | SparError::EvalError { span, .. }
        | SparError::SchemaError { span, .. } => span,
    }
}

pub fn error_message(error: &SparError) -> &str {
    match error {
        SparError::LexError { message, .. }
        | SparError::ParseError { message, .. }
        | SparError::ResolveError { message, .. }
        | SparError::TypeError { message, .. }
        | SparError::EvalError { message, .. }
        | SparError::SchemaError { message, .. } => message,
    }
}

pub fn error_hint(error: &SparError) -> Option<&str> {
    match error {
        SparError::ResolveError { hint, .. } | SparError::TypeError { hint, .. } => hint.as_deref(),
        _ => None,
    }
}

pub fn spar_error_to_diagnostic(error: &SparError) -> Diagnostic {
    let span = error_span(error);
    let message = match error_hint(error) {
        Some(hint) => format!("{}\nHint: {}", error_message(error), hint),
        None => error_message(error).to_string(),
    };
    let start = Position {
        line: span.line.saturating_sub(1),
        character: span.col.saturating_sub(1),
    };
    let end = Position {
        line: start.line,
        character: start.character + (span.end.saturating_sub(span.start) as u32).max(1),
    };
    Diagnostic {
        range: Range { start, end },
        severity: Some(if matches!(error, SparError::EvalError { .. }) {
            DiagnosticSeverity::WARNING
        } else {
            DiagnosticSeverity::ERROR
        }),
        message,
        source: Some("spar".into()),
        ..Default::default()
    }
}

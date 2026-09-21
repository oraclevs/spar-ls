//! Completion and hover for `#[attribute]` syntax. Text-based, like task and
//! decoder positions, so it works even when the file does not parse yet.

use tower_lsp::lsp_types::{CompletionItem, CompletionItemKind};

fn line_bounds(source: &str, offset: usize) -> Option<(usize, usize)> {
    let offset = offset.min(source.len());
    if !source.is_char_boundary(offset) {
        return None;
    }
    let start = source[..offset].rfind('\n').map_or(0, |index| index + 1);
    let end = source[offset..]
        .find('\n')
        .map_or(source.len(), |index| offset + index);
    Some((start, end))
}

/// Attribute-name completions when the cursor is inside an unclosed `#[`.
pub(crate) fn attribute_completion_items(source: &str, offset: usize) -> Option<Vec<CompletionItem>> {
    let (start, _) = line_bounds(source, offset)?;
    let prefix = &source[start..offset.min(source.len())];
    let open = prefix.rfind("#[")?;
    if prefix[open + 2..].contains(']') {
        return None;
    }
    Some(
        spar::ast::KNOWN_ATTRIBUTES
            .iter()
            .map(|name| CompletionItem {
                label: (*name).to_string(),
                kind: Some(CompletionItemKind::PROPERTY),
                detail: Some("Write this declaration in `spar emit` output".to_string()),
                ..Default::default()
            })
            .collect(),
    )
}

/// Hover text when the cursor is on the name inside `#[name]`.
pub(crate) fn attribute_hover_at(source: &str, offset: usize) -> Option<String> {
    let (start, end) = line_bounds(source, offset)?;
    let line = &source[start..end];
    let column = offset.min(source.len()) - start;
    let open = line[..column.min(line.len())].rfind("#[").or_else(|| {
        // Cursor exactly on the `#` or `[`.
        line.get(column.saturating_sub(1)..).and_then(|_| line.find("#["))
    })?;
    let name_start = open + 2;
    let close = line[name_start..].find(']')? + name_start;
    if column < open || column > close {
        return None;
    }
    let name = line[name_start..close].trim();
    match name {
        "emit" => Some(
            "**`#[emit]`**\n\nMarks a top-level `struct` or `var` for output. `spar emit` writes only declarations that carry this attribute; `export` and `private` do not affect emission."
                .to_string(),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_attribute_names_after_hash_bracket() {
        let source = "#[";
        let items = attribute_completion_items(source, source.len()).expect("items");
        let labels: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, vec!["emit"]);
    }

    #[test]
    fn completes_a_partial_name() {
        let source = "var a: int = 1;\n#[em";
        assert!(attribute_completion_items(source, source.len()).is_some());
    }

    #[test]
    fn no_completion_once_the_attribute_is_closed_or_absent() {
        let closed = "#[emit] ";
        assert!(attribute_completion_items(closed, closed.len()).is_none());
        let plain = "var x: int = 1;";
        assert!(attribute_completion_items(plain, plain.len()).is_none());
    }

    #[test]
    fn hover_explains_emit() {
        let source = "#[emit]\nstruct A { x: int = 1; };\n";
        let offset = source.find("emit").unwrap() + 1;
        let text = attribute_hover_at(source, offset).expect("hover");
        assert!(text.contains("#[emit]"), "{text}");
        assert!(text.contains("`spar emit`"), "{text}");
    }

    #[test]
    fn no_hover_outside_an_attribute() {
        let source = "#[emit]\nstruct A { x: int = 1; };\n";
        let offset = source.find("struct").unwrap() + 1;
        assert!(attribute_hover_at(source, offset).is_none());
    }
}

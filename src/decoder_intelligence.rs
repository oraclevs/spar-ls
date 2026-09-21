// ── Structured-input decoder intelligence ──────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecoderCompletionContext<'a> {
    Name {
        namespace: Option<spar::DecoderNamespace>,
        prefix: &'a str,
    },
    Options {
        namespace: Option<spar::DecoderNamespace>,
        name: &'a str,
        args: &'a str,
    },
}

fn decoder_ident_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-')
}

fn decoder_stage_prefix(source: &str, offset: usize) -> Option<&str> {
    let prefix = source.get(..offset.min(source.len()))?;
    let mut search_end = prefix.len();
    while let Some(relative) = prefix[..search_end].rfind("from") {
        let before = prefix[..relative].chars().next_back();
        let after = prefix[relative + 4..].chars().next();
        let word_boundary = before.is_none_or(|ch| !decoder_ident_char(ch))
            && after.is_none_or(|ch| !decoder_ident_char(ch));
        if word_boundary {
            let leading = prefix[..relative].trim_end();
            if leading.ends_with('|') {
                return Some(prefix[relative + 4..].trim_start());
            }
        }
        if relative == 0 {
            break;
        }
        search_end = relative;
    }
    None
}

fn split_decoder_ref(text: &str) -> (Option<spar::DecoderNamespace>, &str) {
    let text = text.trim();
    let Some((namespace, name)) = text.split_once("::") else {
        return (None, text);
    };
    let namespace = match namespace.trim() {
        "codec" => Some(spar::DecoderNamespace::Codec),
        "scoc" => Some(spar::DecoderNamespace::Scoc),
        "custom" => Some(spar::DecoderNamespace::Custom),
        _ => None,
    };
    (namespace, name.trim())
}

fn decoder_completion_context(source: &str, offset: usize) -> Option<DecoderCompletionContext<'_>> {
    let stage = decoder_stage_prefix(source, offset)?;

    // Stop offering decoder completion after the stage has clearly moved on.
    // The cursor prefix is intentionally tolerant of an unfinished option list.
    let mut paren_depth = 0i32;
    let mut quote = None::<char>;
    let mut escaped = false;
    for (index, ch) in stage.char_indices() {
        if let Some(active) = quote {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
            } else if ch == active {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '(' => paren_depth += 1,
            ')' => paren_depth = (paren_depth - 1).max(0),
            '|' if paren_depth == 0 => {
                // `|>` and another native pipe both end the decoder stage.
                if index < stage.len() {
                    return None;
                }
            }
            ';' if paren_depth == 0 => return None,
            _ => {}
        }
    }

    if let Some(open) = stage.find('(') {
        let decoder_ref = stage[..open].trim();
        let (namespace, name) = split_decoder_ref(decoder_ref);
        if name.is_empty() {
            return None;
        }
        return Some(DecoderCompletionContext::Options {
            namespace,
            name,
            args: &stage[open + 1..],
        });
    }

    let trimmed = stage.trim();
    if let Some((namespace_text, prefix)) = trimmed.split_once("::") {
        let namespace = match namespace_text.trim() {
            "codec" => Some(spar::DecoderNamespace::Codec),
            "scoc" => Some(spar::DecoderNamespace::Scoc),
            "custom" => Some(spar::DecoderNamespace::Custom),
            _ => return None,
        };
        return Some(DecoderCompletionContext::Name {
            namespace,
            prefix: prefix.trim(),
        });
    }

    Some(DecoderCompletionContext::Name {
        namespace: None,
        prefix: trimmed,
    })
}

fn decoder_kind_label(kind: spar::DecoderKind) -> &'static str {
    match kind {
        spar::DecoderKind::Codec => "Spar codec",
        spar::DecoderKind::Scoc => "SCOC parser",
        spar::DecoderKind::Custom => "custom parser",
    }
}

fn decoder_name_completion_item(descriptor: &spar::DecoderDescriptor) -> CompletionItem {
    decoder_alias_completion_item(descriptor, &descriptor.name, descriptor.compatibility_alias_for.as_deref())
}

fn decoder_alias_completion_item(
    descriptor: &spar::DecoderDescriptor,
    label: &str,
    alias_for: Option<&str>,
) -> CompletionItem {
    let alias_detail = alias_for
        .map(|canonical| format!(" · alias for {canonical}"))
        .unwrap_or_default();
    CompletionItem {
        label: label.to_string(),
        kind: Some(CompletionItemKind::VALUE),
        detail: Some(format!(
            "{} · {}{}",
            decoder_kind_label(descriptor.kind),
            descriptor.normalized_output.as_str(),
            alias_detail
        )),
        documentation: Some(Documentation::String(descriptor.description.clone())),
        ..Default::default()
    }
}

fn used_decoder_option_names(args: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut start = 0usize;
    let mut paren = 0i32;
    let mut bracket = 0i32;
    let mut brace = 0i32;
    let mut quote = None::<char>;
    let mut escaped = false;
    let mut pieces = Vec::new();
    for (index, ch) in args.char_indices() {
        if let Some(active) = quote {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
            } else if ch == active {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '(' => paren += 1,
            ')' => paren = (paren - 1).max(0),
            '[' => bracket += 1,
            ']' => bracket = (bracket - 1).max(0),
            '{' => brace += 1,
            '}' => brace = (brace - 1).max(0),
            ',' if paren == 0 && bracket == 0 && brace == 0 => {
                pieces.push(&args[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    pieces.push(&args[start..]);
    for piece in pieces {
        if let Some((name, _)) = piece.split_once(':') {
            let name = name.trim();
            if !name.is_empty() {
                out.insert(name.to_string());
            }
        }
    }
    out
}

fn decoder_descriptor_for_context(
    namespace: Option<spar::DecoderNamespace>,
    name: &str,
) -> Option<spar::DecoderDescriptor> {
    let descriptors = spar::structured_decoder_descriptors();
    let wanted_kind = namespace.map(|namespace| match namespace {
        spar::DecoderNamespace::Codec => spar::DecoderKind::Codec,
        spar::DecoderNamespace::Scoc => spar::DecoderKind::Scoc,
        spar::DecoderNamespace::Custom => spar::DecoderKind::Custom,
    });

    if let Some(kind) = wanted_kind {
        return descriptors.into_iter().find_map(|descriptor| {
            if descriptor.kind != kind {
                return None;
            }
            if descriptor.name == name || descriptor.aliases.iter().any(|alias| alias == name) {
                return Some(descriptor);
            }
            None
        });
    }

    // Preserve runtime precedence for unqualified names: codec -> SCOC -> custom.
    for kind in [spar::DecoderKind::Codec, spar::DecoderKind::Scoc, spar::DecoderKind::Custom] {
        if let Some(descriptor) = descriptors.iter().find(|descriptor| {
            descriptor.kind == kind
                && (descriptor.name == name || descriptor.aliases.iter().any(|alias| alias == name))
        }) {
            return Some(descriptor.clone());
        }
    }
    None
}

fn decoder_option_completion_item(name: &str, kind: &str, description: &str) -> CompletionItem {
    CompletionItem {
        label: name.to_string(),
        kind: Some(CompletionItemKind::FIELD),
        detail: Some(kind.to_string()),
        documentation: Some(Documentation::String(description.to_string())),
        insert_text: Some(format!("{name}: ")),
        ..Default::default()
    }
}

fn decoder_completion_items(source: &str, offset: usize) -> Option<Vec<CompletionItem>> {
    match decoder_completion_context(source, offset)? {
        DecoderCompletionContext::Name { namespace, prefix } => {
            let wanted_kind = namespace.map(|namespace| match namespace {
                spar::DecoderNamespace::Codec => spar::DecoderKind::Codec,
                spar::DecoderNamespace::Scoc => spar::DecoderKind::Scoc,
                spar::DecoderNamespace::Custom => spar::DecoderKind::Custom,
            });
            let descriptors = spar::structured_decoder_descriptors();
            let mut items = Vec::new();
            for descriptor in descriptors
                .into_iter()
                .filter(|descriptor| wanted_kind.is_none_or(|kind| descriptor.kind == kind))
                .filter(|descriptor| {
                    // JC streaming compatibility aliases are explicit SCOC names.
                    // Keep the unqualified list canonical and uncluttered.
                    namespace == Some(spar::DecoderNamespace::Scoc)
                        || descriptor.compatibility_alias_for.is_none()
                })
            {
                if descriptor.name.starts_with(prefix) {
                    items.push(decoder_name_completion_item(&descriptor));
                }
                // Normal registry aliases are useful after an explicit namespace
                // (`codec::yml`, `scoc::printenv`) but stay out of the unqualified
                // completion list so canonical names remain the default.
                if namespace.is_some() && descriptor.compatibility_alias_for.is_none() {
                    for alias in &descriptor.aliases {
                        if alias.starts_with(prefix) {
                            items.push(decoder_alias_completion_item(
                                &descriptor,
                                alias,
                                Some(&descriptor.name),
                            ));
                        }
                    }
                }
            }
            items.sort_by(|left, right| left.label.cmp(&right.label));
            items.dedup_by(|left, right| left.label == right.label);
            Some(items)
        }
        DecoderCompletionContext::Options { namespace, name, args } => {
            let descriptor = decoder_descriptor_for_context(namespace, name)?;
            let used = used_decoder_option_names(args);
            let mut items = descriptor
                .options
                .iter()
                .filter(|option| !used.contains(&option.name))
                .map(|option| {
                    let kind = match option.kind {
                        spar::DecoderOptionKind::Bool => "bool",
                        spar::DecoderOptionKind::Integer => "int",
                        spar::DecoderOptionKind::Float => "float",
                        spar::DecoderOptionKind::String => "str",
                        spar::DecoderOptionKind::Enum => "enum",
                    };
                    decoder_option_completion_item(&option.name, kind, &option.description)
                })
                .collect::<Vec<_>>();
            if descriptor.kind == spar::DecoderKind::Scoc && !used.contains("streaming") {
                items.push(decoder_option_completion_item(
                    "streaming",
                    "bool",
                    "Override automatic SCOC streaming selection",
                ));
            }
            items.sort_by(|left, right| left.label.cmp(&right.label));
            Some(items)
        }
    }
}

fn decoder_word_range(source: &str, offset: usize) -> Option<(usize, usize)> {
    if source.is_empty() {
        return None;
    }
    let mut start = offset.min(source.len());
    if start == source.len() && start > 0 {
        start -= 1;
    }
    while start > 0 {
        let ch = source[..start].chars().next_back()?;
        if decoder_ident_char(ch) {
            start -= ch.len_utf8();
        } else {
            break;
        }
    }
    let mut end = offset.min(source.len());
    while end < source.len() {
        let ch = source[end..].chars().next()?;
        if decoder_ident_char(ch) {
            end += ch.len_utf8();
        } else {
            break;
        }
    }
    (start < end).then_some((start, end))
}

fn decoder_context_before_name(source: &str, name_start: usize) -> Option<Option<spar::DecoderNamespace>> {
    let prefix = source.get(..name_start)?.trim_end();
    if let Some(base) = prefix.strip_suffix("::") {
        let namespace_start = base
            .char_indices()
            .rev()
            .take_while(|(_, ch)| decoder_ident_char(*ch))
            .map(|(index, _)| index)
            .last()
            .unwrap_or(base.len());
        let namespace = base[namespace_start..].trim();
        let namespace = match namespace {
            "codec" => spar::DecoderNamespace::Codec,
            "scoc" => spar::DecoderNamespace::Scoc,
            "custom" => spar::DecoderNamespace::Custom,
            _ => return None,
        };
        let before_namespace = base[..namespace_start].trim_end();
        return before_namespace.ends_with("from").then_some(Some(namespace));
    }
    prefix.ends_with("from").then_some(None)
}

fn decoder_hover_at(source: &str, offset: usize) -> Option<String> {
    let (start, end) = decoder_word_range(source, offset)?;
    let name = source.get(start..end)?;
    let namespace = decoder_context_before_name(source, start)?;
    let descriptor = decoder_descriptor_for_context(namespace, name)?;

    let mut lines = vec![format!("**{}**", descriptor.name), String::new()];
    match descriptor.kind {
        spar::DecoderKind::Scoc => lines.push("SCOC command-output parser".to_string()),
        spar::DecoderKind::Codec => lines.push("Native Spar structured decoder".to_string()),
        spar::DecoderKind::Custom => lines.push("Custom Spar decoder".to_string()),
    }
    lines.push(format!("Output: {}", descriptor.normalized_output.as_str()));
    if !descriptor.platforms.is_empty() {
        let platforms = descriptor
            .platforms
            .iter()
            .map(|platform| match platform.as_str() {
                "linux" => "Linux",
                "macos" => "macOS",
                "windows" => "Windows",
                other => other,
            })
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("Platforms: {platforms}"));
    }
    if let Some(baseline) = &descriptor.jc_baseline {
        lines.push(format!("JC baseline: {baseline}"));
    }
    lines.push(format!(
        "Raw mode: {}",
        if descriptor.capabilities.raw { "yes" } else { "no" }
    ));
    lines.push(format!(
        "Streaming: {}",
        if descriptor.capabilities.streaming { "yes (automatic for live input)" } else { "no" }
    ));
    if let Some(canonical) = &descriptor.compatibility_alias_for {
        lines.push(format!("JC compatibility alias for `{canonical}`; prefer `{canonical}`."));
    }
    if !descriptor.description.is_empty() {
        lines.push(String::new());
        lines.push(descriptor.description.clone());
    }
    Some(lines.join("\n"))
}

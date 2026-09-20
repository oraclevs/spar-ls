// ── Local-scope completion (token based, tolerant of broken code) ────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeNameKind {
    Parameter,
    Variable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScopeName {
    name: String,
    kind: ScopeNameKind,
    ty: Option<String>,
}

fn scope_type_text(token: &spar::token::Token) -> Option<String> {
    use spar::token::Token;
    match token {
        Token::TypeStr => Some("str".into()),
        Token::TypeInt => Some("int".into()),
        Token::TypeFloat => Some("float".into()),
        Token::TypeBool => Some("bool".into()),
        Token::TypeShell => Some("shell".into()),
        Token::Ident(name) => Some(name.clone()),
        _ => None,
    }
}

/// Names visible at `offset`: parameters of the enclosing function, `var`
/// declarations, `for` bindings and `catch` names in still-open blocks.
/// Works on the text before the cursor, so it survives syntax errors elsewhere.
fn local_names_at(source: &str, offset: usize) -> Vec<ScopeName> {
    use spar::token::Token;
    let mut end = offset.min(source.len());
    while !source.is_char_boundary(end) {
        end -= 1;
    }
    let Ok(lexed) = Lexer::new(&source[..end]).tokenize() else {
        return Vec::new();
    };
    let tokens: Vec<&Token> = lexed.iter().map(|spanned| &spanned.token).collect();

    let mut stack: Vec<Vec<ScopeName>> = Vec::new();
    let mut pending: Vec<ScopeName> = Vec::new();
    let mut index = 0usize;
    while index < tokens.len() {
        match tokens[index] {
            Token::KwFunction => {
                // function name<T>(params) -> ret {
                let mut cursor = index + 1;
                while cursor < tokens.len()
                    && *tokens[cursor] != Token::LParen
                    && *tokens[cursor] != Token::Eof
                {
                    cursor += 1;
                }
                if cursor < tokens.len() && *tokens[cursor] == Token::LParen {
                    let mut depth = 0i32;
                    let mut params = Vec::new();
                    let mut position = cursor;
                    while position < tokens.len() {
                        match tokens[position] {
                            Token::LParen => depth += 1,
                            Token::RParen => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            Token::Ident(name)
                                if depth == 1 && tokens.get(position + 1).copied() == Some(&Token::Colon) =>
                            {
                                let ty = tokens.get(position + 2).and_then(|token| scope_type_text(token));
                                params.push(ScopeName { name: name.clone(), kind: ScopeNameKind::Parameter, ty });
                            }
                            _ => {}
                        }
                        position += 1;
                    }
                    pending = params;
                    index = position;
                }
            }
            Token::Var => {
                let mut cursor = index + 1;
                if tokens.get(cursor).copied() == Some(&Token::KwMut) {
                    cursor += 1;
                }
                if let Some(Token::Ident(name)) = tokens.get(cursor).copied() {
                    let ty = if tokens.get(cursor + 1).copied() == Some(&Token::Colon) {
                        tokens.get(cursor + 2).and_then(|token| scope_type_text(token))
                    } else {
                        None
                    };
                    if let Some(scope) = stack.last_mut() {
                        scope.push(ScopeName { name: name.clone(), kind: ScopeNameKind::Variable, ty });
                    }
                }
            }
            Token::KwFor => {
                let mut cursor = index + 1;
                while cursor < tokens.len()
                    && *tokens[cursor] != Token::KwIn
                    && *tokens[cursor] != Token::LBrace
                    && *tokens[cursor] != Token::Eof
                {
                    if let Token::Ident(name) = tokens[cursor] {
                        pending.push(ScopeName { name: name.clone(), kind: ScopeNameKind::Variable, ty: None });
                    }
                    cursor += 1;
                }
            }
            Token::KwCatch => {
                if let Some(Token::Ident(name)) = tokens.get(index + 1).copied() {
                    pending.push(ScopeName {
                        name: name.clone(),
                        kind: ScopeNameKind::Variable,
                        ty: Some("error".into()),
                    });
                }
            }
            Token::LBrace => stack.push(std::mem::take(&mut pending)),
            Token::RBrace => {
                stack.pop();
            }
            _ => {}
        }
        index += 1;
    }
    stack.into_iter().flatten().collect()
}

fn with_tier(mut item: CompletionItem, tier: u8) -> CompletionItem {
    item.sort_text = Some(format!("{tier}_{}", item.label));
    item
}

fn scope_completion_items(names: &[ScopeName]) -> Vec<CompletionItem> {
    let mut seen = HashSet::new();
    names
        .iter()
        .rev() // innermost declaration wins on shadowing
        .filter(|scope| seen.insert(scope.name.clone()))
        .map(|scope| {
            with_tier(
                CompletionItem {
                    label: scope.name.clone(),
                    kind: Some(CompletionItemKind::VARIABLE),
                    detail: scope.ty.clone(),
                    label_details: Some(CompletionItemLabelDetails {
                        detail: None,
                        description: Some(match scope.kind {
                            ScopeNameKind::Parameter => "parameter".to_string(),
                            ScopeNameKind::Variable => "local".to_string(),
                        }),
                    }),
                    ..Default::default()
                },
                0,
            )
        })
        .collect()
}

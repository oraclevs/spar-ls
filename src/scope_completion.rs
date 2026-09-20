// ── Local-scope completion (token based, tolerant of broken code) ────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeNameKind {
    Parameter,
    Variable,
}

#[derive(Debug, Clone, PartialEq)]
enum ChainStep {
    Field(String),
    Index,
}

/// A receiver expression made only of a name, `.field` accesses and `[index]` steps.
#[derive(Debug, Clone, PartialEq)]
struct Chain {
    root: String,
    steps: Vec<ChainStep>,
}

#[derive(Debug, Clone, PartialEq)]
struct ScopeName {
    name: String,
    kind: ScopeNameKind,
    /// Display text of the declared type, when annotated.
    ty: Option<String>,
    /// The declared type, parsed.
    declared: Option<SparType>,
    /// `var x = <chain>;` with no annotation: the type is that expression's type.
    init: Option<Chain>,
    /// `for x in <chain>`: the type is the element type of that expression.
    element_of: Option<Chain>,
}

impl ScopeName {
    fn new(name: &str, kind: ScopeNameKind, ty: Option<String>) -> Self {
        Self { name: name.to_string(), kind, ty, declared: None, init: None, element_of: None }
    }
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

/// Parses a type from tokens: `str`, `Name`, `Name<A, B>`, `[T]`.
fn parse_type_tokens(tokens: &[&spar::token::Token], index: &mut usize) -> Option<SparType> {
    use spar::token::Token;
    let token = tokens.get(*index).copied()?;
    let ty = match token {
        Token::LBracket => {
            *index += 1;
            let inner = parse_type_tokens(tokens, index)?;
            if tokens.get(*index).copied() != Some(&Token::RBracket) {
                return None;
            }
            *index += 1;
            return Some(SparType::List(Box::new(inner)));
        }
        Token::TypeStr => SparType::Str,
        Token::TypeInt => SparType::Int,
        Token::TypeFloat => SparType::Float,
        Token::TypeBool => SparType::Bool,
        Token::TypeShell => SparType::Shell,
        Token::TypeVoid => SparType::Void,
        Token::TypeSection => SparType::Section,
        Token::Ident(name) => {
            *index += 1;
            if tokens.get(*index).copied() == Some(&Token::Lt) {
                *index += 1;
                let mut arguments = Vec::new();
                loop {
                    arguments.push(parse_type_tokens(tokens, index)?);
                    match tokens.get(*index).copied() {
                        Some(Token::Comma) => *index += 1,
                        Some(Token::Gt) => {
                            *index += 1;
                            break;
                        }
                        _ => return None,
                    }
                }
                return Some(SparType::Applied { name: name.clone(), arguments });
            }
            return Some(SparType::Named(name.clone()));
        }
        _ => return None,
    };
    *index += 1;
    Some(ty)
}

/// Parses `name(.field | [ ... ])*` starting at `index`; only accepted when the
/// chain is the whole expression (followed by `;`, `{` or the end of input).
fn parse_chain_tokens(tokens: &[&spar::token::Token], mut index: usize) -> Option<Chain> {
    use spar::token::Token;
    let Token::Ident(root) = tokens.get(index).copied()? else {
        return None;
    };
    index += 1;
    let mut steps = Vec::new();
    loop {
        match tokens.get(index).copied() {
            Some(Token::Dot) => {
                let Some(Token::Ident(field)) = tokens.get(index + 1).copied() else {
                    return None;
                };
                steps.push(ChainStep::Field(field.clone()));
                index += 2;
            }
            Some(Token::LBracket) => {
                let mut depth = 0i32;
                loop {
                    match tokens.get(index).copied() {
                        Some(Token::LBracket) => depth += 1,
                        Some(Token::RBracket) => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        None | Some(Token::Eof) => return None,
                        _ => {}
                    }
                    index += 1;
                }
                steps.push(ChainStep::Index);
                index += 1;
            }
            Some(Token::Semicolon) | Some(Token::LBrace) | Some(Token::Eof) | None => break,
            _ => return None,
        }
    }
    Some(Chain { root: root.clone(), steps })
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
                                let mut type_at = position + 2;
                                let declared = parse_type_tokens(&tokens, &mut type_at);
                                let mut param = ScopeName::new(name, ScopeNameKind::Parameter, ty);
                                param.declared = declared;
                                params.push(param);
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
                    let annotated = tokens.get(cursor + 1).copied() == Some(&Token::Colon);
                    let ty = if annotated {
                        tokens.get(cursor + 2).and_then(|token| scope_type_text(token))
                    } else {
                        None
                    };
                    let mut binding = ScopeName::new(name, ScopeNameKind::Variable, ty);
                    if annotated {
                        let mut type_at = cursor + 2;
                        binding.declared = parse_type_tokens(&tokens, &mut type_at);
                    } else if tokens.get(cursor + 1).copied() == Some(&Token::Eq) {
                        binding.init = parse_chain_tokens(&tokens, cursor + 2);
                    }
                    if let Some(scope) = stack.last_mut() {
                        scope.push(binding);
                    }
                }
            }
            Token::KwFor => {
                let mut cursor = index + 1;
                let mut bindings: Vec<ScopeName> = Vec::new();
                while cursor < tokens.len()
                    && *tokens[cursor] != Token::KwIn
                    && *tokens[cursor] != Token::LBrace
                    && *tokens[cursor] != Token::Eof
                {
                    if let Token::Ident(name) = tokens[cursor] {
                        bindings.push(ScopeName::new(name, ScopeNameKind::Variable, None));
                    }
                    cursor += 1;
                }
                if tokens.get(cursor).copied() == Some(&Token::KwIn) {
                    let iterable = parse_chain_tokens(&tokens, cursor + 1);
                    // `for x in xs` and `for (i, x) in xs`: the last name is the element.
                    if let Some(element) = bindings.last_mut() {
                        element.element_of = iterable;
                    }
                    if bindings.len() > 1 {
                        bindings[0].declared = Some(SparType::Int);
                        bindings[0].ty = Some("int".into());
                    }
                }
                pending.extend(bindings);
            }
            Token::KwCatch => {
                if let Some(Token::Ident(name)) = tokens.get(index + 1).copied() {
                    pending.push(ScopeName::new(name, ScopeNameKind::Variable, Some("error".into())));
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

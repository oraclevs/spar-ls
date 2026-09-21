//! Token-free, parse-free classification of the cursor position inside a
//! `task` declaration.
//!
//! While a task is being typed the file is usually invalid (an empty body has
//! no `run` block yet, a half-typed field name is not a field), so the parser
//! produces no AST and AST-driven completion has nothing to work with. This
//! module scans only the text before the cursor — skipping comments and
//! strings — and reports where the cursor is inside the innermost open task.

use spar::ast::RunShell;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskContext {
    /// Inside `task Name( ... )`.
    Params,
    /// At a field-name position in the task body.
    Body,
    /// After `field:` and before its terminating `;`.
    FieldValue(String),
    /// Inside `dependsOn: [ ... ]`.
    DependsOn,
    /// Between `run` and the body's `{`. The flags cover header words that
    /// are already complete (followed by whitespace).
    RunHeader { shell_seen: bool, os_seen: bool },
    /// Inside a `run` body.
    RunBody(RunShell),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskScope {
    pub name: String,
    pub context: TaskContext,
    /// OS slots already taken by completed `run` blocks (`None` = any OS).
    pub used_os_slots: Vec<Option<String>>,
    /// Metadata fields already present in the body.
    pub present_fields: Vec<String>,
    /// `(name, type)` pairs parsed from the parameter list.
    pub params: Vec<(String, String)>,
}

const OS_WORDS: [&str; 3] = ["linux", "macos", "windows"];

enum Outcome {
    /// The task closed before the end of the scanned text; resume scanning at
    /// this index. Carries what the task declared.
    Closed(usize, TaskScope),
    /// The cursor is inside this task.
    Open(TaskScope),
    /// Not a task declaration after all; resume at this index.
    NotATask(usize),
    /// The cursor is in a comment/string or otherwise has no completions.
    Nothing,
}

/// Classifies `offset` relative to the innermost open `task` declaration.
pub fn task_context(source: &str, offset: usize) -> Option<TaskScope> {
    let text = source.get(..offset)?;
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut depth = 0i32;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                i = skip_line_comment(bytes, i)?;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i = skip_block_comment(bytes, i)?;
            }
            b'"' => {
                i = skip_string(bytes, i)?;
            }
            b'{' | b'(' | b'[' => {
                depth += 1;
                i += 1;
            }
            b'}' | b')' | b']' => {
                depth -= 1;
                i += 1;
            }
            b if is_word(b) && depth <= 0 && word_boundary_before(bytes, i) => {
                let end = word_end(bytes, i);
                if &text[i..end] == "task" {
                    match scan_task(text, end) {
                        Outcome::Closed(next, _) | Outcome::NotATask(next) => i = next,
                        Outcome::Open(scope) => return Some(with_whole_task(scope, source, end)),
                        Outcome::Nothing => return None,
                    }
                } else {
                    i = end;
                }
            }
            b if is_word(b) => i = word_end(bytes, i),
            _ => i += 1,
        }
    }
    None
}

/// The prefix scan can't see fields or `run` blocks declared *after* the
/// cursor. Rescan the whole task so `present_fields` / `used_os_slots` cover
/// it too (the cursor context itself still comes from the prefix).
fn with_whole_task(mut scope: TaskScope, source: &str, after_kw: usize) -> TaskScope {
    let (Outcome::Closed(_, whole) | Outcome::Open(whole)) = scan_task(source, after_kw) else {
        return scope;
    };
    for field in whole.present_fields {
        if !scope.present_fields.contains(&field) {
            scope.present_fields.push(field);
        }
    }
    for slot in whole.used_os_slots {
        if !scope.used_os_slots.contains(&slot) {
            scope.used_os_slots.push(slot);
        }
    }
    scope
}

fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn word_end(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && is_word(bytes[i]) {
        i += 1;
    }
    i
}

fn word_boundary_before(bytes: &[u8], i: usize) -> bool {
    i == 0 || !is_word(bytes[i - 1])
}

fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// `Some(next)` past the newline, or `None` when the text ends inside the
/// comment (the cursor is in it).
fn skip_line_comment(bytes: &[u8], mut i: usize) -> Option<usize> {
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

fn skip_block_comment(bytes: &[u8], mut i: usize) -> Option<usize> {
    i += 2;
    while i + 1 < bytes.len() {
        if bytes[i] == b'*' && bytes[i + 1] == b'/' {
            return Some(i + 2);
        }
        i += 1;
    }
    None
}

fn skip_string(bytes: &[u8], mut i: usize) -> Option<usize> {
    i += 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Skips whitespace and comments. `None` when the text ends inside a comment.
fn skip_trivia(bytes: &[u8], mut i: usize) -> Option<usize> {
    loop {
        i = skip_ws(bytes, i);
        if bytes.get(i) == Some(&b'/') && bytes.get(i + 1) == Some(&b'/') {
            i = skip_line_comment(bytes, i)?;
        } else if bytes.get(i) == Some(&b'/') && bytes.get(i + 1) == Some(&b'*') {
            i = skip_block_comment(bytes, i)?;
        } else {
            return Some(i);
        }
    }
}

/// `after_kw` is the index just past the `task` keyword.
fn scan_task(text: &str, after_kw: usize) -> Outcome {
    let bytes = text.as_bytes();
    let Some(mut i) = skip_trivia(bytes, after_kw) else {
        return Outcome::Nothing;
    };
    if i == after_kw || i >= bytes.len() || !is_word(bytes[i]) {
        // `task` followed by punctuation (e.g. `task:`) or nothing: not a decl.
        return Outcome::NotATask(after_kw);
    }
    let name_end = word_end(bytes, i);
    let name = text[i..name_end].to_string();
    i = name_end;
    let Some(next) = skip_trivia(bytes, i) else {
        return Outcome::Nothing;
    };
    i = next;

    let mut scope = TaskScope {
        name,
        context: TaskContext::Body,
        used_os_slots: Vec::new(),
        present_fields: Vec::new(),
        params: Vec::new(),
    };

    if bytes.get(i) == Some(&b'(') {
        let params_start = i + 1;
        let mut depth = 1i32;
        i += 1;
        while i < bytes.len() {
            match bytes[i] {
                b'"' => match skip_string(bytes, i) {
                    Some(next) => {
                        i = next;
                        continue;
                    }
                    None => return Outcome::Nothing,
                },
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        scope.params = parse_params(&text[params_start..i.min(bytes.len())]);
        if depth != 0 {
            scope.context = TaskContext::Params;
            return Outcome::Open(scope);
        }
        i += 1; // ')'
        let Some(next) = skip_trivia(bytes, i) else {
            return Outcome::Nothing;
        };
        i = next;
    }

    if bytes.get(i) != Some(&b'{') {
        return if i >= bytes.len() {
            // Cursor between the name/params and the body brace.
            Outcome::Nothing
        } else {
            Outcome::NotATask(i)
        };
    }
    scan_body(text, i + 1, scope)
}

fn scan_body(text: &str, mut i: usize, mut scope: TaskScope) -> Outcome {
    let bytes = text.as_bytes();
    loop {
        let Some(next) = skip_trivia(bytes, i) else {
            return Outcome::Nothing;
        };
        i = next;
        if i >= bytes.len() {
            scope.context = TaskContext::Body;
            return Outcome::Open(scope);
        }
        match bytes[i] {
            b'}' => {
                i += 1;
                let Some(after) = skip_trivia(bytes, i) else {
                    return Outcome::Nothing;
                };
                return Outcome::Closed(
                    if bytes.get(after) == Some(&b';') {
                        after + 1
                    } else {
                        i
                    },
                    scope,
                );
            }
            b if is_word(b) => {
                let end = word_end(bytes, i);
                let word = &text[i..end];
                if end >= bytes.len() {
                    // Still typing the field name.
                    scope.context = TaskContext::Body;
                    return Outcome::Open(scope);
                }
                if word == "run" {
                    match scan_run(text, end, &mut scope) {
                        RunOutcome::Continue(next) => i = next,
                        RunOutcome::Open => return Outcome::Open(scope),
                        RunOutcome::Nothing => return Outcome::Nothing,
                    }
                    continue;
                }
                let Some(after_word) = skip_trivia(bytes, end) else {
                    return Outcome::Nothing;
                };
                if bytes.get(after_word) == Some(&b':') {
                    scope.present_fields.push(word.to_string());
                    match scan_field_value(text, after_word + 1, word, &mut scope) {
                        FieldOutcome::Continue(next) => i = next,
                        FieldOutcome::Open => return Outcome::Open(scope),
                        FieldOutcome::Nothing => return Outcome::Nothing,
                    }
                } else {
                    i = end;
                }
            }
            _ => i += 1,
        }
    }
}

enum FieldOutcome {
    Continue(usize),
    Open,
    Nothing,
}

/// Scans `value ;` after `field:`. `i` is just past the colon.
fn scan_field_value(text: &str, mut i: usize, field: &str, scope: &mut TaskScope) -> FieldOutcome {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut bracket_depth = 0i32;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => match skip_string(bytes, i) {
                Some(next) => {
                    i = next;
                    continue;
                }
                None => return FieldOutcome::Nothing,
            },
            b'/' if bytes.get(i + 1) == Some(&b'/') => match skip_line_comment(bytes, i) {
                Some(next) => {
                    i = next;
                    continue;
                }
                None => return FieldOutcome::Nothing,
            },
            b'/' if bytes.get(i + 1) == Some(&b'*') => match skip_block_comment(bytes, i) {
                Some(next) => {
                    i = next;
                    continue;
                }
                None => return FieldOutcome::Nothing,
            },
            b'{' | b'(' => depth += 1,
            b'}' | b')' => depth -= 1,
            b'[' => bracket_depth += 1,
            b']' => bracket_depth -= 1,
            b';' if depth <= 0 && bracket_depth <= 0 => return FieldOutcome::Continue(i + 1),
            _ => {}
        }
        i += 1;
    }
    scope.context = if field == "dependsOn" && bracket_depth > 0 {
        TaskContext::DependsOn
    } else {
        TaskContext::FieldValue(field.to_string())
    };
    FieldOutcome::Open
}

enum RunOutcome {
    Continue(usize),
    Open,
    Nothing,
}

/// `after_run` is the index just past the `run` keyword.
fn scan_run(text: &str, after_run: usize, scope: &mut TaskScope) -> RunOutcome {
    let bytes = text.as_bytes();
    let mut i = after_run;
    let mut shell = RunShell::Spar;
    let mut shell_seen = false;
    let mut os: Option<String> = None;
    loop {
        let Some(next) = skip_trivia(bytes, i) else {
            return RunOutcome::Nothing;
        };
        let had_space = next > i;
        i = next;
        if i >= bytes.len() {
            // Cursor right after `run` (no space yet) is still the field name.
            scope.context = if had_space {
                TaskContext::RunHeader {
                    shell_seen,
                    os_seen: os.is_some(),
                }
            } else {
                TaskContext::Body
            };
            return RunOutcome::Open;
        }
        match bytes[i] {
            b'{' => break,
            b if is_word(b) => {
                let end = word_end(bytes, i);
                let word = &text[i..end];
                if end >= bytes.len() {
                    // Header word still being typed: it doesn't count yet.
                    scope.context = TaskContext::RunHeader {
                        shell_seen,
                        os_seen: os.is_some(),
                    };
                    return RunOutcome::Open;
                }
                match word {
                    "spar" | "bash" => {
                        shell_seen = true;
                        shell = if word == "bash" {
                            RunShell::Bash
                        } else {
                            RunShell::Spar
                        };
                    }
                    w if OS_WORDS.contains(&w) => os = Some(w.to_string()),
                    _ => {}
                }
                i = end;
            }
            _ => {
                // Something that isn't a header (e.g. `run;`): not a run block.
                return RunOutcome::Continue(i);
            }
        }
    }
    // `bytes[i]` is the body's `{`.
    let body_start = i + 1;
    let native = shell == RunShell::Spar;
    let mut depth = 1i32;
    let mut j = body_start;
    while j < bytes.len() {
        match bytes[j] {
            b'"' | b'\'' if native => {
                let quote = bytes[j];
                j += 1;
                while j < bytes.len() && bytes[j] != quote {
                    if bytes[j] == b'\\' {
                        j += 1;
                    }
                    j += 1;
                }
                if j >= bytes.len() {
                    // Inside a quote in the body: still a run body.
                    break;
                }
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    scope.used_os_slots.push(os);
                    let after = j + 1;
                    let after = skip_trivia(bytes, after).unwrap_or(bytes.len());
                    return RunOutcome::Continue(if bytes.get(after) == Some(&b';') {
                        after + 1
                    } else {
                        j + 1
                    });
                }
            }
            _ => {}
        }
        j += 1;
    }
    scope.context = TaskContext::RunBody(shell);
    RunOutcome::Open
}

fn parse_params(text: &str) -> Vec<(String, String)> {
    let mut params = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut pieces = Vec::new();
    for (index, ch) in text.char_indices() {
        match ch {
            '<' | '[' | '(' => depth += 1,
            '>' | ']' | ')' => depth -= 1,
            ',' if depth <= 0 => {
                pieces.push(&text[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    pieces.push(&text[start..]);
    for piece in pieces {
        let piece = piece.trim().trim_start_matches('*').trim();
        let Some((name, ty)) = piece.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || !name.bytes().all(is_word) {
            continue;
        }
        let ty = ty.split('=').next().unwrap_or("").trim();
        params.push((name.to_string(), ty.to_string()));
    }
    params
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Splits `src` at the `|` cursor marker.
    fn at(src: &str) -> Option<TaskScope> {
        let offset = src.find('|').expect("cursor marker");
        let cleaned = src.replacen('|', "", 1);
        task_context(&cleaned, offset)
    }

    fn context(src: &str) -> TaskContext {
        at(src).expect("inside a task").context
    }

    #[test]
    fn empty_body_is_body_context() {
        assert_eq!(context("task X {\n    |\n};"), TaskContext::Body);
        assert_eq!(context("task X {|"), TaskContext::Body);
    }

    #[test]
    fn half_typed_field_name_is_body_context() {
        assert_eq!(context("task X {\n    qui|"), TaskContext::Body);
    }

    #[test]
    fn after_field_colon_is_field_value() {
        assert_eq!(
            context("task X {\n    quiet: |"),
            TaskContext::FieldValue("quiet".into())
        );
        assert_eq!(
            context("task X {\n    description: \"a;b\" + |"),
            TaskContext::FieldValue("description".into())
        );
    }

    #[test]
    fn completed_field_returns_to_body() {
        let scope = at("task X {\n    quiet: true;\n    |").unwrap();
        assert_eq!(scope.context, TaskContext::Body);
        assert_eq!(scope.present_fields, vec!["quiet".to_string()]);
    }

    #[test]
    fn inside_depends_on_brackets() {
        assert_eq!(
            context("task X {\n    dependsOn: [A, |"),
            TaskContext::DependsOn
        );
        assert_eq!(
            context("task X {\n    dependsOn: [A]|"),
            TaskContext::FieldValue("dependsOn".into())
        );
    }

    #[test]
    fn run_header_tracks_completed_words_only() {
        assert_eq!(
            context("task X {\n    run |"),
            TaskContext::RunHeader {
                shell_seen: false,
                os_seen: false
            }
        );
        assert_eq!(
            context("task X {\n    run ba|"),
            TaskContext::RunHeader {
                shell_seen: false,
                os_seen: false
            }
        );
        assert_eq!(
            context("task X {\n    run bash |"),
            TaskContext::RunHeader {
                shell_seen: true,
                os_seen: false
            }
        );
        assert_eq!(
            context("task X {\n    run bash macos |"),
            TaskContext::RunHeader {
                shell_seen: true,
                os_seen: true
            }
        );
        assert_eq!(
            context("task X {\n    run windows |"),
            TaskContext::RunHeader {
                shell_seen: false,
                os_seen: true
            }
        );
    }

    #[test]
    fn cursor_directly_after_run_is_still_the_field_name() {
        assert_eq!(context("task X {\n    run|"), TaskContext::Body);
    }

    #[test]
    fn run_bodies_report_their_shell() {
        assert_eq!(
            context("task X {\n    run bash { echo |"),
            TaskContext::RunBody(RunShell::Bash)
        );
        assert_eq!(
            context("task X {\n    run { echo |"),
            TaskContext::RunBody(RunShell::Spar)
        );
        assert_eq!(
            context("task X {\n    run windows { echo { nested } |"),
            TaskContext::RunBody(RunShell::Spar)
        );
    }

    #[test]
    fn completed_run_blocks_record_their_os_slots() {
        let scope = at("task X {\n    run { a; };\n    run bash windows { b; };\n    |").unwrap();
        assert_eq!(scope.context, TaskContext::Body);
        assert_eq!(scope.used_os_slots, vec![None, Some("windows".to_string())]);
    }

    #[test]
    fn params_context_and_parsed_params() {
        assert_eq!(context("task X(a: str, |"), TaskContext::Params);
        let scope = at("task X(env: str = \"a,b\", *rest: str) {\n    run { echo |").unwrap();
        assert_eq!(
            scope.params,
            vec![
                ("env".to_string(), "str".to_string()),
                ("rest".to_string(), "str".to_string())
            ]
        );
    }

    #[test]
    fn outside_and_after_a_task_is_not_a_task_context() {
        assert!(at("var x: int = 1;\n|").is_none());
        assert!(at("task X { run { a; }; };\n|").is_none());
        assert!(at("task X {\n    // note |").is_none());
    }

    #[test]
    fn innermost_open_task_wins_over_earlier_closed_tasks() {
        let scope = at("task A { run { a; }; };\ntask B {\n    |").expect("inside B");
        assert_eq!(scope.name, "B");
        assert!(scope.used_os_slots.is_empty());
    }

    #[test]
    fn fields_and_run_blocks_after_the_cursor_are_still_reported() {
        let src = "task X {\n    |\n    quiet: true;\n    run windows { a; };\n};";
        let scope = at(src).unwrap();
        assert_eq!(scope.context, TaskContext::Body);
        assert_eq!(scope.present_fields, vec!["quiet".to_string()]);
        assert_eq!(scope.used_os_slots, vec![Some("windows".to_string())]);
    }

    #[test]
    fn identifiers_named_task_at_depth_are_ignored() {
        assert!(at("section S {\n    task: 1;\n    |").is_none());
    }
}

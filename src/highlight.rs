//! Code-block syntax highlighting for transcript cells.
//!
//! syntect does the tokenizing; the colours come from `crate::theme`, so code
//! stays inside the DeepSeek palette and still passes through the 256/16-colour
//! quantizer. That also means no tmTheme assets ship in the binary.

use std::sync::OnceLock;

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

use crate::theme::Theme;

/// Which theme slot a token paints with. Resolved from the Sublime scope stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Plain,
    Keyword,
    String,
    Number,
    Comment,
    Type,
    Function,
    Variable,
    Punctuation,
}

impl Role {
    fn style(self, theme: &Theme, base: Style) -> Style {
        match self {
            Self::Plain => base,
            Self::Keyword => base.fg(theme.syn_keyword).add_modifier(Modifier::BOLD),
            Self::String => base.fg(theme.syn_string),
            Self::Number => base.fg(theme.syn_number),
            Self::Comment => base.fg(theme.syn_comment).add_modifier(Modifier::ITALIC),
            Self::Type => base.fg(theme.syn_type),
            Self::Function => base.fg(theme.syn_function),
            Self::Variable => base.fg(theme.syn_variable),
            Self::Punctuation => base.fg(theme.syn_punctuation),
        }
    }
}

/// Scope prefixes in priority order — the *last* match on the stack wins, so
/// more specific inner scopes override the enclosing ones.
const SCOPE_ROLES: &[(&str, Role)] = &[
    ("comment", Role::Comment),
    ("string", Role::String),
    ("constant.numeric", Role::Number),
    ("constant.language", Role::Keyword),
    ("constant.character", Role::String),
    ("constant", Role::Number),
    ("keyword", Role::Keyword),
    ("storage.type", Role::Keyword),
    ("storage.modifier", Role::Keyword),
    ("storage", Role::Keyword),
    ("support.type", Role::Type),
    ("support.class", Role::Type),
    ("support.function", Role::Function),
    ("support.constant", Role::Number),
    ("entity.name.function", Role::Function),
    ("entity.name.type", Role::Type),
    ("entity.name.class", Role::Type),
    ("entity.name.struct", Role::Type),
    ("entity.name.enum", Role::Type),
    ("entity.name.trait", Role::Type),
    ("entity.name.namespace", Role::Type),
    ("entity.name.tag", Role::Keyword),
    ("entity.name", Role::Function),
    ("entity.other.attribute-name", Role::Type),
    ("entity.other.inherited-class", Role::Type),
    ("variable.function", Role::Function),
    ("variable.parameter", Role::Variable),
    ("variable.language", Role::Keyword),
    ("variable", Role::Variable),
    ("meta.function-call", Role::Function),
    ("punctuation", Role::Punctuation),
    // Operators as plain punctuation: keyword-bold on every `=` and `+` is
    // far too loud in a terminal.
    ("keyword.operator", Role::Punctuation),
    // Delimiters read as part of the thing they delimit. These come *after*
    // `punctuation` on purpose — within one scope the last match wins, and a
    // `//` carries both `comment.line…` and `punctuation.definition.comment…`.
    ("punctuation.definition.comment", Role::Comment),
    ("punctuation.definition.string", Role::String),
    ("invalid", Role::Plain),
];

struct Matchers {
    syntaxes: SyntaxSet,
    roles: Vec<(Scope, Role)>,
    plain: SyntaxReference,
}

fn matchers() -> &'static Matchers {
    static CELL: OnceLock<Matchers> = OnceLock::new();
    CELL.get_or_init(|| {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let roles = SCOPE_ROLES
            .iter()
            .filter_map(|(name, role)| Scope::new(name).ok().map(|scope| (scope, *role)))
            .collect();
        let plain = syntaxes.find_syntax_plain_text().clone();
        Matchers {
            syntaxes,
            roles,
            plain,
        }
    })
}

/// Resolve a fenced-code info string (```rust, ```console, ```Dockerfile) to a
/// syntax. Falls back to plain text so an unknown or empty fence still renders.
fn syntax_for(lang: &str) -> &'static SyntaxReference {
    let m = matchers();
    let lang = lang.trim();
    let token = lang.split_whitespace().next().unwrap_or("");
    if token.is_empty() {
        return &m.plain;
    }
    // Common fence labels Sublime's syntaxes do not name directly.
    let token = match token.to_ascii_lowercase().as_str() {
        "sh" | "shell" | "zsh" | "console" | "shell-session" => "bash",
        "rs" => "rust",
        "py" => "python",
        "ts" | "tsx" => "TypeScript",
        "js" | "jsx" | "mjs" | "cjs" => "JavaScript",
        "yml" => "yaml",
        "md" => "markdown",
        "jsonc" | "json5" => "json",
        "docker" | "dockerfile" => "Dockerfile",
        "h" | "hpp" => "c++",
        "proto" => "protobuf",
        _ => token,
    };
    m.syntaxes
        .find_syntax_by_token(token)
        .or_else(|| m.syntaxes.find_syntax_by_extension(token))
        .unwrap_or(&m.plain)
}

fn role_for(stack: &ScopeStack, roles: &[(Scope, Role)]) -> Role {
    let mut best = Role::Plain;
    // Walk outermost → innermost so the deepest scope has the final say.
    for scope in stack.as_slice() {
        for (candidate, role) in roles {
            if candidate.is_prefix_of(*scope) {
                best = *role;
            }
        }
    }
    best
}

/// Highlight one code block into per-line spans. `base` carries the code
/// background so the block keeps its slab look. Never panics and never returns
/// fewer lines than the input: on any syntect error the remaining lines fall
/// back to unstyled text.
pub fn code_lines(code: &str, lang: &str, theme: &Theme, base: Style) -> Vec<Vec<Span<'static>>> {
    let m = matchers();
    let syntax = syntax_for(lang);
    let mut parse = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut out = Vec::new();

    for line in code.split_inclusive('\n') {
        let ops = match parse.parse_line(line, &m.syntaxes) {
            Ok(ops) => ops,
            Err(_) => {
                out.push(vec![Span::styled(
                    line.trim_end_matches('\n').to_string(),
                    base,
                )]);
                continue;
            }
        };
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut cursor = 0usize;
        let mut failed = false;
        for (offset, op) in ops {
            if offset > cursor {
                push_token(&mut spans, &line[cursor..offset], &stack, m, theme, base);
                cursor = offset;
            }
            if stack.apply(&op).is_err() {
                failed = true;
                break;
            }
        }
        if failed {
            out.push(vec![Span::styled(
                line.trim_end_matches('\n').to_string(),
                base,
            )]);
            continue;
        }
        if cursor < line.len() {
            push_token(&mut spans, &line[cursor..], &stack, m, theme, base);
        }
        out.push(spans);
    }
    if out.is_empty() {
        out.push(Vec::new());
    }
    out
}

fn push_token(
    spans: &mut Vec<Span<'static>>,
    text: &str,
    stack: &ScopeStack,
    m: &Matchers,
    theme: &Theme,
    base: Style,
) {
    let text = text.trim_end_matches('\n');
    if text.is_empty() {
        return;
    }
    let style = role_for(stack, &m.roles).style(theme, base);
    // Merge with the previous span when the style matches, so a line of code
    // does not explode into one span per character.
    if let Some(last) = spans.last_mut() {
        if last.style == style {
            let mut merged = std::mem::take(&mut last.content).into_owned();
            merged.push_str(text);
            last.content = merged.into();
            return;
        }
    }
    spans.push(Span::styled(text.to_string(), style));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles_of(code: &str, lang: &str) -> Vec<Vec<(String, Style)>> {
        let theme = crate::theme::DARK;
        code_lines(code, lang, &theme, Style::default())
            .into_iter()
            .map(|line| {
                line.into_iter()
                    .map(|s| (s.content.into_owned(), s.style))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn rust_keywords_strings_and_comments_get_distinct_colours() {
        let theme = crate::theme::DARK;
        let lines = roles_of("fn main() { // hi\n    let s = \"x\";\n}\n", "rust");
        let flat: Vec<_> = lines.iter().flatten().collect();

        let kw = flat
            .iter()
            .find(|(t, _)| t.trim() == "fn")
            .expect("fn is tokenized");
        assert_eq!(kw.1.fg, Some(theme.syn_keyword));

        let comment = flat
            .iter()
            .find(|(t, _)| t.contains("// hi"))
            .expect("comment is tokenized");
        assert_eq!(comment.1.fg, Some(theme.syn_comment));

        let string = flat
            .iter()
            .find(|(t, _)| t.contains('x') && t.len() <= 3)
            .expect("string body is tokenized");
        assert_eq!(string.1.fg, Some(theme.syn_string));
    }

    #[test]
    fn unknown_and_empty_fences_fall_back_to_plain_text() {
        for lang in ["", "not-a-language", "brainfuck-9000"] {
            let lines = roles_of("some text here\n", lang);
            assert_eq!(lines.len(), 1);
            let joined: String = lines[0].iter().map(|(t, _)| t.as_str()).collect();
            assert_eq!(joined, "some text here");
        }
    }

    #[test]
    fn shell_aliases_resolve_to_a_real_syntax() {
        for lang in ["sh", "shell", "console", "zsh", "bash"] {
            let lines = roles_of("echo \"hi\"\n", lang);
            let styles: Vec<_> = lines[0].iter().map(|(_, s)| s.fg).collect();
            assert!(
                styles.iter().any(|fg| fg.is_some()),
                "{lang} should tokenize to something"
            );
        }
    }

    #[test]
    fn line_count_is_preserved_so_wrapping_stays_aligned() {
        let lines = roles_of("a\nb\nc\n", "rust");
        assert_eq!(lines.len(), 3);
        let lines = roles_of("no trailing newline", "rust");
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn adjacent_same_style_tokens_merge_into_one_span() {
        let lines = code_lines(
            "let alpha_beta_gamma = 1;\n",
            "rust",
            &crate::theme::DARK,
            Style::default(),
        );
        // A dozen identifier characters must not become a dozen spans.
        assert!(
            lines[0].len() < 10,
            "expected merged spans, got {:?}",
            lines[0]
        );
    }
}

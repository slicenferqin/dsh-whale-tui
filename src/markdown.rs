//! CommonMark-to-ratatui rendering for assistant transcript cells.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme::Theme;

#[derive(Debug)]
struct ListState {
    next: Option<u64>,
}

struct MarkdownRenderer<'a> {
    theme: &'a Theme,
    base: Style,
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    styles: Vec<Style>,
    lists: Vec<ListState>,
    quote_depth: usize,
    in_code_block: bool,
    /// Fenced code is buffered whole and highlighted on close: syntect needs
    /// multi-line context (a string or comment can span lines), and
    /// pulldown-cmark hands us the body in arbitrary chunks.
    code_lang: String,
    code_buffer: String,
    table_cell: usize,
}

impl<'a> MarkdownRenderer<'a> {
    fn new(theme: &'a Theme, base: Style) -> Self {
        Self {
            theme,
            base,
            lines: Vec::new(),
            current: Vec::new(),
            styles: vec![base],
            lists: Vec::new(),
            quote_depth: 0,
            in_code_block: false,
            code_lang: String::new(),
            code_buffer: String::new(),
            table_cell: 0,
        }
    }

    fn style(&self) -> Style {
        self.styles.last().copied().unwrap_or(self.base)
    }

    fn push_style(&mut self, patch: Style) {
        self.styles.push(self.style().patch(patch));
    }

    fn pop_style(&mut self) {
        if self.styles.len() > 1 {
            self.styles.pop();
        }
    }

    fn ensure_prefix(&mut self) {
        if !self.current.is_empty() {
            return;
        }
        if self.quote_depth > 0 {
            self.current.push(Span::styled(
                "│ ".repeat(self.quote_depth),
                self.base.fg(self.theme.gray),
            ));
        }
        if self.in_code_block {
            self.current.push(Span::styled(
                "  ",
                self.base.fg(self.theme.accent_tool).bg(self.theme.code_bg),
            ));
        }
    }

    fn append_styled(&mut self, text: &str, style: Style) {
        let mut parts = text.split('\n').peekable();
        while let Some(part) = parts.next() {
            if !part.is_empty() {
                self.ensure_prefix();
                self.current.push(Span::styled(part.to_string(), style));
            }
            if parts.peek().is_some() {
                self.newline();
            }
        }
    }

    fn append(&mut self, text: &str) {
        self.append_styled(text, self.style());
    }

    fn newline(&mut self) {
        self.lines.push(Line {
            style: self.base,
            alignment: None,
            spans: std::mem::take(&mut self.current),
        });
    }

    fn newline_if_needed(&mut self) {
        if !self.current.is_empty() {
            self.newline();
        }
    }

    fn separate_blocks(&mut self) {
        self.newline_if_needed();
        if self.lines.last().is_some_and(|line| !line.spans.is_empty()) {
            self.lines.push(Line::styled("", self.base));
        }
    }

    /// Emit the buffered fence, tokenized and coloured from the theme. The
    /// two-space gutter matches `ensure_prefix` so highlighted and plain code
    /// blocks line up identically.
    fn flush_code_block(&mut self) {
        if self.code_buffer.is_empty() {
            return;
        }
        let code = std::mem::take(&mut self.code_buffer);
        let lang = std::mem::take(&mut self.code_lang);
        let slab = self.base.fg(self.theme.text_primary).bg(self.theme.code_bg);
        let gutter = Span::styled("  ", slab);
        for mut spans in crate::highlight::code_lines(&code, &lang, self.theme, slab) {
            let mut row = vec![gutter.clone()];
            row.append(&mut spans);
            self.lines.push(Line {
                style: self.base,
                alignment: None,
                spans: row,
            });
        }
    }

    fn end_block(&mut self, add_spacing: bool) {
        self.newline_if_needed();
        if add_spacing && self.lines.last().is_some_and(|line| !line.spans.is_empty()) {
            self.lines.push(Line::styled("", self.base));
        }
    }

    fn start_item(&mut self) {
        self.newline_if_needed();
        self.ensure_prefix();
        let depth = self.lists.len().saturating_sub(1);
        let marker = self
            .lists
            .last_mut()
            .map(|list| match list.next.as_mut() {
                Some(next) => {
                    let marker = format!("{next}. ");
                    *next += 1;
                    marker
                }
                None => "• ".to_string(),
            })
            .unwrap_or_else(|| "• ".to_string());
        let prefix = format!("{}{marker}", "  ".repeat(depth));
        self.current
            .push(Span::styled(prefix, self.base.fg(self.theme.accent_tool)));
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.separate_blocks();
                let color = match level {
                    HeadingLevel::H1 => self.theme.accent_plan,
                    HeadingLevel::H2 => self.theme.accent_user,
                    _ => self.theme.text_primary,
                };
                self.push_style(Style::default().fg(color).add_modifier(Modifier::BOLD));
            }
            Tag::BlockQuote(_) => {
                self.separate_blocks();
                self.quote_depth += 1;
                self.push_style(
                    Style::default()
                        .fg(self.theme.text_secondary)
                        .add_modifier(Modifier::ITALIC),
                );
            }
            Tag::CodeBlock(kind) => {
                self.separate_blocks();
                self.in_code_block = true;
                self.code_lang.clear();
                self.code_buffer.clear();
                self.push_style(
                    Style::default()
                        .fg(self.theme.text_primary)
                        .bg(self.theme.code_bg),
                );
                if let CodeBlockKind::Fenced(language) = kind {
                    if !language.is_empty() {
                        self.code_lang = language.to_string();
                        let style = self
                            .base
                            .fg(self.theme.gray_dim)
                            .bg(self.theme.code_bg)
                            .add_modifier(Modifier::BOLD);
                        self.append_styled(language.as_ref(), style);
                        self.newline();
                    }
                }
            }
            Tag::HtmlBlock => self.push_style(Style::default().fg(self.theme.gray)),
            Tag::List(start) => {
                if self.lists.is_empty() {
                    self.separate_blocks();
                }
                self.lists.push(ListState { next: start });
            }
            Tag::Item => self.start_item(),
            Tag::FootnoteDefinition(label) => {
                self.separate_blocks();
                self.append_styled(&format!("[{label}] "), self.base.fg(self.theme.gray));
            }
            Tag::DefinitionList => self.separate_blocks(),
            Tag::DefinitionListTitle => {
                self.push_style(Style::default().add_modifier(Modifier::BOLD));
            }
            Tag::DefinitionListDefinition => {
                self.append_styled("  ", self.base.fg(self.theme.gray));
            }
            Tag::Table(_) => self.separate_blocks(),
            Tag::TableHead => {
                self.push_style(Style::default().add_modifier(Modifier::BOLD));
            }
            Tag::TableRow => {
                self.newline_if_needed();
                self.table_cell = 0;
            }
            Tag::TableCell => {
                if self.table_cell > 0 {
                    self.append_styled(" │ ", self.base.fg(self.theme.gray_dim));
                }
                self.table_cell += 1;
            }
            Tag::Emphasis => {
                self.push_style(Style::default().add_modifier(Modifier::ITALIC));
            }
            Tag::Strong => {
                self.push_style(Style::default().add_modifier(Modifier::BOLD));
            }
            Tag::Strikethrough => {
                self.push_style(Style::default().add_modifier(Modifier::CROSSED_OUT));
            }
            Tag::Superscript | Tag::Subscript => {
                self.push_style(Style::default().fg(self.theme.text_secondary));
            }
            Tag::Link { .. } => self.push_style(
                Style::default()
                    .fg(self.theme.accent_user)
                    .add_modifier(Modifier::UNDERLINED),
            ),
            Tag::Image { .. } => {
                self.push_style(Style::default().fg(self.theme.accent_user));
                self.append("[image: ");
            }
            Tag::MetadataBlock(_) => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.end_block(self.lists.is_empty()),
            TagEnd::Heading(_) => {
                self.pop_style();
                self.end_block(true);
            }
            TagEnd::BlockQuote(_) => {
                self.pop_style();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.end_block(true);
            }
            TagEnd::CodeBlock => {
                self.flush_code_block();
                self.newline_if_needed();
                self.pop_style();
                self.in_code_block = false;
                self.end_block(true);
            }
            TagEnd::HtmlBlock => {
                self.pop_style();
                self.end_block(true);
            }
            TagEnd::List(_) => {
                self.newline_if_needed();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.end_block(true);
                }
            }
            TagEnd::Item => self.newline_if_needed(),
            TagEnd::FootnoteDefinition => self.end_block(true),
            TagEnd::DefinitionList => self.end_block(true),
            TagEnd::DefinitionListTitle => {
                self.pop_style();
                self.newline_if_needed();
            }
            TagEnd::DefinitionListDefinition => self.newline_if_needed(),
            TagEnd::Table => self.end_block(true),
            TagEnd::TableHead => self.pop_style(),
            TagEnd::TableRow => self.newline_if_needed(),
            TagEnd::TableCell => {}
            TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::Link => self.pop_style(),
            TagEnd::Image => {
                self.append("]");
                self.pop_style();
            }
            TagEnd::MetadataBlock(_) => {}
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => {
                if self.in_code_block {
                    self.code_buffer.push_str(text.as_ref());
                } else {
                    self.append(text.as_ref());
                }
            }
            Event::Code(code) | Event::InlineMath(code) => {
                let style = self
                    .style()
                    .fg(self.theme.accent_tool)
                    .bg(self.theme.code_bg);
                self.append_styled(code.as_ref(), style);
            }
            Event::DisplayMath(math) => {
                self.separate_blocks();
                let style = self.base.fg(self.theme.accent_tool).bg(self.theme.code_bg);
                self.append_styled(math.as_ref(), style);
                self.end_block(true);
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                self.append_styled(html.as_ref(), self.style().fg(self.theme.gray));
            }
            Event::FootnoteReference(label) => {
                self.append_styled(&format!("[{label}]"), self.base.fg(self.theme.gray));
            }
            Event::SoftBreak | Event::HardBreak => self.newline(),
            Event::Rule => {
                self.separate_blocks();
                self.append_styled("────────────────────", self.base.fg(self.theme.gray_dim));
                self.end_block(true);
            }
            Event::TaskListMarker(checked) => self.append_styled(
                if checked { "[x] " } else { "[ ] " },
                self.base.fg(self.theme.accent_tool),
            ),
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.newline_if_needed();
        while self.lines.last().is_some_and(|line| line.spans.is_empty()) {
            self.lines.pop();
        }
        if self.lines.is_empty() {
            self.lines.push(Line::styled("", self.base));
        }
        self.lines
    }
}

pub fn render(text: &str, theme: &Theme, base: Style) -> Vec<Line<'static>> {
    let options = Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let mut renderer = MarkdownRenderer::new(theme, base);
    for event in Parser::new_ext(text, options) {
        renderer.event(event);
    }
    renderer.finish()
}

/// Memoized `render`. Parsing plus syntect tokenizing costs a few hundred
/// microseconds per message, and `draw` re-renders every visible cell on every
/// frame — without this a long transcript spends a whole frame budget
/// re-highlighting text that has not changed. Only the streaming cell misses.
pub fn render_cached(text: &str, theme: &Theme, base: Style) -> Vec<Line<'static>> {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::hash::{Hash, Hasher};

    /// Wholesale-clear cap. Assistant cells are the only entries, so a few
    /// hundred covers any realistic transcript viewport.
    const CAP: usize = 512;

    thread_local! {
        static CACHE: RefCell<HashMap<(u64, &'static str), Vec<Line<'static>>>> =
            RefCell::new(HashMap::new());
    }

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    // `base` carries the cell background, which changes with the theme; hash it
    // so a theme switch cannot serve stale colours.
    format!("{base:?}").hash(&mut hasher);
    let key = (hasher.finish(), theme.name);

    CACHE.with(|cache| {
        if let Some(hit) = cache.borrow().get(&key) {
            return hit.clone();
        }
        let lines = render(text, theme, base);
        let mut cache = cache.borrow_mut();
        if cache.len() >= CAP {
            cache.clear();
        }
        cache.insert(key, lines.clone());
        lines
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::DARK;

    fn text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn renders_inline_markdown_without_delimiters() {
        let lines = render(
            "- **飞书相关**: 使用 \x60lark-doc\x60",
            &DARK,
            Style::default().fg(DARK.text_primary).bg(DARK.bg_base),
        );
        let rendered = text(&lines);

        assert!(!rendered.contains("**"));
        assert!(!rendered.contains('\x60'));
        assert!(rendered.contains("• 飞书相关: 使用 lark-doc"));
        assert!(lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .any(|span| span.style.add_modifier.contains(Modifier::BOLD)));
        assert!(lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .any(|span| span.content == "lark-doc" && span.style.bg == Some(DARK.code_bg)));
    }

    #[test]
    fn renders_fenced_code_and_language() {
        let lines = render(
            "\x60\x60\x60rust\nlet value = 1;\n\x60\x60\x60",
            &DARK,
            Style::default().fg(DARK.text_primary).bg(DARK.bg_base),
        );
        let rendered = text(&lines);

        assert!(rendered.contains("rust"));
        assert!(rendered.contains("let value = 1;"));
        assert!(!rendered.contains("\x60\x60\x60"));
    }

    #[test]
    fn incomplete_streaming_markdown_degrades_to_text() {
        let lines = render(
            "still streaming **bold",
            &DARK,
            Style::default().fg(DARK.text_primary).bg(DARK.bg_base),
        );
        assert!(text(&lines).contains("still streaming"));
    }

    #[test]
    fn cache_makes_repeat_renders_cheap() {
        let body = r#"Here is the fix.

```rust
fn main() {
    let items: Vec<String> = vec!["a".into(), "b".into()];
    for (i, item) in items.iter().enumerate() {
        println!("{i}: {item}"); // trace
    }
}
```

```bash
cargo test --all && cargo clippy -- -D warnings
```
"#;
        let style = Style::default();
        let _ = render_cached(body, &DARK, style);
        let start = std::time::Instant::now();
        let n = 500;
        for _ in 0..n {
            let _ = render_cached(body, &DARK, style);
        }
        let cached = start.elapsed().as_micros() as f64 / n as f64;

        let start = std::time::Instant::now();
        for _ in 0..n {
            let _ = render(body, &DARK, style);
        }
        let cold = start.elapsed().as_micros() as f64 / n as f64;
        assert!(cached * 4.0 < cold, "cache should be well under cold cost");
    }

    #[test]
    fn cache_is_keyed_on_theme_so_a_switch_repaints() {
        let body = "```rust\nfn x() {}\n```\n";
        let style = Style::default();
        let dark = render_cached(body, &DARK, style);
        let light = render_cached(body, &crate::theme::LIGHT, style);
        let dark_fg: Vec<_> = dark
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.style.fg))
            .collect();
        let light_fg: Vec<_> = light
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.style.fg))
            .collect();
        assert_ne!(
            dark_fg, light_fg,
            "a theme switch must not serve cached colours"
        );
    }
}

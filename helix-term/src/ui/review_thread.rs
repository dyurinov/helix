//! Renders review threads, including their Markdown comments, to styled lines for inline
//! display. Wrapping to the view width happens later in [`helix_view::review::layout_rows`].

use helix_core::syntax;
use helix_view::{
    graphics::Style,
    review::{str_width, RenderedLine, RenderedThread, Thread},
    theme::Modifier,
    Theme,
};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::ui::markdown::highlighted_code_block;

/// Rendered lines shown per comment before the rest is elided.
const MAX_COMMENT_LINES: usize = 40;

struct Styles {
    frame: Style,
    author: Style,
    draft: Style,
    meta: Style,
    resolved: Style,
    text: Style,
    code: Style,
    quote: Style,
    link: Style,
    list: Style,
    rule: Style,
    headings: [Style; 6],
}

impl Styles {
    fn new(theme: &Theme) -> Self {
        let get = |scope: &str, fallback: &str| {
            theme
                .try_get_exact(scope)
                .unwrap_or_else(|| theme.get(fallback))
        };
        let heading = |level: usize| theme.get(&format!("markup.heading.{level}"));
        Styles {
            frame: get("ui.virtual.review.frame", "info"),
            author: get("ui.virtual.review.author", "info").add_modifier(Modifier::BOLD),
            draft: get("ui.virtual.review.draft", "warning"),
            meta: get("ui.virtual.review.meta", "comment"),
            resolved: get("ui.virtual.review.resolved", "comment"),
            text: get("ui.virtual.review.body", "ui.text"),
            code: theme.get("markup.raw.inline"),
            quote: theme.get("markup.quote"),
            link: theme.get("markup.link.text"),
            list: theme.get("markup.list"),
            rule: theme.get("punctuation.special"),
            headings: [
                heading(1),
                heading(2),
                heading(3),
                heading(4),
                heading(5),
                heading(6),
            ],
        }
    }
}

/// Render a thread: a header, then each comment's Markdown body.
pub fn render_thread(thread: &Thread, theme: &Theme, loader: &syntax::Loader) -> RenderedThread {
    let styles = Styles::new(theme);
    let language = thread
        .path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("");
    let mut lines = Vec::new();

    let mut header = Vec::new();
    let marker = if thread.resolved { "✓ " } else { "● " };
    header.push((marker.to_string(), styles.meta));
    let author = thread
        .comments
        .first()
        .map_or("comment", |c| c.author.as_str());
    header.push((author.to_string(), styles.author));
    header.push((format!(" · {}", thread.line_label()), styles.meta));
    if thread.comments.len() > 1 {
        header.push((
            format!(" · {} comments", thread.comments.len()),
            styles.meta,
        ));
    }
    match thread.state_label() {
        "draft" => header.push((" · draft".to_string(), styles.draft)),
        "reply" => header.push((" · draft reply".to_string(), styles.draft)),
        "resolved" => header.push((" · resolved".to_string(), styles.resolved)),
        "outdated" => header.push((" · outdated".to_string(), styles.meta)),
        _ => {}
    }
    if thread.on_removed_lines {
        header.push((" · on removed lines".to_string(), styles.meta));
    }
    lines.push(RenderedLine::new(header));

    // Resolved threads collapse to their header.
    if !thread.resolved {
        for (i, comment) in thread.comments.iter().enumerate() {
            if i > 0 {
                let mut reply = vec![(format!("↳ {}", comment.author), styles.author)];
                if comment.draft {
                    reply.push((" · draft".to_string(), styles.draft));
                }
                lines.push(RenderedLine::new(reply));
            }
            let mut body = render_markdown(&comment.body, &styles, theme, loader, language);
            if body.len() > MAX_COMMENT_LINES {
                let more = body.len() - MAX_COMMENT_LINES;
                body.truncate(MAX_COMMENT_LINES);
                body.push(RenderedLine::new(vec![(
                    format!("… {more} more lines"),
                    styles.meta,
                )]));
            }
            lines.extend(body);
        }
    }

    RenderedThread {
        frame: styles.frame,
        lines,
    }
}

/// Converts GitHub flavored Markdown to styled lines.
struct MarkdownRenderer<'a> {
    styles: &'a Styles,
    theme: &'a Theme,
    loader: &'a syntax::Loader,
    /// Language of the commented file, used for ```suggestion blocks.
    file_language: &'a str,
    lines: Vec<RenderedLine>,
    spans: Vec<(String, Style)>,
    /// Width of the line prefix (quote bars, list indentation) of the current line.
    prefix_width: usize,
    style_stack: Vec<Style>,
    /// `Some(n)` for ordered lists, `None` for bullets.
    lists: Vec<Option<u64>>,
    /// Width of the current list item's bullet, for continuation lines.
    item_indent: usize,
    quote_depth: usize,
    code_block: Option<String>,
    code: String,
    blank_pending: bool,
    in_table_head: bool,
}

impl<'a> MarkdownRenderer<'a> {
    fn style(&self) -> Style {
        self.style_stack.last().copied().unwrap_or(self.styles.text)
    }

    fn quote_prefix(&self) -> Vec<(String, Style)> {
        (0..self.quote_depth)
            .map(|_| ("│ ".to_string(), self.styles.quote))
            .collect()
    }

    /// Start a line if none is in progress, emitting a pending blank line first.
    fn ensure_line(&mut self) {
        if !self.spans.is_empty() {
            return;
        }
        if self.blank_pending && !self.lines.is_empty() {
            let prefix = self.quote_prefix();
            self.lines.push(RenderedLine::new(prefix));
        }
        self.blank_pending = false;
        self.spans = self.quote_prefix();
        if self.item_indent > 0 {
            self.spans
                .push((" ".repeat(self.item_indent), Style::default()));
        }
        self.prefix_width = self.spans.iter().map(|(text, _)| str_width(text)).sum();
    }

    fn flush(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.spans);
        self.lines.push(RenderedLine {
            spans,
            indent: self.prefix_width,
        });
    }

    fn blank(&mut self) {
        self.flush();
        self.blank_pending = true;
    }

    fn text(&mut self, text: &str, style: Style) {
        self.ensure_line();
        self.spans.push((text.to_string(), style));
    }

    fn code_block(&mut self, language: &str, code: &str) {
        let language = if language == "suggestion" {
            self.ensure_line();
            self.spans
                .push(("Suggested change:".to_string(), self.styles.meta));
            self.flush();
            self.file_language
        } else {
            // Info strings may carry attributes: ```rust,ignore
            language
                .split(|c: char| c == ',' || c.is_whitespace())
                .next()
                .unwrap_or("")
        };
        let text = highlighted_code_block(code, language, Some(self.theme), self.loader, None);
        for line in text.lines {
            self.ensure_line();
            for span in line.0 {
                self.spans.push((span.content.into_owned(), span.style));
            }
            if self.spans.len() == self.quote_prefix().len() {
                // Keep empty code lines.
                self.spans.push((String::new(), self.styles.code));
            }
            self.flush();
        }
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                if self.code_block.is_some() {
                    self.code.push_str(&text);
                } else {
                    let style = self.style();
                    self.text(&text, style);
                }
            }
            Event::Code(code) => {
                let style = self.styles.code;
                self.text(&code, style);
            }
            Event::InlineHtml(html) | Event::Html(html) => {
                let tag = html.trim().to_ascii_lowercase();
                if tag.starts_with("<br") {
                    self.flush();
                } else if !tag.starts_with('<') {
                    let style = self.style();
                    self.text(html.trim_end(), style);
                }
                // Other tags (for example <details>) are dropped; their text is kept.
            }
            // GitHub renders single newlines in comments as line breaks.
            Event::SoftBreak | Event::HardBreak => self.flush(),
            Event::Rule => {
                self.flush();
                let style = self.styles.rule;
                self.text("───", style);
                self.blank();
            }
            Event::TaskListMarker(checked) => {
                let style = self.styles.list;
                self.text(if checked { "☑ " } else { "☐ " }, style);
            }
            Event::FootnoteReference(name) => {
                let style = self.styles.meta;
                self.text(&format!("[^{name}]"), style);
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.flush();
                let index = match level {
                    HeadingLevel::H1 => 0,
                    HeadingLevel::H2 => 1,
                    HeadingLevel::H3 => 2,
                    HeadingLevel::H4 => 3,
                    HeadingLevel::H5 => 4,
                    HeadingLevel::H6 => 5,
                };
                self.style_stack
                    .push(self.styles.headings[index].add_modifier(Modifier::BOLD));
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.quote_depth += 1;
                self.style_stack.push(self.styles.quote);
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                let language = match kind {
                    CodeBlockKind::Fenced(language) => language.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code_block = Some(language);
                self.code.clear();
            }
            Tag::List(start) => {
                self.flush();
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush();
                let depth = self.lists.len().max(1);
                let bullet = match self.lists.last_mut() {
                    Some(Some(number)) => {
                        let bullet = format!("{number}. ");
                        *number += 1;
                        bullet
                    }
                    _ => "• ".to_string(),
                };
                let indent = "  ".repeat(depth - 1);
                // The bullet is part of this line; continuation lines align after it.
                self.item_indent = 0;
                self.ensure_line();
                let style = self.styles.list;
                self.spans.push((format!("{indent}{bullet}"), style));
                self.item_indent = str_width(&indent) + str_width(&bullet);
                self.prefix_width += self.item_indent;
            }
            Tag::Emphasis => {
                let style = self.style().add_modifier(Modifier::ITALIC);
                self.style_stack.push(style);
            }
            Tag::Strong => {
                let style = self.style().add_modifier(Modifier::BOLD);
                self.style_stack.push(style);
            }
            Tag::Strikethrough => {
                let style = self.style().add_modifier(Modifier::CROSSED_OUT);
                self.style_stack.push(style);
            }
            Tag::Link { .. } => {
                let style = self.style().patch(self.styles.link);
                self.style_stack.push(style);
            }
            Tag::Image { .. } => {
                let style = self.styles.meta;
                self.text("[image: ", style);
                self.style_stack.push(style);
            }
            Tag::Table(_) => self.flush(),
            Tag::TableHead => {
                self.in_table_head = true;
                let style = self.style().add_modifier(Modifier::BOLD);
                self.style_stack.push(style);
            }
            Tag::TableRow => {}
            Tag::TableCell => self.ensure_line(),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.blank(),
            TagEnd::Heading(_) => {
                self.style_stack.pop();
                self.blank();
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.style_stack.pop();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.blank();
            }
            TagEnd::CodeBlock => {
                let language = self.code_block.take().unwrap_or_default();
                let code = std::mem::take(&mut self.code);
                self.code_block(&language, code.trim_end_matches('\n'));
                self.blank();
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.item_indent = 0;
                    self.blank();
                }
            }
            TagEnd::Item => {
                self.flush();
                // Tight list items do not end with a paragraph; loose ones leave a blank
                // line pending, which looks wrong between bullets.
                self.blank_pending = false;
                let depth = self.lists.len();
                self.item_indent = if depth > 1 { 2 * (depth - 1) } else { 0 };
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                self.style_stack.pop();
            }
            TagEnd::Image => {
                self.style_stack.pop();
                let style = self.styles.meta;
                self.text("]", style);
            }
            TagEnd::TableHead => {
                self.style_stack.pop();
                self.in_table_head = false;
                self.flush();
            }
            TagEnd::TableRow => self.flush(),
            TagEnd::TableCell => {
                let style = self.styles.rule;
                self.text(" │ ", style);
            }
            TagEnd::Table => {
                self.flush();
                self.blank();
            }
            _ => {}
        }
    }
}

fn render_markdown(
    body: &str,
    styles: &Styles,
    theme: &Theme,
    loader: &syntax::Loader,
    file_language: &str,
) -> Vec<RenderedLine> {
    let mut renderer = MarkdownRenderer {
        styles,
        theme,
        loader,
        file_language,
        lines: Vec::new(),
        spans: Vec::new(),
        prefix_width: 0,
        style_stack: Vec::new(),
        lists: Vec::new(),
        item_indent: 0,
        quote_depth: 0,
        code_block: None,
        code: String::new(),
        blank_pending: false,
        in_table_head: false,
    };
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_TABLES);
    for event in Parser::new_ext(body, options) {
        renderer.event(event);
    }
    renderer.flush();
    // Drop the trailing cell separators of table rows.
    for line in &mut renderer.lines {
        if line.spans.last().is_some_and(|(text, _)| text == " │ ") {
            line.spans.pop();
        }
    }
    if renderer.lines.is_empty() {
        renderer.lines.push(RenderedLine::default());
    }
    renderer.lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_loader() -> syntax::Loader {
        syntax::Loader::new(syntax::config::Configuration {
            language: vec![],
            language_server: Default::default(),
        })
        .unwrap()
    }

    fn render(body: &str) -> Vec<String> {
        let theme = Theme::default();
        let loader = empty_loader();
        let styles = Styles::new(&theme);
        render_markdown(body, &styles, &theme, &loader, "rs")
            .into_iter()
            .map(|line| line.spans.into_iter().map(|(text, _)| text).collect())
            .collect()
    }

    #[test]
    fn renders_github_markdown() {
        assert_eq!(render("plain **bold** `code`"), ["plain bold code"]);
        // Single newlines are line breaks in GitHub comments.
        assert_eq!(render("one\ntwo"), ["one", "two"]);
        assert_eq!(render("para\n\nnext"), ["para", "", "next"]);
        assert_eq!(render("# Title\nbody"), ["Title", "", "body"]);
        assert_eq!(
            render("> quoted\n> more\n\nafter"),
            ["│ quoted", "│ more", "", "after"]
        );
        assert_eq!(render("- a\n- b\n  - c"), ["• a", "• b", "  • c"]);
        assert_eq!(render("1. a\n2. b"), ["1. a", "2. b"]);
        assert_eq!(render("- [x] done\n- [ ] todo"), ["• ☑ done", "• ☐ todo"]);
        assert_eq!(render("a<br>b"), ["a", "b"]);
        assert_eq!(render("---"), ["───"]);
        assert_eq!(
            render("| a | b |\n|---|---|\n| 1 | 2 |"),
            ["a │ b", "1 │ 2"]
        );
    }

    #[test]
    fn renders_code_blocks() {
        assert_eq!(
            render("see\n```\nlet a = 1;\n\nlet b = 2;\n```\nafter"),
            ["see", "", "let a = 1;", "", "let b = 2;", "", "after"]
        );
        assert_eq!(
            render("```suggestion\nlet x = 2;\n```"),
            ["Suggested change:", "let x = 2;"]
        );
    }

    #[test]
    fn list_continuations_align_with_text() {
        let theme = Theme::default();
        let loader = empty_loader();
        let styles = Styles::new(&theme);
        let lines = render_markdown("- item\n  continued", &styles, &theme, &loader, "");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].indent, 2);
        let second: String = lines[1].spans.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(second, "  continued");
    }
}

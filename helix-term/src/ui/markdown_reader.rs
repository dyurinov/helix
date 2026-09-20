//! Rendered Markdown reading, kept separate from the editable document.
use helix_core::{syntax, unicode::width::UnicodeWidthStr};
use helix_view::{
    graphics::{Modifier, Rect, Style},
    Theme,
};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use tui::{
    buffer::Buffer,
    text::{Span, Spans, Text},
    widgets::{Paragraph, Widget, Wrap},
};

#[derive(Default)]
pub(super) struct MarkdownReader {
    pub active: bool,
    pub scroll: u16,
    pub pending_g: bool,
    pub pending_space: bool,
    pub search: Option<String>,
    pub query: String,
    pub help: bool,
    pub moved: bool,
    pub focus_source: Option<usize>,
    cache: Option<Rendered>,
    version: i32,
    theme: String,
    width: u16,
}

struct Rendered {
    text: Text<'static>,
    sources: Vec<usize>,
    rows: Vec<usize>,
    headings: Vec<usize>,
}

pub(super) fn content_area(area: Rect) -> Rect {
    let mut area = area.clip_bottom(1).clip_top(1);
    let width = area.width.saturating_sub(4).clamp(1, 100).min(area.width);
    area.x += area.width.saturating_sub(width) / 2;
    area.width = width;
    area
}

impl MarkdownReader {
    pub fn is_current(&self, version: i32, theme: &Theme, width: u16) -> bool {
        self.cache.is_some()
            && self.version == version
            && self.theme == theme.name()
            && self.width == width
    }
    pub fn prepare(
        &mut self,
        source: &str,
        version: i32,
        theme: &Theme,
        loader: &syntax::Loader,
        width: u16,
        cursor_byte: usize,
    ) {
        if self.cache.is_some()
            && self.version == version
            && self.theme == theme.name()
            && self.width == width
        {
            return;
        }
        let previous_source = self.source_byte().unwrap_or(cursor_byte);
        let rendered = render_markdown(source, theme, loader, width);
        let line = rendered
            .sources
            .partition_point(|&source| source <= previous_source)
            .saturating_sub(1);
        let line = rendered.sources.get(line).map_or(0, |source| {
            rendered.sources.partition_point(|offset| offset < source)
        });
        self.scroll = rendered
            .rows
            .iter()
            .position(|&row| row >= line)
            .unwrap_or(0)
            .min(u16::MAX as usize) as u16;
        self.version = version;
        self.theme = theme.name().to_owned();
        self.width = width;
        self.cache = Some(rendered);
    }

    pub fn invalidate(&mut self) {
        self.cache = None;
        self.focus_source = None;
    }

    pub fn source_byte(&self) -> Option<usize> {
        if self.focus_source.is_some() {
            return self.focus_source;
        }
        let cache = self.cache.as_ref()?;
        let line = *cache.rows.get(self.scroll as usize)?;
        cache.sources.get(line).copied()
    }

    pub fn max_scroll(&self, height: u16) -> u16 {
        self.cache.as_ref().map_or(0, |cache| {
            cache
                .rows
                .len()
                .saturating_sub(height as usize)
                .min(u16::MAX as usize) as u16
        })
    }

    pub fn heading(&mut self, forward: bool) {
        let Some(cache) = &self.cache else { return };
        let current = self.navigation_row();
        let rows = cache
            .headings
            .iter()
            .filter_map(|heading| cache.rows.iter().position(|row| row == heading));
        let target = if forward {
            rows.filter(|&row| row > current).min()
        } else {
            rows.filter(|&row| row < current).max()
        };
        if let Some(row) = target {
            self.focus_source = cache.sources.get(cache.rows[row]).copied();
            self.scroll = row.min(u16::MAX as usize) as u16;
            self.moved = true;
        }
    }

    pub fn find(&mut self, forward: bool, include_current: bool) -> bool {
        let Some(cache) = &self.cache else {
            return false;
        };
        if self.query.is_empty() {
            return false;
        }
        let needle = self.query.to_lowercase();
        let matches: Vec<usize> = cache
            .text
            .lines
            .iter()
            .enumerate()
            .filter_map(|(index, line)| {
                let text: String = line.0.iter().map(|span| span.content.as_ref()).collect();
                text.to_lowercase()
                    .contains(&needle)
                    .then(|| cache.rows.iter().position(|&row| row == index))
                    .flatten()
            })
            .collect();
        let current = self.navigation_row();
        let target = if forward {
            matches
                .iter()
                .find(|&&row| row > current || (include_current && row == current))
                .or(matches.first())
        } else {
            matches
                .iter()
                .rev()
                .find(|&&row| row < current)
                .or(matches.last())
        };
        if let Some(&row) = target {
            self.focus_source = cache.sources.get(cache.rows[row]).copied();
            self.scroll = row.min(u16::MAX as usize) as u16;
            self.moved = true;
            true
        } else {
            false
        }
    }

    fn navigation_row(&self) -> usize {
        self.focus_source
            .and_then(|source| {
                let cache = self.cache.as_ref()?;
                let line = cache.sources.iter().position(|&offset| offset == source)?;
                cache.rows.iter().position(|&row| row == line)
            })
            .unwrap_or(self.scroll as usize)
    }

    pub fn draw(&self, area: Rect, surface: &mut Buffer, theme: &Theme, name: &str) {
        let Some(cache) = &self.cache else { return };
        let body = content_area(area);
        let scroll = self.scroll.min(self.max_scroll(body.height));
        if self.help {
            let help = Text::from("Preview controls\n\nSpace m / q / Esc   Return to source\ng w                 Return to source and jump\nj / k               Scroll a line\nCtrl-d / Ctrl-u     Scroll half a page\nPage Down / Up      Scroll a page\ng g / G             Beginning / end\n[ / ]               Previous / next heading\n/                   Search rendered text\nn / N               Next / previous match\n?                   Close this help");
            Paragraph::new(&help)
                .style(theme.get("ui.text"))
                .wrap(Wrap { trim: false })
                .render(body, surface);
        } else {
            Paragraph::new(&cache.text)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0))
                .render(body, surface);
            let focused_row = self.navigation_row();
            if self.focus_source.is_some()
                && focused_row >= usize::from(scroll)
                && focused_row < usize::from(scroll) + usize::from(body.height)
            {
                surface.set_style(
                    Rect {
                        x: body.x,
                        y: body.y + (focused_row - usize::from(scroll)) as u16,
                        width: body.width,
                        height: 1,
                    },
                    theme.get("ui.selection"),
                );
            }
        }
        let footer = area.clip_top(area.height.saturating_sub(1));
        let percent = if self.max_scroll(body.height) == 0 {
            100
        } else {
            usize::from(scroll) * 100 / usize::from(self.max_scroll(body.height))
        };
        let label = if let Some(query) = &self.search {
            format!(" /{query}  · Enter find · Esc cancel")
        } else if self.help {
            " PREVIEW HELP   ? return · q close".into()
        } else if area.width < 80 {
            format!(" PREVIEW {percent}% · gw edit · q close · / find · ? help")
        } else {
            format!(" PREVIEW  {name}  {percent}%   gw edit · q close · / find · ? help")
        };
        let style = theme.get("ui.statusline");
        surface.set_style(footer, style);
        surface.set_stringn(footer.x, footer.y, &label, footer.width as usize, style);
    }
}

struct Builder<'a> {
    lines: Vec<Spans<'static>>,
    sources: Vec<usize>,
    headings: Vec<usize>,
    spans: Vec<Span<'static>>,
    source: usize,
    styles: Vec<Style>,
    lists: Vec<Option<u64>>,
    quote_depth: usize,
    code: Option<String>,
    comment: bool,
    table: Option<Vec<Vec<String>>>,
    row: Vec<String>,
    theme: &'a Theme,
    width: u16,
}

impl Builder<'_> {
    fn style(&self) -> Style {
        self.styles
            .last()
            .copied()
            .unwrap_or_else(|| self.theme.get("ui.text"))
    }
    fn push(&mut self, text: impl Into<String>, style: Style) {
        self.spans.push(Span::styled(text.into(), style));
    }
    fn flush(&mut self) {
        if !self.spans.is_empty() {
            self.lines
                .push(Spans::from(std::mem::take(&mut self.spans)));
            self.sources.push(self.source);
        }
    }
    fn blank(&mut self) {
        self.flush();
        if self.lines.last().is_some_and(|line| !line.0.is_empty()) {
            self.lines.push(Spans::default());
            self.sources.push(self.source);
        }
    }
    fn html(&mut self, raw: &str) {
        let mut remaining = raw;
        while !remaining.is_empty() {
            if self.comment {
                let Some(end) = remaining.find("-->") else {
                    return;
                };
                remaining = &remaining[end + 3..];
                self.comment = false;
            } else if let Some(rest) = remaining.strip_prefix("<!--") {
                self.comment = true;
                remaining = rest;
            } else if remaining.starts_with('<') {
                let Some(end) = remaining.find('>') else {
                    return;
                };
                let tag = &remaining[..=end];
                if matches!(tag, "<br>" | "<br/>" | "<br />") {
                    self.flush();
                }
                remaining = &remaining[end + 1..];
            } else {
                let end = remaining.find('<').unwrap_or(remaining.len());
                let text = &remaining[..end];
                if !text.trim().is_empty() {
                    self.push(text, self.style());
                }
                remaining = &remaining[end..];
            }
        }
    }
    fn finish_table(&mut self) {
        let rows = self.table.take().unwrap_or_default();
        let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return;
        }
        let available = usize::from(self.width)
            .saturating_sub(3 * columns.saturating_sub(1))
            .max(columns);
        let mut widths = vec![1; columns];
        for row in &rows {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(cell.width());
            }
        }
        while widths.iter().sum::<usize>() > available {
            let Some((index, &width)) = widths.iter().enumerate().max_by_key(|(_, width)| *width)
            else {
                break;
            };
            if width <= 1 {
                break;
            }
            widths[index] -= 1;
        }
        for (index, row) in rows.iter().enumerate() {
            let wrapped: Vec<Vec<String>> = row
                .iter()
                .enumerate()
                .map(|(column, value)| wrap_cell(value, widths[column]))
                .collect();
            for line in 0..wrapped.iter().map(Vec::len).max().unwrap_or(1) {
                for (column, &width) in widths.iter().enumerate() {
                    if column > 0 {
                        self.push(" │ ", self.theme.get("ui.text.inactive"));
                    }
                    let cell = wrapped
                        .get(column)
                        .and_then(|cell| cell.get(line))
                        .map(String::as_str)
                        .unwrap_or("");
                    let style = if index == 0 {
                        self.theme.get("ui.text").add_modifier(Modifier::BOLD)
                    } else {
                        self.theme.get("ui.text")
                    };
                    self.push(
                        format!("{cell}{}", " ".repeat(width.saturating_sub(cell.width()))),
                        style,
                    );
                }
                self.flush();
            }
            if index == 0 {
                self.push(
                    widths
                        .iter()
                        .map(|width| "─".repeat(*width))
                        .collect::<Vec<_>>()
                        .join("─┼─"),
                    self.theme.get("ui.text.inactive"),
                );
                self.flush();
            }
        }
        self.blank();
    }
}

fn wrap_cell(text: &str, width: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    for word in text.split_whitespace() {
        let line = lines.last_mut().unwrap();
        if !line.is_empty() && line.width() + 1 + word.width() <= width {
            line.push(' ');
        } else if !line.is_empty() {
            lines.push(String::new());
        }
        for character in word.chars() {
            let value = character.to_string();
            if !lines.last().unwrap().is_empty()
                && lines.last().unwrap().width() + value.width() > width
            {
                lines.push(String::new());
            }
            lines.last_mut().unwrap().push(character);
        }
    }
    lines
}

fn render_markdown(source: &str, theme: &Theme, loader: &syntax::Loader, width: u16) -> Rendered {
    let mut out = Builder {
        lines: vec![],
        sources: vec![],
        headings: vec![],
        spans: vec![],
        source: 0,
        styles: vec![],
        lists: vec![],
        quote_depth: 0,
        code: None,
        comment: false,
        table: None,
        row: vec![],
        theme,
        width,
    };
    let options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
        match event {
            Event::Start(tag) => {
                let mut style = out.style();
                match &tag {
                    Tag::Heading { level, .. } => {
                        out.blank();
                        out.source = range.start;
                        out.headings.push(out.lines.len());
                        let scope = match level {
                            HeadingLevel::H1 => "markup.heading.1",
                            HeadingLevel::H2 => "markup.heading.2",
                            HeadingLevel::H3 => "markup.heading.3",
                            _ => "markup.heading.4",
                        };
                        style = theme.get(scope).add_modifier(Modifier::BOLD);
                    }
                    Tag::Paragraph => {
                        if out.lists.is_empty() {
                            out.blank();
                            out.source = range.start;
                        }
                        if out.quote_depth > 0 {
                            out.push("│ ".repeat(out.quote_depth), theme.get("ui.text.inactive"));
                        }
                    }
                    Tag::List(start) => {
                        out.flush();
                        out.lists.push(*start);
                    }
                    Tag::Item => {
                        out.flush();
                        out.source = range.start;
                        let prefix = match out.lists.last_mut() {
                            Some(Some(number)) => {
                                let prefix = format!("{number}. ");
                                *number += 1;
                                prefix
                            }
                            _ => "• ".into(),
                        };
                        out.push(
                            format!("{}{prefix}", "  ".repeat(out.lists.len().saturating_sub(1))),
                            theme.get("markup.list"),
                        );
                    }
                    Tag::CodeBlock(kind) => {
                        out.blank();
                        out.source = range.start;
                        let language = match kind {
                            CodeBlockKind::Fenced(language) => {
                                language.split_whitespace().next().unwrap_or("")
                            }
                            _ => "",
                        };
                        out.code = Some(language.to_owned());
                        if !language.is_empty() {
                            out.push(format!("  {language}"), theme.get("ui.text.inactive"));
                            out.flush();
                        }
                    }
                    Tag::BlockQuote(_) => {
                        out.blank();
                        out.quote_depth += 1;
                    }
                    Tag::Emphasis => style = style.add_modifier(Modifier::ITALIC),
                    Tag::Strong => style = style.add_modifier(Modifier::BOLD),
                    Tag::Strikethrough => style = style.add_modifier(Modifier::CROSSED_OUT),
                    Tag::Link { .. } => {
                        style = theme.get("markup.link.text").add_modifier(Modifier::BOLD)
                    }
                    Tag::Table(_) => {
                        out.blank();
                        out.source = range.start;
                        out.table = Some(vec![]);
                    }
                    Tag::TableHead | Tag::TableRow => out.row.clear(),
                    Tag::TableCell => out.spans.clear(),
                    _ => {}
                }
                out.styles.push(style);
            }
            Event::End(tag) => {
                match tag {
                    TagEnd::Heading(_) => out.blank(),
                    TagEnd::Paragraph => {
                        if out.lists.is_empty() {
                            out.blank();
                        } else {
                            out.flush();
                        }
                    }
                    TagEnd::Item => out.flush(),
                    TagEnd::List(_) => {
                        out.lists.pop();
                        if out.lists.is_empty() {
                            out.blank();
                        }
                    }
                    TagEnd::CodeBlock => {
                        out.code = None;
                        out.blank();
                    }
                    TagEnd::BlockQuote(_) => {
                        out.quote_depth = out.quote_depth.saturating_sub(1);
                        out.blank();
                    }
                    TagEnd::TableCell => {
                        out.row.push(
                            out.spans
                                .drain(..)
                                .map(|span| span.content.into_owned())
                                .collect(),
                        );
                    }
                    TagEnd::TableHead | TagEnd::TableRow => {
                        if let Some(table) = &mut out.table {
                            table.push(std::mem::take(&mut out.row));
                        }
                    }
                    TagEnd::Table => out.finish_table(),
                    _ => {}
                }
                out.styles.pop();
            }
            Event::Text(text) => {
                if let Some(language) = &out.code {
                    let code = super::markdown::highlighted_code_block(
                        &text,
                        language,
                        Some(theme),
                        loader,
                        None,
                    );
                    let mut offset = range.start;
                    for (line, raw) in code.lines.into_iter().zip(text.split('\n')) {
                        out.source = offset;
                        out.push("  ", theme.get("markup.raw.block"));
                        out.spans.extend(
                            line.0
                                .into_iter()
                                .map(|span| Span::styled(span.content.into_owned(), span.style)),
                        );
                        out.flush();
                        offset += raw.len() + 1;
                    }
                } else {
                    out.push(text.into_string(), out.style());
                }
            }
            Event::Code(text) => out.push(text.into_string(), theme.get("markup.raw.inline")),
            Event::Html(html) | Event::InlineHtml(html) => out.html(&html),
            Event::SoftBreak => out.push(" ", out.style()),
            Event::HardBreak => out.flush(),
            Event::TaskListMarker(checked) => {
                if let Some(prefix) = out.spans.last_mut() {
                    if let Some(indent) = prefix.content.strip_suffix("• ") {
                        prefix.content = indent.to_owned().into();
                    }
                }
                out.push(if checked { "☑ " } else { "☐ " }, theme.get("markup.list"))
            }
            Event::Rule => {
                out.blank();
                out.source = range.start;
                out.push(
                    "─".repeat(usize::from(width.min(40))),
                    theme.get("ui.text.inactive"),
                );
                out.blank();
            }
            _ => {}
        }
    }
    out.flush();
    while out.lines.last().is_some_and(|line| line.0.is_empty()) {
        out.lines.pop();
        out.sources.pop();
    }
    let mut rows = Vec::new();
    for (index, line) in out.lines.iter().enumerate() {
        let text = Text::from(vec![line.clone()]);
        let (_, height) = Paragraph::new(&text)
            .wrap(Wrap { trim: false })
            .required_size(width.max(1));
        rows.extend(std::iter::repeat_n(index, usize::from(height)));
    }
    Rendered {
        text: Text::from(out.lines),
        sources: out.sources,
        rows,
        headings: out.headings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn render(source: &str) -> Rendered {
        let loader = syntax::Loader::new(syntax::config::Configuration {
            language: vec![],
            language_server: Default::default(),
        })
        .unwrap();
        render_markdown(source, &Theme::default(), &loader, 60)
    }
    fn plain(rendered: &Rendered) -> String {
        rendered
            .text
            .lines
            .iter()
            .map(|line| {
                line.0
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn comments_are_hidden_but_literal_code_is_preserved() {
        let result = render("<!-- secret\ncomment -->\n# Heading\n\nBefore <!-- inline --> after.\n\n`<!-- literal -->`\n\n```html\n<!-- example -->\n```\n");
        let text = plain(&result);
        assert!(!text.contains("secret"));
        assert!(!text.contains("inline"));
        assert!(text.contains("Before  after."));
        assert!(text.contains("<!-- literal -->"));
        assert!(text.contains("<!-- example -->"));
    }
    #[test]
    fn soft_lines_join_and_tables_and_tasks_render() {
        let result = render("A paragraph\ncontinued here.\n\n- [x] Done\n- [ ] Later\n\n| Name | State |\n| --- | --- |\n| Preview | Ready |\n");
        let text = plain(&result);
        assert!(text.contains("A paragraph continued here."));
        assert!(text.contains("☑ Done"));
        assert!(text.contains("☐ Later"));
        assert!(text.contains("Name"));
        assert!(text.contains("Preview"));
        assert!(text.contains('│'));
        assert!(!text.contains("| ---"));
    }
    #[test]
    fn source_mapping_skips_comments_and_tracks_sections() {
        let source = "<!-- hidden -->\n# First\n\nText.\n\n## Second\n\nMore.\n";
        let result = render(source);
        assert_eq!(
            result.sources[result.headings[0]],
            source.find("# First").unwrap()
        );
        assert_eq!(
            result.sources[result.headings[1]],
            source.find("## Second").unwrap()
        );
        assert!(result.sources.windows(2).all(|pair| pair[0] <= pair[1]));
    }
}

//! Renders [Mermaid](https://mermaid.js.org) diagrams as box drawing text for Markdown views.
//!
//! Flowcharts, state, class and entity relationship diagrams share a layered graph layout
//! ([`layout`]). Sequence diagrams, pie charts and mindmaps have their own renderers. Diagram
//! types that are not supported, and sources that cannot be parsed, return `None` so callers
//! can show the source instead.

mod charts;
mod diagrams;
mod layout;
mod sequence;

use helix_core::unicode::width::{UnicodeWidthChar, UnicodeWidthStr};
use helix_view::{graphics::Style, theme::Modifier, Theme};

/// Larger sources are not worth laying out for a terminal.
const MAX_SOURCE: usize = 32 * 1024;

/// The kind of a piece of a diagram, mapped to a theme style by [`Role::style`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Text,
    Title,
    Border,
    Edge,
    Arrow,
    Label,
    Muted,
}

impl Role {
    /// The theme style for this role. `markup.diagram.*` scopes override the defaults.
    pub fn style(self, theme: &Theme) -> Style {
        let (scope, fallback) = match self {
            Role::Text => ("markup.diagram.text", "ui.text"),
            Role::Title => ("markup.diagram.title", "markup.heading.1"),
            Role::Border => ("markup.diagram.border", "type"),
            Role::Edge => ("markup.diagram.edge", "punctuation"),
            Role::Arrow => ("markup.diagram.arrow", "operator"),
            Role::Label => ("markup.diagram.label", "string"),
            Role::Muted => ("markup.diagram.muted", "comment"),
        };
        let style = theme
            .try_get_exact(scope)
            .unwrap_or_else(|| theme.get(fallback));
        if self == Role::Title {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        }
    }
}

/// One rendered row: text runs with their roles.
pub type Line = Vec<(String, Role)>;

/// Render a Mermaid diagram, aiming to stay within `max_width` columns. Wider diagrams are
/// returned as they are; callers clip them.
pub fn render(source: &str, max_width: usize) -> Option<Vec<Line>> {
    if source.len() > MAX_SOURCE {
        return None;
    }
    let lines = source_lines(source);
    let (header, body) = lines.split_first()?;
    let header = header.trim();
    let keyword = header
        .split(|c: char| c.is_whitespace() || c == ';')
        .next()
        .unwrap_or("");
    let max_width = max_width.max(20);
    let rendered = match keyword {
        "graph" | "flowchart" | "flowchart-elk" => {
            layout::render(&diagrams::flowchart(header, body)?, max_width)
        }
        "stateDiagram" | "stateDiagram-v2" => layout::render(&diagrams::state(body)?, max_width),
        "classDiagram" | "classDiagram-v2" => layout::render(&diagrams::class(body)?, max_width),
        "erDiagram" => layout::render(&diagrams::entity_relationship(body)?, max_width),
        "sequenceDiagram" => sequence::render(body),
        "pie" => charts::pie(header, body, max_width),
        "mindmap" => charts::mindmap(body),
        _ => None,
    }?;
    (!rendered.is_empty()).then_some(rendered)
}

/// Source lines without front matter, comments, directives and blank lines.
fn source_lines(source: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = source.lines().map(str::trim_end).collect();
    if let Some(first) = lines.iter().position(|line| !line.trim().is_empty()) {
        if lines[first].trim() == "---" {
            if let Some(end) = lines[first + 1..]
                .iter()
                .position(|line| line.trim() == "---")
            {
                lines.drain(..first + end + 2);
            }
        }
    }
    lines.retain(|line| {
        let line = line.trim();
        !line.is_empty() && !line.starts_with("%%")
    });
    lines
}

fn width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Node and edge text: removes quotes and Markdown string markers, turns `<br>` into line
/// breaks and decodes entities.
fn clean_text(text: &str) -> Vec<String> {
    let mut text = text.trim();
    if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
        text = &text[1..text.len() - 1];
    }
    let markdown = text.len() >= 2 && text.starts_with('`') && text.ends_with('`');
    let mut text = if markdown {
        text[1..text.len() - 1].replace("**", "").replace('*', "")
    } else {
        text.to_string()
    };
    for br in ["<br/>", "<br />", "<br>", "<BR>", "<BR/>", "<br/ >"] {
        text = text.replace(br, "\n");
    }
    // Font Awesome icons (`fa:fa-car`) cannot be drawn.
    let icon = |word: &&str| {
        word.split_once(':').is_some_and(|(prefix, _)| {
            matches!(prefix, "fa" | "fab" | "fas" | "far" | "fal" | "fak")
        })
    };
    decode_entities(&text)
        .split('\n')
        .map(|line| {
            line.split_whitespace()
                .filter(|word| !icon(word))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|line| !line.is_empty())
        .collect()
}

fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(['&', '#']) {
        out.push_str(&rest[..start]);
        let candidate = &rest[start..];
        let decoded = candidate
            .find(';')
            .filter(|&end| end <= 10)
            .and_then(|end| {
                let name = candidate[1..end].trim_start_matches('#');
                let ch = match name {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    "nbsp" => Some(' '),
                    _ => name.parse::<u32>().ok().and_then(char::from_u32),
                }?;
                Some((ch, end + 1))
            });
        match decoded {
            Some((ch, len)) => {
                out.push(ch);
                rest = &candidate[len..];
            }
            None => {
                out.push_str(&candidate[..1]);
                rest = &candidate[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Word wrap `text` to `limit` columns, breaking words that do not fit.
fn wrap(text: &str, limit: usize) -> Vec<String> {
    let limit = limit.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && width(&line) + 1 + width(word) > limit {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        for ch in word.chars() {
            if !line.is_empty() && width(&line) + ch.width().unwrap_or(0) > limit {
                lines.push(std::mem::take(&mut line));
            }
            line.push(ch);
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// Clip a line to `limit` columns.
pub fn clip<T: Copy>(line: Vec<(String, T)>, limit: usize) -> Vec<(String, T)> {
    let mut used = 0;
    let mut out = Vec::with_capacity(line.len());
    for (text, role) in line {
        if used >= limit {
            break;
        }
        let mut clipped = String::new();
        for ch in text.chars() {
            let ch_width = ch.width().unwrap_or(0);
            if used + ch_width > limit {
                used = limit;
                break;
            }
            used += ch_width;
            clipped.push(ch);
        }
        out.push((clipped, role));
    }
    out
}

// --- Canvas -------------------------------------------------------------------------------

const UP: u8 = 1;
const DOWN: u8 = 2;
const LEFT: u8 = 4;
const RIGHT: u8 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stroke {
    Solid,
    Dotted,
    Thick,
}

/// Marks the second column of a wide character.
const WIDE: char = '\0';

#[derive(Clone, Copy)]
struct Cell {
    ch: char,
    role: Role,
    /// Line connections (`UP | DOWN | LEFT | RIGHT`), drawn when `ch` is blank.
    mask: u8,
    stroke: Stroke,
}

const BLANK: Cell = Cell {
    ch: ' ',
    role: Role::Text,
    mask: 0,
    stroke: Stroke::Solid,
};

/// A grid of characters. Lines are recorded as connection masks so crossings and joins get
/// the right box drawing characters.
struct Canvas {
    width: usize,
    height: usize,
    cells: Vec<Cell>,
}

impl Canvas {
    fn new(width: usize, height: usize) -> Self {
        Canvas {
            width,
            height,
            cells: vec![BLANK; width * height],
        }
    }

    fn cell(&mut self, row: usize, col: usize) -> Option<&mut Cell> {
        (row < self.height && col < self.width).then(|| &mut self.cells[row * self.width + col])
    }

    fn is_blank(&self, row: usize, col: usize) -> bool {
        row < self.height && col < self.width && {
            let cell = &self.cells[row * self.width + col];
            cell.ch == ' ' && cell.mask == 0
        }
    }

    /// The connections of a line cell, or 0 for anything else.
    fn mask(&self, row: usize, col: usize) -> u8 {
        if row < self.height && col < self.width {
            let cell = &self.cells[row * self.width + col];
            if cell.ch == ' ' {
                return cell.mask;
            }
        }
        0
    }

    fn put(&mut self, row: usize, col: usize, ch: char, role: Role) {
        if let Some(cell) = self.cell(row, col) {
            *cell = Cell {
                ch,
                role,
                mask: 0,
                stroke: Stroke::Solid,
            };
        }
    }

    /// Write text, returning the column after it.
    fn text(&mut self, row: usize, mut col: usize, text: &str, role: Role) -> usize {
        for ch in text.chars() {
            let ch_width = ch.width().unwrap_or(0);
            if ch_width == 0 {
                continue;
            }
            if col + ch_width > self.width {
                break;
            }
            self.put(row, col, ch, role);
            if ch_width == 2 {
                self.put(row, col + 1, WIDE, role);
            }
            col += ch_width;
        }
        col
    }

    /// Write text into blank cells only, ending with `…` when it does not fit.
    fn text_in_blank(&mut self, row: usize, col: usize, text: &str, role: Role) {
        let room = (col..self.width)
            .take_while(|&c| self.is_blank(row, c))
            .count();
        if room == 0 {
            return;
        }
        if width(text) <= room {
            self.text(row, col, text, role);
        } else if room > 1 {
            let mut clipped: String = clip(vec![(text.to_string(), ())], room - 1)
                .into_iter()
                .map(|(text, _)| text)
                .collect();
            clipped.push('…');
            self.text(row, col, &clipped, role);
        }
    }

    fn link(&mut self, row: usize, col: usize, bits: u8, stroke: Stroke) {
        if let Some(cell) = self.cell(row, col) {
            if cell.mask == 0 {
                cell.stroke = stroke;
            }
            cell.mask |= bits;
        }
    }

    /// Draw a horizontal or vertical line between two cells.
    fn segment(&mut self, from: (usize, usize), to: (usize, usize), stroke: Stroke) {
        let ((r0, c0), (r1, c1)) = (from, to);
        if r0 == r1 {
            let (lo, hi) = (c0.min(c1), c0.max(c1));
            for col in lo..=hi {
                let bits = if col > lo { LEFT } else { 0 } | if col < hi { RIGHT } else { 0 };
                self.link(r0, col, bits, stroke);
            }
        } else if c0 == c1 {
            let (lo, hi) = (r0.min(r1), r0.max(r1));
            for row in lo..=hi {
                let bits = if row > lo { UP } else { 0 } | if row < hi { DOWN } else { 0 };
                self.link(row, c0, bits, stroke);
            }
        }
    }

    /// Turn a box border cell into a junction with a line leaving in `direction`.
    fn attach(&mut self, row: usize, col: usize, direction: u8) {
        if let Some(cell) = self.cell(row, col) {
            let mask = match cell.ch {
                '─' => LEFT | RIGHT,
                '│' => UP | DOWN,
                _ => return,
            };
            cell.ch = line_char(mask | direction, Stroke::Solid);
        }
    }

    fn into_lines(self) -> Vec<Line> {
        let mut lines: Vec<Line> = Vec::with_capacity(self.height);
        for row in self.cells.chunks(self.width.max(1)).take(self.height) {
            let mut line: Line = Vec::new();
            for cell in row {
                let (ch, role) = match cell.ch {
                    WIDE => continue,
                    ' ' if cell.mask != 0 => (line_char(cell.mask, cell.stroke), Role::Edge),
                    ' ' => (' ', Role::Text),
                    ch => (ch, cell.role),
                };
                match line.last_mut() {
                    Some((text, last)) if *last == role || ch == ' ' => text.push(ch),
                    _ => line.push((ch.to_string(), role)),
                }
            }
            while line
                .last()
                .is_some_and(|(text, _)| text.trim_end().is_empty())
            {
                line.pop();
            }
            if let Some((text, _)) = line.last_mut() {
                text.truncate(text.trim_end().len());
            }
            lines.push(line);
        }
        while lines.last().is_some_and(Vec::is_empty) {
            lines.pop();
        }
        let leading = lines.iter().take_while(|line| line.is_empty()).count();
        lines.drain(..leading);
        lines
    }
}

fn line_char(mask: u8, stroke: Stroke) -> char {
    if mask & (LEFT | RIGHT) == 0 {
        return match stroke {
            Stroke::Solid => '│',
            Stroke::Dotted => '┆',
            Stroke::Thick => '┃',
        };
    }
    if mask & (UP | DOWN) == 0 {
        return match stroke {
            Stroke::Solid => '─',
            Stroke::Dotted => '┄',
            Stroke::Thick => '━',
        };
    }
    match mask {
        m if m == DOWN | RIGHT => '╭',
        m if m == DOWN | LEFT => '╮',
        m if m == UP | RIGHT => '╰',
        m if m == UP | LEFT => '╯',
        m if m == UP | DOWN | RIGHT => '├',
        m if m == UP | DOWN | LEFT => '┤',
        m if m == LEFT | RIGHT | DOWN => '┬',
        m if m == LEFT | RIGHT | UP => '┴',
        _ => '┼',
    }
}

// --- Boxes --------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Rect,
    Round,
    Diamond,
    /// A single character without a border, such as a state diagram's start point.
    Dot(char),
}

/// A node: text sections separated by rules, inside a border.
#[derive(Debug, Clone)]
struct NodeBox {
    sections: Vec<Vec<String>>,
    shape: Shape,
    /// Draw the first section bold and the others left aligned, as in class diagrams.
    titled: bool,
}

impl NodeBox {
    fn new(text: Vec<String>, shape: Shape) -> Self {
        NodeBox {
            sections: vec![text],
            shape,
            titled: false,
        }
    }

    fn wrapped(&self, limit: usize) -> NodeBox {
        let sections = self
            .sections
            .iter()
            .map(|section| section.iter().flat_map(|line| wrap(line, limit)).collect())
            .collect();
        NodeBox {
            sections,
            ..self.clone()
        }
    }

    /// Width and height including the border.
    fn size(&self) -> (usize, usize) {
        if let Shape::Dot(_) = self.shape {
            return (1, 1);
        }
        let inner = self
            .sections
            .iter()
            .flatten()
            .map(|line| width(line))
            .max()
            .unwrap_or(0);
        let lines: usize = self
            .sections
            .iter()
            .map(|section| section.len().max(1))
            .sum();
        (
            inner.max(1) + 4,
            lines + self.sections.len().saturating_sub(1) + 2,
        )
    }

    fn draw(&self, canvas: &mut Canvas, row: usize, col: usize) {
        let (corners, border) = match self.shape {
            Shape::Dot(ch) => {
                canvas.put(row, col, ch, Role::Border);
                return;
            }
            Shape::Rect => (['┌', '┐', '└', '┘'], Role::Border),
            Shape::Round => (['╭', '╮', '╰', '╯'], Role::Border),
            Shape::Diamond => (['╱', '╲', '╲', '╱'], Role::Border),
        };
        let (w, h) = self.size();
        let rule = |canvas: &mut Canvas, row: usize, left: char, right: char| {
            canvas.put(row, col, left, border);
            for c in col + 1..col + w - 1 {
                canvas.put(row, c, '─', border);
            }
            canvas.put(row, col + w - 1, right, border);
        };
        rule(canvas, row, corners[0], corners[1]);
        rule(canvas, row + h - 1, corners[2], corners[3]);
        let mut r = row + 1;
        for (index, section) in self.sections.iter().enumerate() {
            if index > 0 {
                rule(canvas, r, '├', '┤');
                r += 1;
            }
            let empty = [String::new()];
            let lines = if section.is_empty() {
                &empty[..]
            } else {
                section
            };
            for line in lines {
                canvas.put(r, col, '│', border);
                canvas.put(r, col + w - 1, '│', border);
                let start = if index == 0 || !self.titled {
                    col + 2 + (w - 4 - width(line)) / 2
                } else {
                    col + 2
                };
                let role = if index == 0 && self.titled {
                    Role::Title
                } else {
                    Role::Text
                };
                canvas.text(r, start, line, role);
                r += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render to plain text for assertions.
    pub(super) fn plain(source: &str, max_width: usize) -> String {
        render(source, max_width)
            .expect("diagram renders")
            .into_iter()
            .map(|line| line.into_iter().map(|(text, _)| text).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn cleans_text() {
        assert_eq!(clean_text("\"a <br> b\""), ["a", "b"]);
        assert_eq!(clean_text("`**bold** text`"), ["bold text"]);
        assert_eq!(clean_text("a &amp; b #quot;c#quot;"), ["a & b \"c\""]);
        assert_eq!(clean_text("#35; and &#38;"), ["# and &"]);
        assert_eq!(clean_text("fa:fa-car Car"), ["Car"]);
    }

    #[test]
    fn wraps_text() {
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("abcdefgh", 3), ["abc", "def", "gh"]);
    }

    #[test]
    fn unsupported_and_invalid_sources_fall_back() {
        assert!(render("gantt\n  title A", 80).is_none());
        assert!(render("not a diagram", 80).is_none());
        assert!(render("", 80).is_none());
        assert!(render("pie\n", 80).is_none());
    }

    /// Random token soup must never panic.
    #[test]
    fn survives_garbage() {
        const HEADERS: &[&str] = &[
            "graph TD",
            "graph LR",
            "flowchart BT",
            "flowchart RL",
            "stateDiagram-v2",
            "classDiagram",
            "erDiagram",
            "sequenceDiagram",
            "pie showData",
            "mindmap",
        ];
        const TOKENS: &[&str] = &[
            "A",
            "B",
            "c-d",
            "ü",
            "日本",
            "😀",
            " ",
            "  ",
            "\n",
            "\t",
            "-->",
            "---",
            "-.->",
            "==>",
            "<-->",
            "--o",
            "--x",
            "|",
            "|x|",
            "-- t -->",
            "[",
            "]",
            "(",
            ")",
            "{",
            "}",
            "((",
            "))",
            ">",
            "\"",
            ":",
            ";",
            "&",
            ":::",
            "@{",
            "[*]",
            "<|--",
            "..>",
            "*--",
            "o--",
            "||--o{",
            "->>",
            "-->>",
            "-x",
            "-)",
            "<<->>",
            "+",
            "loop",
            "alt",
            "else",
            "end",
            "opt",
            "Note",
            "note right of",
            "over",
            ",",
            "participant",
            "as",
            "state",
            "class",
            "<<",
            ">>",
            "~",
            "subgraph",
            "direction",
            "title",
            "1",
            "2.5",
            "-3",
            "%%",
            "---",
            "<br>",
            "&amp;",
            "#35;",
        ];
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = |bound: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % bound as u64) as usize
        };
        for _ in 0..3000 {
            let mut source = HEADERS[next(HEADERS.len())].to_string();
            source.push('\n');
            for _ in 0..next(60) {
                source.push_str(TOKENS[next(TOKENS.len())]);
            }
            for width in [20, 80] {
                if let Some(lines) = render(&source, width) {
                    assert!(lines.len() < 2000, "{source}");
                }
            }
        }
    }

    #[test]
    fn large_graphs_stay_fast() {
        let mut source = String::from("graph TD\n");
        for i in 0..59 {
            source.push_str(&format!("n{i} --> n{}\n", i + 1));
            source.push_str(&format!("n0 --> n{}\n", i + 1));
        }
        let start = std::time::Instant::now();
        // Close to the vertex limit, so it is laid out.
        assert!(render(&source, 100).is_some());
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn skips_front_matter_and_comments() {
        let text = plain("---\ntitle: x\n---\n%% comment\ngraph TD\nA-->B", 80);
        assert!(text.contains("│ A │"), "{text}");
    }
}

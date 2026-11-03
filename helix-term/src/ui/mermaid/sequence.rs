//! Sequence diagrams: participants in columns with lifelines, messages as arrows between them,
//! notes, and `loop`/`alt`/`opt`/`par`/`critical`/`break` blocks as frames.

use super::{clean_text, width, Canvas, Line, NodeBox, Role, Shape, DOWN};

struct Participant {
    id: String,
    label: String,
    actor: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    None,
    Arrow,
    Cross,
    Async,
}

enum NotePlace {
    Left,
    Right,
    Over,
}

enum Item {
    Message {
        from: usize,
        to: usize,
        text: String,
        dotted: bool,
        end: End,
        both: bool,
    },
    Note {
        place: NotePlace,
        first: usize,
        last: usize,
        text: String,
    },
    Open(String),
    Divider(String),
    Close,
}

/// Arrows, longest first: (syntax, dotted, end, both ends).
const ARROWS: &[(&str, bool, End, bool)] = &[
    ("<<-->>", true, End::Arrow, true),
    ("<<->>", false, End::Arrow, true),
    ("-->>", true, End::Arrow, false),
    ("->>", false, End::Arrow, false),
    ("--x", true, End::Cross, false),
    ("-x", false, End::Cross, false),
    ("--)", true, End::Async, false),
    ("-)", false, End::Async, false),
    ("-->", true, End::None, false),
    ("->", false, End::None, false),
];

#[derive(Default)]
struct Parser {
    participants: Vec<Participant>,
    items: Vec<Item>,
    title: Option<String>,
    autonumber: bool,
    /// Open blocks; `false` for invisible ones (`rect`, `box`).
    blocks: Vec<bool>,
}

impl Parser {
    fn participant(&mut self, id: &str) -> usize {
        let id = id.trim();
        if let Some(index) = self.participants.iter().position(|p| p.id == id) {
            return index;
        }
        self.participants.push(Participant {
            id: id.to_string(),
            label: id.to_string(),
            actor: false,
        });
        self.participants.len() - 1
    }

    fn line(&mut self, line: &str) {
        let line = line.trim();
        let line = line.strip_prefix("create ").unwrap_or(line);
        let (keyword, rest) = match line.split_once(char::is_whitespace) {
            Some((keyword, rest)) => (keyword, rest.trim()),
            None => (line, ""),
        };
        match keyword {
            "participant" | "actor" => {
                let (id, label) = match rest.split_once(" as ") {
                    Some((id, label)) => (id.trim(), Some(label)),
                    None => (rest, None),
                };
                let id = id.split("@{").next().unwrap_or(id);
                let index = self.participant(id);
                if let Some(label) = label {
                    self.participants[index].label = clean_text(label).join(" ");
                }
                self.participants[index].actor = keyword == "actor";
            }
            "autonumber" => self.autonumber = rest != "off",
            "title" => self.title = Some(clean_text(rest).join(" ")),
            "loop" | "alt" | "opt" | "par" | "critical" | "break" => {
                self.blocks.push(true);
                self.items.push(Item::Open(block_label(keyword, rest)));
            }
            "rect" | "box" => self.blocks.push(false),
            "else" | "and" | "option" => self.items.push(Item::Divider(block_label(keyword, rest))),
            "end" => {
                if self.blocks.pop() == Some(true) {
                    self.items.push(Item::Close);
                }
            }
            "activate" | "deactivate" | "destroy" | "link" | "links" | "properties" | "details" => {
            }
            _ if keyword.eq_ignore_ascii_case("note") => self.note(rest),
            _ if keyword.starts_with("accTitle") || keyword.starts_with("accDescr") => {}
            _ => self.message(line),
        }
    }

    fn note(&mut self, rest: &str) {
        let Some((target, text)) = rest.split_once(':') else {
            return;
        };
        let target = target.trim();
        let lower = target.to_ascii_lowercase();
        let (place, names) = if lower.starts_with("left of") {
            (NotePlace::Left, &target[7..])
        } else if lower.starts_with("right of") {
            (NotePlace::Right, &target[8..])
        } else if lower.starts_with("over") {
            (NotePlace::Over, &target[4..])
        } else {
            return;
        };
        let indices: Vec<usize> = names
            .split(',')
            .filter(|name| !name.trim().is_empty())
            .map(|name| self.participant(name))
            .collect();
        let (Some(&first), Some(&last)) = (indices.iter().min(), indices.iter().max()) else {
            return;
        };
        self.items.push(Item::Note {
            place,
            first,
            last,
            text: clean_text(text).join(" "),
        });
    }

    fn message(&mut self, line: &str) {
        let found = line.char_indices().skip(1).find_map(|(i, _)| {
            ARROWS
                .iter()
                .find(|arrow| line[i..].starts_with(arrow.0))
                .map(|arrow| (i, arrow))
        });
        let Some((at, &(syntax, dotted, end, both))) = found else {
            return;
        };
        let from = line[..at].trim();
        let rest = &line[at + syntax.len()..];
        let (to, text) = match rest.split_once(':') {
            Some((to, text)) => (to, clean_text(text).join(" ")),
            None => (rest, String::new()),
        };
        // Activation shortcuts: A->>+B
        let to = to.trim().trim_start_matches(['+', '-']).trim();
        if from.is_empty() || to.is_empty() {
            return;
        }
        let from = self.participant(from);
        let to = self.participant(to);
        self.items.push(Item::Message {
            from,
            to,
            text,
            dotted,
            end,
            both,
        });
    }
}

fn block_label(keyword: &str, rest: &str) -> String {
    let text = clean_text(rest).join(" ");
    if text.is_empty() {
        format!("[{keyword}]")
    } else {
        format!("[{keyword}] {text}")
    }
}

pub(super) fn render(body: &[&str]) -> Option<Vec<Line>> {
    let mut parser = Parser::default();
    for line in body {
        parser.line(line);
    }
    for open in std::mem::take(&mut parser.blocks) {
        if open {
            parser.items.push(Item::Close);
        }
    }
    let Parser {
        participants,
        mut items,
        title,
        autonumber,
        ..
    } = parser;
    if participants.is_empty() {
        return None;
    }
    if autonumber {
        let mut number = 0;
        for item in &mut items {
            if let Item::Message { text, .. } = item {
                number += 1;
                *text = if text.is_empty() {
                    number.to_string()
                } else {
                    format!("{number}. {text}")
                };
            }
        }
    }

    let count = participants.len();
    let boxes: Vec<NodeBox> = participants
        .iter()
        .map(|p| {
            let mut node = NodeBox::new(
                vec![p.label.clone()],
                if p.actor { Shape::Round } else { Shape::Rect },
            );
            node.titled = true;
            node
        })
        .collect();
    let box_width: Vec<i64> = boxes.iter().map(|b| b.size().0 as i64).collect();

    // Minimum distances between lifelines.
    let mut need = vec![vec![0i64; count]; count];
    let mut require = |a: usize, b: usize, distance: i64| {
        let (a, b) = (a.min(b), a.max(b));
        if a != b {
            need[a][b] = need[a][b].max(distance);
        }
    };
    for i in 0..count.saturating_sub(1) {
        require(
            i,
            i + 1,
            (box_width[i] - box_width[i] / 2) + box_width[i + 1] / 2 + 2,
        );
    }
    for item in &items {
        match item {
            Item::Message { from, to, text, .. } if from != to => {
                require(*from, *to, width(text) as i64 + 4);
            }
            Item::Message { from, text, .. } if from + 1 < count => {
                require(*from, from + 1, width(text) as i64 + 7);
            }
            Item::Note {
                place: NotePlace::Right,
                first,
                text,
                ..
            } if first + 1 < count => require(*first, first + 1, width(text) as i64 + 8),
            Item::Note {
                place: NotePlace::Left,
                first,
                text,
                ..
            } if *first > 0 => require(first - 1, *first, width(text) as i64 + 8),
            _ => {}
        }
    }
    let mut center = vec![0i64; count];
    for j in 1..count {
        center[j] = (0..j).map(|i| center[i] + need[i][j]).max().unwrap_or(0);
    }

    // Horizontal extents of everything drawn, to find the margins.
    let mut lo = 0i64;
    let mut hi = 0i64;
    let mut extend = |a: i64, b: i64| {
        lo = lo.min(a);
        hi = hi.max(b);
    };
    for i in 0..count {
        let left = center[i] - box_width[i] / 2;
        extend(left, left + box_width[i] - 1);
    }
    let note_span = |place: &NotePlace, first: usize, last: usize, text: &str| -> (i64, i64) {
        let note_width = width(text) as i64 + 4;
        match place {
            NotePlace::Right => (center[first] + 2, center[first] + 1 + note_width),
            NotePlace::Left => (center[first] - 1 - note_width, center[first] - 2),
            NotePlace::Over => {
                let span = center[last] - center[first] + 5;
                let note_width = note_width.max(span);
                let left = (center[first] + center[last]) / 2 - note_width / 2;
                (left, left + note_width - 1)
            }
        }
    };
    let mut depth = 0usize;
    let mut max_depth = 0usize;
    let mut widest_label = 0;
    for item in &items {
        match item {
            Item::Message { from, to, text, .. } if from == to => {
                extend(center[*from], center[*from] + 5 + width(text) as i64);
            }
            Item::Note {
                place,
                first,
                last,
                text,
            } => {
                let (a, b) = note_span(place, *first, *last, text);
                extend(a, b);
            }
            Item::Open(label) | Item::Divider(label) => {
                if matches!(item, Item::Open(_)) {
                    depth += 1;
                    max_depth = max_depth.max(depth);
                }
                widest_label = widest_label.max(width(label) as i64 + 4);
            }
            Item::Close => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    hi = hi.max(lo + widest_label);
    let margin = max_depth as i64 * 2;
    let offset = margin - lo;
    let total_width = (hi - lo + 1 + 2 * margin) as usize;
    let x = |value: i64| (value + offset) as usize;

    let rows: usize = 3
        + 1
        + items
            .iter()
            .map(|item| match item {
                Item::Message { .. } => 2,
                Item::Note { .. } => 3,
                _ => 1,
            })
            .sum::<usize>();
    let mut canvas = Canvas::new(total_width, rows);

    for i in 0..count {
        let col = x(center[i] - box_width[i] / 2);
        boxes[i].draw(&mut canvas, 0, col);
        canvas.attach(2, x(center[i]), DOWN);
        for row in 3..rows {
            canvas.put(row, x(center[i]), '┆', Role::Muted);
        }
    }

    let mut row = 3;
    // Open frames: (left, right, first row).
    let mut frames: Vec<(usize, usize, usize)> = Vec::new();
    for item in &items {
        match item {
            Item::Message {
                from,
                to,
                text,
                dotted,
                end,
                both,
            } => {
                let line_char = if *dotted { '┄' } else { '─' };
                if from == to {
                    let c = x(center[*from]);
                    canvas.put(row, c + 1, line_char, Role::Edge);
                    canvas.put(row, c + 2, line_char, Role::Edge);
                    canvas.put(row, c + 3, '╮', Role::Edge);
                    canvas.text(row, c + 5, text, Role::Text);
                    canvas.put(
                        row + 1,
                        c + 1,
                        glyph(*end, false).unwrap_or(line_char),
                        Role::Arrow,
                    );
                    canvas.put(row + 1, c + 2, line_char, Role::Edge);
                    canvas.put(row + 1, c + 3, '╯', Role::Edge);
                } else {
                    let (a, b) = (x(center[*from]), x(center[*to]));
                    let (left, right) = (a.min(b), a.max(b));
                    let inner = right - left - 1;
                    let text_width = width(text).min(inner);
                    canvas.text(row, left + 1 + (inner - text_width) / 2, text, Role::Text);
                    for col in left + 1..right {
                        canvas.put(row + 1, col, line_char, Role::Edge);
                    }
                    let rightward = b > a;
                    if let Some(head) = glyph(*end, rightward) {
                        let col = if rightward { right - 1 } else { left + 1 };
                        canvas.put(row + 1, col, head, Role::Arrow);
                    }
                    if *both {
                        if let Some(head) = glyph(*end, !rightward) {
                            let col = if rightward { left + 1 } else { right - 1 };
                            canvas.put(row + 1, col, head, Role::Arrow);
                        }
                    }
                }
                row += 2;
            }
            Item::Note {
                place,
                first,
                last,
                text,
            } => {
                let (a, b) = note_span(place, *first, *last, text);
                let note_width = (b - a + 1) as usize;
                let mut note = NodeBox::new(vec![text.clone()], Shape::Rect);
                // Widen the text so the box spans the participants.
                let padding = note_width.saturating_sub(width(text) + 4);
                note.sections[0][0] = format!(
                    "{}{text}{}",
                    " ".repeat(padding / 2),
                    " ".repeat(padding - padding / 2)
                );
                note.draw(&mut canvas, row, x(a));
                for col in x(a)..=x(b) {
                    for r in row..row + 3 {
                        if let Some(cell) = canvas.cell(r, col) {
                            if cell.role == Role::Border {
                                cell.role = Role::Muted;
                            } else if cell.ch != ' ' {
                                cell.role = Role::Label;
                            }
                        }
                    }
                }
                row += 3;
            }
            Item::Open(label) => {
                let level = frames.len();
                let (left, right) = (level * 2, total_width - 1 - level * 2);
                frame_rule(&mut canvas, row, left, right, '┌', '┐', '─');
                canvas.text(row, left + 2, &format!(" {label} "), Role::Label);
                frames.push((left, right, row));
                row += 1;
            }
            Item::Divider(label) => {
                if let Some(&(left, right, _)) = frames.last() {
                    frame_rule(&mut canvas, row, left, right, '├', '┤', '┄');
                    canvas.text(row, left + 2, &format!(" {label} "), Role::Label);
                }
                row += 1;
            }
            Item::Close => {
                if let Some((left, right, first)) = frames.pop() {
                    frame_rule(&mut canvas, row, left, right, '└', '┘', '─');
                    for r in first + 1..row {
                        for col in [left, right] {
                            if canvas.is_blank(r, col) {
                                canvas.put(r, col, '│', Role::Muted);
                            }
                        }
                    }
                }
                row += 1;
            }
        }
    }

    let mut lines = Vec::new();
    if let Some(title) = title {
        lines.push(vec![(title, Role::Title)]);
        lines.push(Vec::new());
    }
    lines.extend(canvas.into_lines());
    Some(lines)
}

fn frame_rule(
    canvas: &mut Canvas,
    row: usize,
    left: usize,
    right: usize,
    first: char,
    last: char,
    fill: char,
) {
    canvas.put(row, left, first, Role::Muted);
    for col in left + 1..right {
        canvas.put(row, col, fill, Role::Muted);
    }
    canvas.put(row, right, last, Role::Muted);
}

fn glyph(end: End, rightward: bool) -> Option<char> {
    match end {
        End::None => None,
        End::Arrow => Some(if rightward { '►' } else { '◄' }),
        End::Cross => Some('×'),
        End::Async => Some(if rightward { '>' } else { '<' }),
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::plain;

    #[test]
    fn renders_messages() {
        let text = plain(
            "sequenceDiagram\nparticipant A as Alice\nA->>Bob: Hello\nBob-->>A: Hi\nBob->>Bob: think",
            80,
        );
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[1].contains("│ Alice │"), "{text}");
        assert!(lines[1].contains("│ Bob │"), "{text}");
        assert!(text.contains("Hello"), "{text}");
        assert!(text.contains("─►"), "{text}");
        assert!(text.contains("◄┄"), "{text}");
        assert!(text.contains("╮ think"), "{text}");
    }

    #[test]
    fn renders_blocks_and_notes() {
        let text = plain(
            "sequenceDiagram\nautonumber\nloop Every minute\nA->>B: ping\nalt ok\nB->>A: pong\nelse failed\nB-xA: error\nend\nend\nNote over A,B: done",
            80,
        );
        assert!(text.contains("[loop] Every minute"), "{text}");
        assert!(text.contains("[else] failed"), "{text}");
        assert!(text.contains("1. ping"), "{text}");
        assert!(text.contains("3. error"), "{text}");
        assert!(text.contains('×'), "{text}");
        assert!(text.contains("done"), "{text}");
        assert!(text.contains('└'), "{text}");
    }
}

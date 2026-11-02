//! Parsers for the diagram types drawn with the graph layout: flowcharts, state, class and
//! entity relationship diagrams. Unknown statements are skipped.

use std::collections::HashMap;

use super::layout::{Direction, Edge, Graph, Head};
use super::{clean_text, Shape, Stroke};

// --- Flowcharts ---------------------------------------------------------------------------

pub(super) fn flowchart(header: &str, body: &[&str]) -> Option<Graph> {
    let mut statements = split_statements(header).into_iter();
    let first = statements.next()?;
    let direction = first
        .split_whitespace()
        .nth(1)
        .and_then(Direction::parse)
        .unwrap_or(Direction::Down);
    let mut parser = Flowchart {
        graph: Graph::new(direction),
        subgraphs: HashMap::new(),
    };
    for statement in statements {
        parser.statement(statement);
    }
    for line in body {
        for statement in split_statements(line) {
            parser.statement(statement);
        }
    }
    Some(parser.graph)
}

/// Split a line at `;` outside of quotes.
fn split_statements(line: &str) -> Vec<&str> {
    let mut statements = Vec::new();
    let mut quoted = false;
    let mut start = 0;
    for (i, ch) in line.char_indices() {
        match ch {
            '"' => quoted = !quoted,
            ';' if !quoted => {
                statements.push(&line[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    statements.push(&line[start..]);
    statements.retain(|s| !s.trim().is_empty());
    statements
}

struct Flowchart {
    graph: Graph,
    /// Titles of subgraphs, shown when edges connect to a subgraph.
    subgraphs: HashMap<String, String>,
}

struct Cursor<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn rest(&self) -> &'a str {
        &self.text[self.pos..]
    }

    fn skip_whitespace(&mut self) {
        let rest = self.rest();
        self.pos += rest.len() - rest.trim_start().len();
    }

    fn eat(&mut self, prefix: &str) -> bool {
        let matched = self.rest().starts_with(prefix);
        if matched {
            self.pos += prefix.len();
        }
        matched
    }

    fn at_end(&self) -> bool {
        self.rest().trim().is_empty()
    }
}

struct LinkSpec {
    label: String,
    stroke: Stroke,
    head: Head,
    tail: Head,
    minlen: usize,
}

impl Flowchart {
    fn statement(&mut self, statement: &str) {
        let statement = statement.trim();
        let keyword = statement.split_whitespace().next().unwrap_or("");
        match keyword {
            "subgraph" => {
                self.subgraph(statement["subgraph".len()..].trim());
                return;
            }
            "end" | "direction" | "classDef" | "class" | "style" | "linkStyle" | "click" => return,
            _ if keyword.starts_with("accTitle") || keyword.starts_with("accDescr") => return,
            _ => {}
        }
        let mut cursor = Cursor {
            text: statement,
            pos: 0,
        };
        let Some(mut left) = self.group(&mut cursor) else {
            return;
        };
        while !cursor.at_end() {
            let Some(link) = link(&mut cursor) else {
                return;
            };
            let Some(right) = self.group(&mut cursor) else {
                return;
            };
            for &from in &left {
                for &to in &right {
                    self.graph.edges.push(Edge {
                        label: link.label.clone(),
                        stroke: link.stroke,
                        head: link.head,
                        tail: link.tail,
                        minlen: link.minlen,
                        ..Edge::new(from, to)
                    });
                }
            }
            left = right;
        }
    }

    fn subgraph(&mut self, rest: &str) {
        let (id, title) = match rest.find('[') {
            Some(open) => {
                let title = rest[open + 1..].trim_end().trim_end_matches(']');
                (rest[..open].trim(), clean_text(title).join(" "))
            }
            None => {
                let title = clean_text(rest).join(" ");
                (rest.trim_matches('"'), title)
            }
        };
        if !id.is_empty() {
            self.subgraphs.insert(id.to_string(), title);
        }
    }

    /// Nodes joined with `&`.
    fn group(&mut self, cursor: &mut Cursor) -> Option<Vec<usize>> {
        let mut nodes = Vec::new();
        loop {
            cursor.skip_whitespace();
            nodes.push(self.node(cursor)?);
            cursor.skip_whitespace();
            if !cursor.eat("&") {
                return Some(nodes);
            }
        }
    }

    fn node(&mut self, cursor: &mut Cursor) -> Option<usize> {
        let id = take_id(cursor);
        if id.is_empty() {
            return None;
        }
        let shape = shape(cursor);
        if cursor.eat(":::") {
            take_id(cursor);
        }
        let exists = self.graph.ids.contains_key(id);
        let index = self.graph.node(id);
        if !exists {
            if let Some(title) = self.subgraphs.get(id) {
                self.graph.nodes[index].sections = vec![vec![title.clone()]];
            }
        }
        if let Some((shape, text)) = shape {
            let node = &mut self.graph.nodes[index];
            node.shape = shape;
            node.sections = vec![clean_text(&text)];
        }
        Some(index)
    }
}

/// A node id: letters, digits, `_`, and `-` between them.
fn take_id<'a>(cursor: &mut Cursor<'a>) -> &'a str {
    let rest = cursor.rest();
    let mut end = 0;
    let mut chars = rest.char_indices().peekable();
    while let Some((i, ch)) = chars.next() {
        let continues = ch.is_alphanumeric()
            || ch == '_'
            || (ch == '-'
                && end > 0
                && chars
                    .peek()
                    .is_some_and(|&(_, next)| next.is_alphanumeric()));
        if !continues {
            break;
        }
        end = i + ch.len_utf8();
    }
    cursor.pos += end;
    &rest[..end]
}

/// Node shapes: opening delimiter, possible closing delimiters, shape.
const SHAPES: &[(&str, &[&str], Shape)] = &[
    ("(((", &[")))"], Shape::Round),
    ("((", &["))"], Shape::Round),
    ("([", &["])"], Shape::Round),
    ("[(", &[")]"], Shape::Round),
    ("[[", &["]]"], Shape::Rect),
    ("{{", &["}}"], Shape::Diamond),
    ("[/", &["/]", "\\]"], Shape::Rect),
    ("[\\", &["\\]", "/]"], Shape::Rect),
    ("[", &["]"], Shape::Rect),
    ("(", &[")"], Shape::Round),
    ("{", &["}"], Shape::Diamond),
    (">", &["]"], Shape::Rect),
];

fn shape(cursor: &mut Cursor) -> Option<(Shape, String)> {
    if cursor.eat("@{") {
        // A@{ shape: diamond, label: "Text" }
        let rest = cursor.rest();
        let end = rest.find('}').unwrap_or(rest.len());
        let body = &rest[..end];
        cursor.pos += (end + 1).min(rest.len());
        let field = |name: &str| {
            body.split(',').find_map(|field| {
                let (key, value) = field.split_once(':')?;
                (key.trim() == name).then(|| value.trim().to_string())
            })
        };
        let shape = match field("shape").as_deref() {
            Some("diam" | "diamond" | "decision" | "hex" | "hexagon" | "question") => {
                Shape::Diamond
            }
            Some("rounded" | "event" | "stadium" | "terminal" | "pill" | "circle" | "circ") => {
                Shape::Round
            }
            _ => Shape::Rect,
        };
        return Some((shape, field("label")?));
    }
    let rest = cursor.rest();
    let &(open, closers, shape) = SHAPES.iter().find(|(open, ..)| rest.starts_with(open))?;
    let inner = &rest[open.len()..];
    let trimmed = inner.trim_start();
    // Quoted text may contain the closing delimiter.
    if let Some(quoted) = trimmed.strip_prefix('"') {
        if let Some(end) = quoted.find('"') {
            let after = quoted[end + 1..].trim_start();
            if let Some(closer) = closers.iter().find(|closer| after.starts_with(*closer)) {
                let consumed = rest.len() - after.len() + closer.len();
                cursor.pos += consumed;
                return Some((shape, quoted[..end].to_string()));
            }
        }
    }
    let (end, closer) = closers
        .iter()
        .filter_map(|closer| inner.find(closer).map(|end| (end, closer.len())))
        .min()
        .unwrap_or((inner.len(), 0));
    cursor.pos += open.len() + end + closer;
    Some((shape, inner[..end].to_string()))
}

fn is_link_char(byte: u8) -> bool {
    matches!(byte, b'-' | b'=' | b'.')
}

/// Parse a link such as `-->`, `-.->`, `==>`, `---`, `<-->`, `--o`, `-- text -->` or
/// `-->|text|`.
fn link(cursor: &mut Cursor) -> Option<LinkSpec> {
    cursor.skip_whitespace();
    // Edge ids: A e1@--> B
    let rest = cursor.rest();
    if let Some(at) = rest.find('@') {
        let id = &rest[..at];
        if !id.is_empty()
            && id.chars().all(|c| c.is_alphanumeric() || c == '_')
            && rest[at + 1..].bytes().next().is_some_and(is_link_char)
        {
            cursor.pos += at + 1;
        }
    }
    let rest = cursor.rest();
    let bytes = rest.as_bytes();
    let (tail, mut i) = match bytes {
        [b'<', ..] => (Head::Arrow, 1),
        [b'o', next, ..] if is_link_char(*next) => (Head::Circle, 1),
        [b'x', next, ..] if is_link_char(*next) => (Head::Cross, 1),
        _ => (Head::None, 0),
    };
    let body_start = i;
    while bytes.get(i).copied().is_some_and(is_link_char) {
        i += 1;
    }
    let body = &rest[body_start..i];
    if body.len() < 2 {
        return None;
    }
    let head_at = |i: usize| -> (Head, usize) {
        let ends = |j: usize| {
            bytes
                .get(j)
                .is_none_or(|b| b.is_ascii_whitespace() || *b == b'|')
        };
        match bytes.get(i) {
            Some(b'>') => (Head::Arrow, 1),
            Some(b'o') if ends(i + 1) => (Head::Circle, 1),
            Some(b'x') if ends(i + 1) => (Head::Cross, 1),
            _ => (Head::None, 0),
        }
    };
    let (mut head, head_len) = head_at(i);
    let mut run = body.to_string();
    let mut label = String::new();
    let mut end = i + head_len;

    // Text between two halves: -- text -->, -. text .->, == text ==>
    if head == Head::None
        && body.len() == 2
        && bytes.get(i).is_some_and(|b| b.is_ascii_whitespace())
    {
        let closing = (i..bytes.len().saturating_sub(1))
            .find(|&j| is_link_char(bytes[j]) && is_link_char(bytes[j + 1]));
        if let Some(close_start) = closing {
            let mut close_end = close_start;
            while bytes.get(close_end).copied().is_some_and(is_link_char) {
                close_end += 1;
            }
            let (closing_head, closing_len) = head_at(close_end);
            label = clean_text(&rest[i..close_start]).join(" ");
            run.push_str(&rest[close_start..close_end]);
            head = closing_head;
            end = close_end + closing_len;
        }
    }
    cursor.pos += end;

    // Text after the link: -->|text|
    cursor.skip_whitespace();
    if cursor.eat("|") {
        let rest = cursor.rest();
        let close = rest.find('|').unwrap_or(rest.len());
        label = clean_text(&rest[..close]).join(" ");
        cursor.pos += (close + 1).min(rest.len());
    }

    let stroke = if run.contains('=') {
        Stroke::Thick
    } else if run.contains('.') {
        Stroke::Dotted
    } else {
        Stroke::Solid
    };
    // Longer links span more layers: ---> is one longer than -->.
    let extra = match stroke {
        Stroke::Dotted => run.matches('.').count().saturating_sub(1),
        _ if !label.is_empty() && run.len() >= 4 => run.len() - 4,
        _ if head != Head::None => run.len().saturating_sub(2),
        _ => run.len().saturating_sub(3),
    };
    Some(LinkSpec {
        label,
        stroke,
        head,
        tail,
        minlen: 1 + extra.min(4),
    })
}

// --- State diagrams -----------------------------------------------------------------------

pub(super) fn state(body: &[&str]) -> Option<Graph> {
    let mut graph = Graph::new(Direction::Down);
    let mut scopes: Vec<String> = Vec::new();
    let mut in_note = false;
    let mut start = None;
    let mut end = None;
    let mut descriptions: HashMap<usize, Vec<String>> = HashMap::new();

    let state_node = |graph: &mut Graph, id: &str| {
        let id = id.split(":::").next().unwrap_or(id).trim();
        let exists = graph.ids.contains_key(id);
        let index = graph.node(id);
        if !exists {
            graph.nodes[index].shape = Shape::Round;
        }
        index
    };

    for line in body {
        let line = line.trim();
        if in_note {
            in_note = !line.eq_ignore_ascii_case("end note");
            continue;
        }
        let keyword = line.split_whitespace().next().unwrap_or("");
        match keyword {
            "note" => {
                in_note = !line.contains(':');
                continue;
            }
            "}" => {
                scopes.pop();
                continue;
            }
            "--" | "classDef" | "class" | "style" | "click" => continue,
            "direction" => {
                if scopes.is_empty() {
                    if let Some(direction) =
                        line.split_whitespace().nth(1).and_then(Direction::parse)
                    {
                        graph.direction = direction;
                    }
                }
                continue;
            }
            _ if keyword.starts_with("accTitle") || keyword.starts_with("accDescr") => continue,
            _ => {}
        }
        if let Some(rest) = line.strip_prefix("state ") {
            let rest = rest.trim();
            let (rest, opens) = match rest.strip_suffix('{') {
                Some(rest) => (rest.trim(), true),
                None => (rest, false),
            };
            let (id, label) = match rest.strip_prefix('"') {
                Some(quoted) => {
                    let close = quoted.find('"').unwrap_or(quoted.len());
                    let label = &quoted[..close];
                    let after = quoted.get(close + 1..).unwrap_or("").trim();
                    let id = after.strip_prefix("as").map(str::trim).unwrap_or(label);
                    (id, Some(label))
                }
                None => (rest.split_whitespace().next().unwrap_or(""), None),
            };
            if id.is_empty() {
                continue;
            }
            let index = state_node(&mut graph, id);
            if let Some(label) = label {
                graph.nodes[index].sections[0] = clean_text(label);
            }
            if rest.contains("<<choice>>") {
                graph.nodes[index].shape = Shape::Dot('◆');
            } else if rest.contains("<<fork>>") || rest.contains("<<join>>") {
                graph.nodes[index].shape = Shape::Dot('━');
            }
            if opens {
                scopes.push(id.to_string());
            }
            continue;
        }
        if let Some(arrow) = line.find("-->") {
            let from = line[..arrow].trim();
            let rest = &line[arrow + 3..];
            let (to, label) = match rest.split_once(':') {
                Some((to, label)) => (to.trim(), clean_text(label).join(" ")),
                None => (rest.trim(), String::new()),
            };
            let mut stroke = Stroke::Solid;
            let from = if from == "[*]" {
                match scopes.last() {
                    // Entering a composite state starts at its initial state.
                    Some(scope) => {
                        stroke = Stroke::Dotted;
                        let scope = scope.clone();
                        state_node(&mut graph, &scope)
                    }
                    None => *start.get_or_insert_with(|| {
                        let index = graph.node("[*] start");
                        graph.nodes[index].shape = Shape::Dot('●');
                        index
                    }),
                }
            } else {
                state_node(&mut graph, from)
            };
            let to = if to == "[*]" {
                if !scopes.is_empty() {
                    continue;
                }
                *end.get_or_insert_with(|| {
                    let index = graph.node("[*] end");
                    graph.nodes[index].shape = Shape::Dot('◉');
                    index
                })
            } else {
                state_node(&mut graph, to)
            };
            graph.edges.push(Edge {
                label,
                stroke,
                ..Edge::new(from, to)
            });
            continue;
        }
        match line.split_once(':') {
            Some((id, description)) if is_identifier(id.trim()) => {
                let index = state_node(&mut graph, id.trim());
                descriptions
                    .entry(index)
                    .or_default()
                    .extend(clean_text(description));
            }
            _ if is_identifier(line) => {
                state_node(&mut graph, line);
            }
            _ => {}
        }
    }
    for (index, description) in descriptions {
        graph.nodes[index].sections.push(description);
    }
    Some(graph)
}

fn is_identifier(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

// --- Class diagrams -----------------------------------------------------------------------

#[derive(Default)]
struct Class {
    annotation: Option<String>,
    label: Option<String>,
    attributes: Vec<String>,
    methods: Vec<String>,
}

impl Class {
    fn member(&mut self, member: &str) {
        let member = generics(member.trim());
        if member.is_empty() {
            return;
        }
        if let Some(annotation) = member.strip_prefix("<<").and_then(|m| m.strip_suffix(">>")) {
            self.annotation = Some(annotation.to_string());
        } else if member.contains('(') {
            self.methods.push(member);
        } else {
            self.attributes.push(member);
        }
    }
}

/// Mermaid writes generics as `List~int~`.
fn generics(text: &str) -> String {
    let mut open = true;
    text.chars()
        .map(|c| match c {
            '~' => {
                open = !open;
                if open {
                    '>'
                } else {
                    '<'
                }
            }
            c => c,
        })
        .collect()
}

pub(super) fn class(body: &[&str]) -> Option<Graph> {
    let mut graph = Graph::new(Direction::Down);
    let mut classes: HashMap<usize, Class> = HashMap::new();
    let mut body_of: Option<usize> = None;

    let class_node = |graph: &mut Graph, name: &str| -> (usize, Option<String>) {
        let name = name.split(":::").next().unwrap_or(name).trim();
        let (name, label) = match name.split_once('[') {
            Some((name, label)) => (
                name.trim(),
                Some(clean_text(label.trim_end_matches(']')).join(" ")),
            ),
            None => (name, None),
        };
        let id = name.split('~').next().unwrap_or(name);
        let index = graph.node(id);
        let label = label.or_else(|| (id != name).then(|| generics(name)));
        (index, label)
    };

    for line in body {
        let line = line.trim();
        if let Some(index) = body_of {
            if line == "}" {
                body_of = None;
            } else {
                classes.entry(index).or_default().member(line);
            }
            continue;
        }
        let keyword = line.split_whitespace().next().unwrap_or("");
        match keyword {
            "direction" => {
                if let Some(direction) = line.split_whitespace().nth(1).and_then(Direction::parse) {
                    graph.direction = direction;
                }
                continue;
            }
            "note" | "click" | "link" | "callback" | "cssClass" | "style" | "classDef"
            | "namespace" | "}" => continue,
            _ if keyword.starts_with("accTitle") || keyword.starts_with("accDescr") => continue,
            _ => {}
        }
        if let Some(rest) = line.strip_prefix("<<") {
            if let Some((annotation, name)) = rest.split_once(">>") {
                let (index, _) = class_node(&mut graph, name);
                classes.entry(index).or_default().annotation = Some(annotation.to_string());
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("class ") {
            let rest = rest.trim();
            let (rest, opens) = match rest.strip_suffix('{') {
                Some(rest) => (rest.trim(), true),
                None => (rest, false),
            };
            let (index, label) = class_node(&mut graph, rest);
            let class = classes.entry(index).or_default();
            if label.is_some() {
                class.label = label;
            }
            if opens {
                body_of = Some(index);
            }
            continue;
        }
        if let Some(relation) = relation(line) {
            let (from, _) = class_node(&mut graph, relation.from);
            let (to, _) = class_node(&mut graph, relation.to);
            classes.entry(from).or_default();
            classes.entry(to).or_default();
            graph.edges.push(Edge {
                label: relation.label,
                stroke: relation.stroke,
                head: relation.head,
                tail: relation.tail,
                ..Edge::new(from, to)
            });
            continue;
        }
        if let Some((name, member)) = line.split_once(':') {
            if is_identifier(name.trim().split('~').next().unwrap_or("")) {
                let (index, _) = class_node(&mut graph, name);
                classes.entry(index).or_default().member(member);
            }
        }
    }

    for (index, class) in classes {
        let node = &mut graph.nodes[index];
        let mut title = Vec::new();
        if let Some(annotation) = class.annotation {
            title.push(format!("«{annotation}»"));
        }
        match class.label {
            Some(label) => title.push(label),
            None => title.extend(node.sections[0].iter().cloned()),
        }
        node.sections = vec![title];
        node.titled = true;
        if !class.attributes.is_empty() || !class.methods.is_empty() {
            node.sections.push(class.attributes);
        }
        if !class.methods.is_empty() {
            node.sections.push(class.methods);
        }
    }
    Some(graph)
}

struct Relation<'a> {
    from: &'a str,
    to: &'a str,
    label: String,
    stroke: Stroke,
    head: Head,
    tail: Head,
}

/// `A "1" <|-- "many" B : label`
fn relation(line: &str) -> Option<Relation<'_>> {
    let (main, label) = match line.split_once(':') {
        Some((main, label)) => (main, label.trim()),
        None => (line, ""),
    };
    let solid = main.find("--");
    let dotted = main.find("..");
    let (index, stroke) = match (solid, dotted) {
        (Some(s), Some(d)) if d < s => (d, Stroke::Dotted),
        (Some(s), _) => (s, Stroke::Solid),
        (None, Some(d)) => (d, Stroke::Dotted),
        (None, None) => return None,
    };
    let before = &main[..index];
    let after = &main[index + 2..];
    let standalone = |text: &str| text.is_empty() || text.ends_with(char::is_whitespace);
    let (tail, before) = if let Some(rest) = before.strip_suffix("<|") {
        (Head::Triangle, rest)
    } else if let Some(rest) = before.strip_suffix('*') {
        (Head::Diamond, rest)
    } else if let Some(rest) = before.strip_suffix('o').filter(|rest| standalone(rest)) {
        (Head::HollowDiamond, rest)
    } else if let Some(rest) = before.strip_suffix('<') {
        (Head::Arrow, rest)
    } else {
        (Head::None, before)
    };
    let starts_standalone = |text: &str| text.is_empty() || text.starts_with(char::is_whitespace);
    let (head, after) = if let Some(rest) = after.strip_prefix("|>") {
        (Head::Triangle, rest)
    } else if let Some(rest) = after.strip_prefix('*') {
        (Head::Diamond, rest)
    } else if let Some(rest) = after
        .strip_prefix('o')
        .filter(|rest| starts_standalone(rest))
    {
        (Head::HollowDiamond, rest)
    } else if let Some(rest) = after.strip_prefix('>') {
        (Head::Arrow, rest)
    } else {
        (Head::None, after)
    };
    let (from, from_cardinality) = split_cardinality(before.trim(), true);
    let (to, to_cardinality) = split_cardinality(after.trim(), false);
    let valid = |name: &str| is_identifier(name.split(['~', '[']).next().unwrap_or(""));
    if !valid(from) || !valid(to) {
        return None;
    }
    let mut label = clean_text(label).join(" ");
    if !from_cardinality.is_empty() || !to_cardinality.is_empty() {
        let cardinality = format!("[{from_cardinality}:{to_cardinality}]");
        label = if label.is_empty() {
            cardinality
        } else {
            format!("{label} {cardinality}")
        };
    }
    Some(Relation {
        from,
        to,
        label,
        stroke,
        head,
        tail,
    })
}

/// Split a quoted cardinality off a class name: `A "1"` or `"many" B`.
fn split_cardinality(text: &str, trailing: bool) -> (&str, &str) {
    let quoted = if trailing {
        text.strip_suffix('"').and_then(|rest| {
            rest.rfind('"')
                .map(|open| (&rest[..open], &rest[open + 1..]))
        })
    } else {
        text.strip_prefix('"').and_then(|rest| {
            rest.find('"')
                .map(|close| (&rest[close + 1..], &rest[..close]))
        })
    };
    match quoted {
        Some((name, cardinality)) => (name.trim(), cardinality.trim()),
        None => (text, ""),
    }
}

// --- Entity relationship diagrams ---------------------------------------------------------

pub(super) fn entity_relationship(body: &[&str]) -> Option<Graph> {
    let mut graph = Graph::new(Direction::Down);
    let mut attributes: HashMap<usize, Vec<String>> = HashMap::new();
    let mut entities: Vec<usize> = Vec::new();
    let mut body_of: Option<usize> = None;

    let entity = |graph: &mut Graph, entities: &mut Vec<usize>, name: &str| -> usize {
        let name = name.trim();
        let (id, label) = match name.split_once('[') {
            Some((id, label)) => (id.trim(), Some(label.trim_end_matches(']'))),
            None => (name.trim_matches('"'), None),
        };
        let exists = graph.ids.contains_key(id);
        let index = graph.node(id);
        if let Some(label) = label {
            graph.nodes[index].sections[0] = clean_text(label);
        }
        if !exists {
            entities.push(index);
        }
        index
    };

    for line in body {
        let line = line.trim();
        if let Some(index) = body_of {
            if line == "}" {
                body_of = None;
            } else {
                // type name PK, FK "comment"
                let line = line.split('"').next().unwrap_or(line);
                let attribute = line.split_whitespace().collect::<Vec<_>>().join(" ");
                if !attribute.is_empty() {
                    attributes.entry(index).or_default().push(attribute);
                }
            }
            continue;
        }
        let keyword = line.split_whitespace().next().unwrap_or("");
        match keyword {
            "direction" => {
                if let Some(direction) = line.split_whitespace().nth(1).and_then(Direction::parse) {
                    graph.direction = direction;
                }
                continue;
            }
            "style" | "classDef" | "class" => continue,
            _ if keyword.starts_with("accTitle") || keyword.starts_with("accDescr") => continue,
            _ => {}
        }
        if let Some(name) = line.strip_suffix('{') {
            body_of = Some(entity(&mut graph, &mut entities, name));
            continue;
        }
        let (main, label) = match line.split_once(':') {
            Some((main, label)) => (main, clean_text(label).join(" ")),
            None => (line, String::new()),
        };
        let parts: Vec<&str> = main.split_whitespace().collect();
        if let [left, relationship, right] = parts[..] {
            let identifying = relationship.contains("--");
            if (identifying || relationship.contains("..")) && relationship.len() >= 6 {
                let from = entity(&mut graph, &mut entities, left);
                let to = entity(&mut graph, &mut entities, right);
                let cardinality = |marker: &str| match marker {
                    "||" => "1",
                    "|o" | "o|" => "0..1",
                    "}|" | "|{" => "1..N",
                    "}o" | "o{" => "0..N",
                    _ => "?",
                };
                let cardinality = format!(
                    "[{}:{}]",
                    cardinality(relationship.get(..2).unwrap_or("")),
                    cardinality(relationship.get(relationship.len() - 2..).unwrap_or(""))
                );
                graph.edges.push(Edge {
                    label: if label.is_empty() {
                        cardinality
                    } else {
                        format!("{label} {cardinality}")
                    },
                    stroke: if identifying {
                        Stroke::Solid
                    } else {
                        Stroke::Dotted
                    },
                    head: Head::None,
                    ..Edge::new(from, to)
                });
            }
        } else if parts.len() == 1 {
            entity(&mut graph, &mut entities, parts[0]);
        }
    }
    for index in entities {
        let node = &mut graph.nodes[index];
        node.titled = true;
        if let Some(attributes) = attributes.remove(&index) {
            node.sections.push(attributes);
        }
    }
    Some(graph)
}

#[cfg(test)]
mod tests {
    use super::super::tests::plain;
    use super::*;

    fn edges(source: &str) -> Vec<(String, String, String)> {
        let lines: Vec<&str> = source.lines().collect();
        let graph = flowchart(lines[0], &lines[1..]).unwrap();
        let name = |index: usize| graph.nodes[index].sections[0].join(" ");
        graph
            .edges
            .iter()
            .map(|e| (name(e.from), name(e.to), e.label.clone()))
            .collect()
    }

    #[test]
    fn parses_flowchart_syntax() {
        let e = |a: &str, b: &str, l: &str| (a.to_string(), b.to_string(), l.to_string());
        assert_eq!(
            edges("graph TD\nA[Start] -->|go| B(Next) --> C{Check?}"),
            [e("Start", "Next", "go"), e("Next", "Check?", "")]
        );
        assert_eq!(
            edges("flowchart LR; A -- some text --> B; B -.-> C"),
            [e("A", "B", "some text"), e("B", "C", "")]
        );
        assert_eq!(
            edges("graph TD\nA & B --> C"),
            [e("A", "C", ""), e("B", "C", "")]
        );
        assert_eq!(
            edges("graph TD\nA[\"quoted [text]\"] ==> my-node"),
            [e("quoted [text]", "my-node", "")]
        );
        assert_eq!(
            edges("graph TD\nA:::warn --> B@{ shape: diamond, label: \"Ok?\" }"),
            [e("A", "Ok?", "")]
        );
        assert_eq!(
            edges("graph TD\nsubgraph one [First group]\nA\nend\nA --> one"),
            [e("A", "First group", "")]
        );
    }

    #[test]
    fn parses_link_styles() {
        let lines = ["graph TD", "A <--> B", "B --o C", "C -.- D", "D ---> E"];
        let graph = flowchart(lines[0], &lines[1..]).unwrap();
        let e = &graph.edges;
        assert_eq!((e[0].tail, e[0].head), (Head::Arrow, Head::Arrow));
        assert_eq!(e[1].head, Head::Circle);
        assert_eq!((e[2].head, e[2].stroke), (Head::None, Stroke::Dotted));
        assert_eq!(e[3].minlen, 2);
    }

    #[test]
    fn renders_state_diagrams() {
        let text = plain(
            "stateDiagram-v2\n[*] --> Still\nStill --> Moving : push\nMoving --> [*]",
            80,
        );
        assert!(text.contains('●'), "{text}");
        assert!(text.contains('◉'), "{text}");
        assert!(text.contains("╭───────╮"), "{text}");
        assert!(text.contains("push"), "{text}");
    }

    #[test]
    fn renders_class_diagrams() {
        let text = plain(
            "classDiagram\nAnimal <|-- Duck\nclass Animal {\n  <<abstract>>\n  +int age\n  +isMammal() bool\n}\nDuck : +swim()",
            80,
        );
        assert!(text.contains("«abstract»"), "{text}");
        assert!(text.contains("+int age"), "{text}");
        assert!(text.contains("+swim()"), "{text}");
        assert!(text.contains('△'), "{text}");
        assert!(text.contains('├'), "{text}");
    }

    #[test]
    fn renders_entity_relationship_diagrams() {
        let text = plain(
            "erDiagram\nCUSTOMER ||--o{ ORDER : places\nCUSTOMER {\n  string name PK \"the name\"\n}",
            80,
        );
        assert!(text.contains("CUSTOMER"), "{text}");
        assert!(text.contains("string name PK"), "{text}");
        assert!(!text.contains("the name"), "{text}");
        assert!(text.contains("places [1:0..N]"), "{text}");
    }
}

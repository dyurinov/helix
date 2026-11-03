//! Pie charts, drawn as bar charts, and mindmaps, drawn as trees.

use super::{clean_text, width, Line, Role};

const BAR_ROLES: [Role; 4] = [Role::Border, Role::Label, Role::Arrow, Role::Title];
const EIGHTHS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];

pub(super) fn pie(header: &str, body: &[&str], max_width: usize) -> Option<Vec<Line>> {
    let mut show_data = header.contains("showData");
    let mut title = header
        .find("title")
        .map(|start| header[start + 5..].trim().to_string());
    let mut slices: Vec<(String, f64)> = Vec::new();
    for line in body {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("title") {
            title = Some(rest.trim().to_string());
        } else if line == "showData" {
            show_data = true;
        } else if let Some((label, value)) = line.rsplit_once(':') {
            let Ok(value) = value.trim().parse::<f64>() else {
                continue;
            };
            if value.is_finite() && value >= 0.0 {
                slices.push((clean_text(label).join(" "), value));
            }
        }
    }
    let total: f64 = slices.iter().map(|(_, value)| value).sum();
    let largest = slices.iter().map(|(_, value)| *value).fold(0.0, f64::max);
    if slices.is_empty() || total <= 0.0 {
        return None;
    }

    let format_value = |value: f64| {
        if value.fract() == 0.0 {
            format!("{value:.0}")
        } else {
            format!("{value}")
        }
    };
    let label_width = slices
        .iter()
        .map(|(label, _)| width(label))
        .max()
        .unwrap_or(0);
    let data_width = if show_data {
        slices
            .iter()
            .map(|(_, value)| format_value(*value).len() + 3)
            .max()
            .unwrap_or(0)
    } else {
        0
    };
    let bar_width = max_width
        .saturating_sub(label_width + 2 + 7 + data_width)
        .clamp(10, 40);

    let mut lines = Vec::new();
    if let Some(title) = title.filter(|title| !title.is_empty()) {
        lines.push(vec![(clean_text(&title).join(" "), Role::Title)]);
    }
    for (index, (label, value)) in slices.iter().enumerate() {
        let eighths = (value / largest * bar_width as f64 * 8.0).round() as usize;
        let mut bar = "█".repeat(eighths / 8);
        bar.push_str(EIGHTHS[eighths % 8]);
        let padding = bar_width + 1 - width(&bar);
        let mut line = vec![
            (
                format!("{label}{}  ", " ".repeat(label_width - width(label))),
                Role::Text,
            ),
            (bar, BAR_ROLES[index % BAR_ROLES.len()]),
            (
                format!("{}{:5.1}%", " ".repeat(padding), value / total * 100.0),
                Role::Muted,
            ),
        ];
        if show_data {
            line.push((format!(" ({})", format_value(*value)), Role::Muted));
        }
        lines.push(line);
    }
    Some(lines)
}

struct Branch {
    text: String,
    children: Vec<usize>,
}

pub(super) fn mindmap(body: &[&str]) -> Option<Vec<Line>> {
    let mut nodes: Vec<Branch> = Vec::new();
    // (indentation, node)
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for line in body {
        let text = line.trim();
        if text.starts_with("::icon(") {
            continue;
        }
        let indent: usize = line
            .chars()
            .take_while(|c| c.is_whitespace())
            .map(|c| if c == '\t' { 4 } else { 1 })
            .sum();
        let text = text.split(":::").next().unwrap_or(text);
        let index = nodes.len();
        nodes.push(Branch {
            text: node_text(text),
            children: Vec::new(),
        });
        while stack.last().is_some_and(|&(level, _)| level >= indent) {
            stack.pop();
        }
        match stack.last() {
            Some(&(_, parent)) => nodes[parent].children.push(index),
            // Further roots are attached to the first one.
            None if index > 0 => nodes[0].children.push(index),
            None => {}
        }
        stack.push((indent, index));
    }
    if nodes.is_empty() {
        return None;
    }
    let mut lines = vec![vec![(nodes[0].text.clone(), Role::Title)]];
    let mut pending: Vec<(usize, String, bool)> = nodes[0]
        .children
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &child)| (child, String::new(), i == 0))
        .collect();
    while let Some((index, prefix, last)) = pending.pop() {
        let branch = if last { "└─ " } else { "├─ " };
        let role = if prefix.is_empty() {
            Role::Border
        } else {
            Role::Text
        };
        lines.push(vec![
            (format!("{prefix}{branch}"), Role::Muted),
            (nodes[index].text.clone(), role),
        ]);
        let child_prefix = format!("{prefix}{}", if last { "   " } else { "│  " });
        for (i, &child) in nodes[index].children.iter().rev().enumerate() {
            pending.push((child, child_prefix.clone(), i == 0));
        }
    }
    Some(lines)
}

/// The text of a mindmap node: `id((text))`, `id[text]`, `id)text(` or plain text.
fn node_text(text: &str) -> String {
    let start = text.find(['(', '[', '{', ')']);
    let inner = match start {
        // Only after an id: "Text (with a remark)" is plain text.
        Some(start)
            if text.ends_with([')', ']', '}', '('])
                && !text[..start].contains(char::is_whitespace) =>
        {
            let inner = &text[start..];
            inner.trim_matches(['(', ')', '[', ']', '{', '}'])
        }
        _ => text,
    };
    clean_text(inner).join(" ")
}

#[cfg(test)]
mod tests {
    use super::super::tests::plain;

    #[test]
    fn renders_pie_charts_as_bars() {
        let text = plain(
            "pie showData title Pets\n\"Dogs\" : 386\n\"Cats\" : 85.5\n\"Rats\" : 15",
            60,
        );
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "Pets");
        assert!(lines[1].starts_with("Dogs  ████"), "{text}");
        assert!(lines[1].ends_with("79.3% (386)"), "{text}");
        assert!(lines[2].contains("17.6% (85.5)"), "{text}");
        assert!(
            lines.iter().all(|line| line.chars().count() <= 60),
            "{text}"
        );
    }

    #[test]
    fn renders_mindmaps_as_trees() {
        let text = plain(
            "mindmap\n  root((Project))\n    Goals\n      Fast\n      Simple\n    Risks[Known risks]",
            80,
        );
        assert_eq!(
            text,
            "Project\n├─ Goals\n│  ├─ Fast\n│  └─ Simple\n└─ Known risks"
        );
    }
}

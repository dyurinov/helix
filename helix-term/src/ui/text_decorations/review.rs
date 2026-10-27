use helix_core::doc_formatter::FormattedGrapheme;
use helix_core::line_ending::line_end_char_index;
use helix_core::Position;
use helix_view::review::{annotation_rows, annotations_on_line, Role};
use helix_view::theme::{Modifier, Style};
use helix_view::{Document, Theme};

use crate::ui::document::{LinePos, TextRenderer};
use crate::ui::text_decorations::Decoration;

struct Styles {
    frame: Style,
    author: Style,
    draft: Style,
    meta: Style,
    body: Style,
    resolved: Style,
}

impl Styles {
    fn new(theme: &Theme) -> Self {
        let get = |scope: &str, fallback: &str| {
            theme
                .try_get_exact(scope)
                .unwrap_or_else(|| theme.get(fallback))
        };
        Styles {
            frame: get("ui.virtual.review.frame", "info"),
            author: get("ui.virtual.review.author", "info").add_modifier(Modifier::BOLD),
            draft: get("ui.virtual.review.draft", "warning"),
            meta: get("ui.virtual.review.meta", "comment"),
            body: get("ui.virtual.review.body", "ui.text"),
            resolved: get("ui.virtual.review.resolved", "comment"),
        }
    }

    fn get(&self, role: Role) -> Style {
        match role {
            Role::Frame => self.frame,
            Role::Author => self.author,
            Role::Draft => self.draft,
            Role::Meta => self.meta,
            Role::Body => self.body,
            Role::Resolved => self.resolved,
        }
    }
}

/// Draws review threads in the virtual lines reserved by
/// [`helix_view::review::ReviewThreadLines`].
pub struct ReviewThreads<'a> {
    doc: &'a Document,
    styles: Styles,
    width: usize,
    /// Index of the next annotation whose line end has not been rendered yet.
    next: usize,
    /// The document line whose threads are drawn after the current visual line.
    pending: Option<usize>,
}

impl<'a> ReviewThreads<'a> {
    pub fn new(doc: &'a Document, theme: &Theme, width: u16) -> Self {
        ReviewThreads {
            doc,
            styles: Styles::new(theme),
            width: width as usize,
            next: 0,
            pending: None,
        }
    }

    /// The char index of the newline ending the line of the annotation at `self.next`.
    fn next_anchor(&self) -> usize {
        let text = self.doc.text().slice(..);
        self.doc
            .review_annotations
            .get(self.next)
            .map_or(usize::MAX, |annotation| {
                line_end_char_index(&text, text.char_to_line(annotation.char_idx))
            })
    }
}

impl Decoration for ReviewThreads<'_> {
    fn reset_pos(&mut self, pos: usize) -> usize {
        let text = self.doc.text().slice(..);
        self.pending = None;
        // Skip threads whose line ended before `pos`.
        self.next = self.doc.review_annotations.partition_point(|annotation| {
            line_end_char_index(&text, text.char_to_line(annotation.char_idx)) < pos
        });
        self.next_anchor()
    }

    fn decorate_grapheme(
        &mut self,
        _renderer: &mut TextRenderer,
        grapheme: &FormattedGrapheme,
    ) -> usize {
        let text = self.doc.text().slice(..);
        let line = text.char_to_line(grapheme.char_idx.min(text.len_chars()));
        let range = annotations_on_line(&self.doc.review_annotations, text, line);
        if !range.is_empty() {
            self.pending = Some(line);
        }
        self.next = self.next.max(range.end);
        // Advance past every annotation on this line.
        while self
            .doc
            .review_annotations
            .get(self.next)
            .is_some_and(|annotation| text.char_to_line(annotation.char_idx) <= line)
        {
            self.next += 1;
        }
        self.next_anchor()
    }

    fn render_virt_lines(
        &mut self,
        renderer: &mut TextRenderer,
        pos: LinePos,
        virt_off: Position,
    ) -> Position {
        let Some(line) = self.pending.take() else {
            return Position::new(0, 0);
        };
        if line != pos.doc_line {
            return Position::new(0, 0);
        }
        let text = self.doc.text().slice(..);
        let annotations = &self.doc.review_annotations;
        let mut row = pos.visual_line as usize + virt_off.row;
        let first_row = row;
        for annotation in &annotations[annotations_on_line(annotations, text, line)] {
            for layout_row in annotation_rows(annotation, self.width) {
                let visible = row >= renderer.offset.row
                    && row - renderer.offset.row < renderer.viewport.height as usize;
                if visible {
                    let mut col = 0usize;
                    for (segment, role) in layout_row {
                        let remaining = self.width.saturating_sub(col);
                        if remaining == 0 {
                            break;
                        }
                        let (end, _) = renderer.set_string_truncated(
                            renderer.viewport.x + col as u16,
                            row as u16,
                            &segment,
                            remaining,
                            |_| self.styles.get(role),
                            false,
                            false,
                        );
                        col = (end - renderer.viewport.x) as usize;
                    }
                }
                row += 1;
            }
        }
        Position::new(row - first_row, 0)
    }
}

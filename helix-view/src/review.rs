//! Code review sessions: the reviewed files, comment threads and draft comments.
//!
//! A [`ReviewSession`] compares a worktree against a base commit. Draft comments and viewed
//! files are persisted under the repository's Git directory so a review survives restarts.
//! Threads loaded from a GitHub pull request are never persisted; they are fetched again.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use helix_core::{
    text_annotations::LineAnnotation, unicode::width::UnicodeWidthChar, Position, RopeSlice,
};

use crate::graphics::Style;
use serde::{Deserialize, Serialize};

use crate::Document;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Untracked,
}

impl FileStatus {
    pub fn label(self) -> &'static str {
        match self {
            FileStatus::Added => "A",
            FileStatus::Modified => "M",
            FileStatus::Deleted => "D",
            FileStatus::Renamed => "R",
            FileStatus::Untracked => "?",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReviewFile {
    /// Path relative to the review root.
    pub path: PathBuf,
    /// The previous path of a renamed file.
    pub old_path: Option<PathBuf>,
    pub status: FileStatus,
    pub additions: usize,
    pub deletions: usize,
    pub binary: bool,
}

#[derive(Debug, Clone)]
pub struct PullRequest {
    pub number: u64,
    /// GitHub host, `github.com` or a GitHub Enterprise server.
    pub host: String,
    pub owner: String,
    pub repo: String,
    pub title: String,
    pub url: String,
    pub author: String,
    pub body: String,
    pub head_ref: String,
    pub head_oid: String,
    pub base_ref: String,
    pub state: String,
    pub is_draft: bool,
}

#[derive(Debug, Clone)]
pub struct Comment {
    pub author: String,
    pub body: String,
    pub created_at: String,
    /// Draft comments exist only locally until the review is submitted.
    pub draft: bool,
    /// Stable identifier of a draft, persisted with it. Empty for submitted comments.
    pub uid: String,
}

/// A process-unique identifier for a new draft comment.
pub fn new_uid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| time.as_nanos());
    format!("{nanos:x}-{:x}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Identifies a GitHub review thread.
#[derive(Debug, Clone)]
pub struct RemoteThread {
    /// GraphQL node id, used to resolve the thread.
    pub node_id: String,
    /// REST id of the first comment, used to reply to the thread.
    pub reply_to: u64,
}

pub type ThreadId = u64;

#[derive(Debug, Clone)]
pub struct Thread {
    pub id: ThreadId,
    /// Path relative to the review root.
    pub path: PathBuf,
    /// First commented line (1-based, inclusive).
    pub start_line: usize,
    /// Last commented line (1-based, inclusive). Inline threads are drawn below this line.
    pub line: usize,
    pub resolved: bool,
    /// The commented code changed after the comment was written.
    pub outdated: bool,
    /// The comment refers to removed lines of the base version.
    pub on_removed_lines: bool,
    pub remote: Option<RemoteThread>,
    pub comments: Vec<Comment>,
}

impl Thread {
    pub fn is_draft(&self) -> bool {
        self.remote.is_none()
    }

    pub fn has_drafts(&self) -> bool {
        self.comments.iter().any(|comment| comment.draft)
    }

    pub fn contains_line(&self, line: usize) -> bool {
        (self.start_line..=self.line).contains(&line)
    }

    pub fn line_label(&self) -> String {
        if self.start_line < self.line {
            format!("L{}-{}", self.start_line, self.line)
        } else {
            format!("L{}", self.line)
        }
    }

    pub fn state_label(&self) -> &'static str {
        if self.is_draft() {
            "draft"
        } else if self.resolved {
            "resolved"
        } else if self.outdated {
            "outdated"
        } else if self.has_drafts() {
            "reply"
        } else {
            "open"
        }
    }
}

pub struct ReviewSession {
    /// Unique per session, so background work can detect that its session was replaced.
    pub generation: u64,
    /// Canonical path of the reviewed worktree.
    pub root: PathBuf,
    /// The ref the review was started against, for example `origin/main`.
    pub base_label: String,
    /// The merge base of `base_label` and `HEAD`.
    pub base_commit: String,
    /// The merge base GitHub uses for the pull request diff, which differs from `base_commit`
    /// when the review was started against an explicit base.
    pub pr_base_commit: Option<String>,
    pub head_label: String,
    pub head_commit: String,
    pub pr: Option<PullRequest>,
    pub files: Vec<ReviewFile>,
    pub threads: Vec<Thread>,
    /// Viewed files and a hash of their contents when they were marked.
    pub viewed: HashMap<PathBuf, u64>,
    pub show_inline: bool,
    /// The explicit base passed to `:review`, reused by `:review-refresh`.
    pub requested_base: Option<String>,
    pub state_file: PathBuf,
    /// Draft replies whose thread could not be loaded, kept so they are not lost.
    orphan_replies: Vec<SavedReply>,
    next_id: ThreadId,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SavedState {
    #[serde(default)]
    viewed: Vec<SavedViewed>,
    #[serde(default)]
    drafts: Vec<SavedDraft>,
    #[serde(default)]
    replies: Vec<SavedReply>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SavedViewed {
    path: PathBuf,
    hash: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct SavedDraft {
    #[serde(default)]
    uid: String,
    path: PathBuf,
    start_line: usize,
    line: usize,
    body: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct SavedReply {
    #[serde(default)]
    uid: String,
    thread: String,
    body: String,
}

pub const DRAFT_AUTHOR: &str = "you";

impl ReviewSession {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        root: PathBuf,
        base_label: String,
        base_commit: String,
        pr_base_commit: Option<String>,
        head_label: String,
        head_commit: String,
        pr: Option<PullRequest>,
        files: Vec<ReviewFile>,
        remote_threads: Vec<Thread>,
        state_file: PathBuf,
        requested_base: Option<String>,
    ) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static GENERATION: AtomicU64 = AtomicU64::new(1);
        let mut session = ReviewSession {
            generation: GENERATION.fetch_add(1, Ordering::Relaxed),
            root,
            base_label,
            base_commit,
            pr_base_commit,
            head_label,
            head_commit,
            pr,
            files,
            threads: Vec::new(),
            viewed: HashMap::new(),
            show_inline: true,
            requested_base,
            state_file,
            orphan_replies: Vec::new(),
            next_id: 1,
        };
        for mut thread in remote_threads {
            thread.id = session.next_id();
            session.threads.push(thread);
        }
        session.load_state();
        session.revalidate_viewed();
        session.sort_threads();
        session
    }

    fn next_id(&mut self) -> ThreadId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn sort_threads(&mut self) {
        self.threads
            .sort_by(|a, b| (&a.path, a.line, a.id).cmp(&(&b.path, b.line, b.id)));
    }

    /// Path of `path` relative to the review root, if it is inside it.
    pub fn relative_path(&self, path: &Path) -> Option<PathBuf> {
        // The root has symlinks resolved, so resolve them here as well.
        let path = std::fs::canonicalize(path)
            .or_else(|_| {
                // Deleted files can not be canonicalized; resolve their directory instead.
                let parent = std::fs::canonicalize(path.parent().unwrap_or(path))?;
                Ok::<_, std::io::Error>(parent.join(path.file_name().unwrap_or_default()))
            })
            .unwrap_or_else(|_| helix_stdx::path::canonicalize(path));
        path.strip_prefix(&self.root).ok().map(Path::to_path_buf)
    }

    pub fn absolute_path(&self, relative: &Path) -> PathBuf {
        self.root.join(relative)
    }

    pub fn file(&self, relative: &Path) -> Option<&ReviewFile> {
        self.files.iter().find(|file| file.path == relative)
    }

    pub fn file_index(&self, relative: &Path) -> Option<usize> {
        self.files.iter().position(|file| file.path == relative)
    }

    pub fn thread(&self, id: ThreadId) -> Option<&Thread> {
        self.threads.iter().find(|thread| thread.id == id)
    }

    pub fn thread_mut(&mut self, id: ThreadId) -> Option<&mut Thread> {
        self.threads.iter_mut().find(|thread| thread.id == id)
    }

    pub fn threads_in<'a>(&'a self, relative: &'a Path) -> impl Iterator<Item = &'a Thread> + 'a {
        self.threads
            .iter()
            .filter(move |thread| thread.path == relative)
    }

    /// The thread to act on for the cursor at `line` (1-based): the narrowest thread covering
    /// the line.
    pub fn thread_at(&self, relative: &Path, line: usize) -> Option<&Thread> {
        self.threads
            .iter()
            .filter(|thread| thread.path == relative && thread.contains_line(line))
            .min_by_key(|thread| (thread.line - thread.start_line, thread.id))
    }

    pub fn add_draft(
        &mut self,
        path: PathBuf,
        start_line: usize,
        line: usize,
        body: String,
    ) -> ThreadId {
        self.push_draft(path, start_line, line, body, new_uid())
    }

    fn push_draft(
        &mut self,
        path: PathBuf,
        start_line: usize,
        line: usize,
        body: String,
        uid: String,
    ) -> ThreadId {
        let id = self.next_id();
        self.threads.push(Thread {
            id,
            path,
            start_line: start_line.min(line).max(1),
            line: line.max(start_line).max(1),
            resolved: false,
            outdated: false,
            on_removed_lines: false,
            remote: None,
            comments: vec![Comment {
                author: DRAFT_AUTHOR.to_string(),
                body,
                created_at: String::new(),
                draft: true,
                uid,
            }],
        });
        self.sort_threads();
        id
    }

    /// Take over drafts from the session this one replaces: drafts written after this
    /// session was loaded from disk, and positions that followed edits meanwhile.
    pub fn merge_drafts(&mut self, previous: &ReviewSession) {
        for thread in &previous.threads {
            for comment in thread.comments.iter().filter(|comment| comment.draft) {
                let existing = self
                    .threads
                    .iter_mut()
                    .find(|t| t.comments.iter().any(|c| c.draft && c.uid == comment.uid));
                match (existing, &thread.remote) {
                    (Some(existing), None) => {
                        existing.start_line = thread.start_line;
                        existing.line = thread.line;
                    }
                    (Some(_), Some(_)) => {}
                    (None, None) => {
                        self.push_draft(
                            thread.path.clone(),
                            thread.start_line,
                            thread.line,
                            comment.body.clone(),
                            comment.uid.clone(),
                        );
                    }
                    (None, Some(remote)) => {
                        let target = self.threads.iter_mut().find(|t| {
                            t.remote
                                .as_ref()
                                .is_some_and(|r| r.node_id == remote.node_id)
                        });
                        match target {
                            Some(target) => target.comments.push(comment.clone()),
                            None => self.orphan_replies.push(SavedReply {
                                uid: comment.uid.clone(),
                                thread: remote.node_id.clone(),
                                body: comment.body.clone(),
                            }),
                        }
                    }
                }
            }
        }
        self.sort_threads();
    }

    /// Remove draft comments by uid, dropping draft threads left without comments.
    pub fn remove_drafts(&mut self, uids: &[String]) {
        for thread in &mut self.threads {
            thread
                .comments
                .retain(|comment| !(comment.draft && uids.contains(&comment.uid)));
        }
        self.threads
            .retain(|thread| !(thread.is_draft() && thread.comments.is_empty()));
        self.orphan_replies
            .retain(|reply| !uids.contains(&reply.uid));
    }

    pub fn draft_count(&self) -> usize {
        self.threads
            .iter()
            .map(|thread| thread.comments.iter().filter(|c| c.draft).count())
            .sum::<usize>()
            + self.orphan_replies.len()
    }

    pub fn open_thread_count(&self) -> usize {
        self.threads
            .iter()
            .filter(|thread| !thread.is_draft() && !thread.resolved)
            .count()
    }

    pub fn viewed_count(&self) -> usize {
        self.files
            .iter()
            .filter(|file| self.viewed.contains_key(&file.path))
            .count()
    }

    pub fn is_viewed(&self, relative: &Path) -> bool {
        self.viewed.contains_key(relative)
    }

    /// Mark a file as viewed. Viewed files become unviewed again when their contents change.
    pub fn set_viewed(&mut self, relative: &Path, viewed: bool) {
        if viewed {
            let hash = content_hash(&self.absolute_path(relative));
            self.viewed.insert(relative.to_path_buf(), hash);
        } else {
            self.viewed.remove(relative);
        }
    }

    /// Forget viewed marks for files that changed since they were viewed.
    pub fn revalidate_viewed(&mut self) {
        let root = self.root.clone();
        let files: Vec<_> = self.files.iter().map(|file| file.path.clone()).collect();
        self.viewed
            .retain(|path, hash| files.contains(path) && content_hash(&root.join(path)) == *hash);
    }

    fn load_state(&mut self) {
        let Ok(contents) = std::fs::read_to_string(&self.state_file) else {
            return;
        };
        let state: SavedState = match serde_json::from_str(&contents) {
            Ok(state) => state,
            Err(err) => {
                // Keep the unreadable file instead of overwriting it on the next save.
                let backup = self.state_file.with_extension("json.bak");
                log::warn!(
                    "invalid review state {}: {err}; moved to {}",
                    self.state_file.display(),
                    backup.display()
                );
                let _ = std::fs::rename(&self.state_file, backup);
                return;
            }
        };
        for viewed in state.viewed {
            self.viewed.insert(viewed.path, viewed.hash);
        }
        for draft in state.drafts {
            let uid = if draft.uid.is_empty() {
                new_uid()
            } else {
                draft.uid
            };
            self.push_draft(draft.path, draft.start_line, draft.line, draft.body, uid);
        }
        for reply in state.replies {
            let thread = self.threads.iter_mut().find(|thread| {
                thread
                    .remote
                    .as_ref()
                    .is_some_and(|remote| remote.node_id == reply.thread)
            });
            let uid = if reply.uid.is_empty() {
                new_uid()
            } else {
                reply.uid.clone()
            };
            match thread {
                Some(thread) => thread.comments.push(Comment {
                    author: DRAFT_AUTHOR.to_string(),
                    body: reply.body,
                    created_at: String::new(),
                    draft: true,
                    uid,
                }),
                // The thread could not be loaded, for example while offline.
                None => self.orphan_replies.push(reply),
            }
        }
    }

    /// Persist draft comments and viewed files.
    pub fn save(&self) -> anyhow::Result<()> {
        let mut state = SavedState::default();
        for (path, hash) in &self.viewed {
            state.viewed.push(SavedViewed {
                path: path.clone(),
                hash: *hash,
            });
        }
        state.viewed.sort_by(|a, b| a.path.cmp(&b.path));
        for thread in &self.threads {
            for comment in thread.comments.iter().filter(|comment| comment.draft) {
                match &thread.remote {
                    None => state.drafts.push(SavedDraft {
                        uid: comment.uid.clone(),
                        path: thread.path.clone(),
                        start_line: thread.start_line,
                        line: thread.line,
                        body: comment.body.clone(),
                    }),
                    Some(remote) => state.replies.push(SavedReply {
                        uid: comment.uid.clone(),
                        thread: remote.node_id.clone(),
                        body: comment.body.clone(),
                    }),
                }
            }
        }
        for reply in &self.orphan_replies {
            state.replies.push(SavedReply {
                uid: reply.uid.clone(),
                thread: reply.thread.clone(),
                body: reply.body.clone(),
            });
        }
        if state.viewed.is_empty() && state.drafts.is_empty() && state.replies.is_empty() {
            match std::fs::remove_file(&self.state_file) {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => return Err(err.into()),
                _ => return Ok(()),
            }
        }
        if let Some(parent) = self.state_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = serde_json::to_string_pretty(&state)?;
        let tmp = self.state_file.with_extension("json.tmp");
        std::fs::write(&tmp, contents)?;
        std::fs::rename(&tmp, &self.state_file)?;
        Ok(())
    }

    /// Render all comments as Markdown, suitable for pasting into a pull request or handing to
    /// another tool.
    pub fn to_markdown(
        &self,
        read_lines: impl Fn(&Path, usize, usize) -> Option<String>,
    ) -> String {
        let mut out = format!(
            "# Review of `{}` against `{}`\n",
            self.head_label, self.base_label
        );
        if let Some(pr) = &self.pr {
            out.push_str(&format!("\n{} — #{} {}\n", pr.url, pr.number, pr.title));
        }
        let mut current: Option<&Path> = None;
        for thread in &self.threads {
            if thread.resolved {
                continue;
            }
            if current != Some(thread.path.as_path()) {
                current = Some(thread.path.as_path());
                out.push_str(&format!("\n## `{}`\n", thread.path.display()));
            }
            out.push_str(&format!(
                "\n### {} ({})\n\n",
                thread.line_label(),
                thread.state_label()
            ));
            if let Some(code) = read_lines(&thread.path, thread.start_line, thread.line) {
                let lang = thread
                    .path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .unwrap_or("");
                out.push_str(&format!("```{lang}\n{}\n```\n\n", code.trim_end()));
            }
            for comment in &thread.comments {
                let draft = if comment.draft { " (draft)" } else { "" };
                out.push_str(&format!(
                    "**{}**{draft}: {}\n\n",
                    comment.author, comment.body
                ));
            }
        }
        out
    }
}

/// FNV-1a hash of a file's contents, or 0 if it can not be read.
pub fn content_hash(path: &Path) -> u64 {
    let Ok(bytes) = std::fs::read(path) else {
        return 0;
    };
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ *byte as u64).wrapping_mul(0x100000001b3)
    })
}

/// Convert `\n` escapes typed into a single line prompt into newlines.
pub fn unescape_body(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.peek() {
                Some('n') => {
                    chars.next();
                    out.push('\n');
                    continue;
                }
                Some('\\') => {
                    chars.next();
                    out.push('\\');
                    continue;
                }
                _ => {}
            }
        }
        out.push(ch);
    }
    out.trim().to_string()
}

/// The inverse of [`unescape_body`], used to edit a comment in a single line prompt.
pub fn escape_body(body: &str) -> String {
    body.replace('\\', "\\\\").replace('\n', "\\n")
}

// --- Inline rendering ---------------------------------------------------------------------

/// A thread rendered to styled lines (Markdown is rendered by the caller). Lines are wrapped
/// to the view width by [`layout_rows`].
#[derive(Debug, Clone, Default)]
pub struct RenderedThread {
    pub frame: Style,
    pub lines: Vec<RenderedLine>,
}

#[derive(Debug, Clone, Default)]
pub struct RenderedLine {
    pub spans: Vec<(String, Style)>,
    /// Indentation of wrapped continuation rows, for example to align list items.
    pub indent: usize,
}

impl RenderedLine {
    pub fn new(spans: Vec<(String, Style)>) -> Self {
        RenderedLine { spans, indent: 0 }
    }
}

/// A review thread anchored in a document. `char_idx` is mapped through edits so threads stay
/// attached to their line while the document changes.
#[derive(Debug, Clone)]
pub struct ReviewAnnotation {
    pub char_idx: usize,
    pub thread: Arc<Thread>,
    pub rendered: Arc<RenderedThread>,
    /// Hidden annotations still follow edits but are not drawn.
    pub hidden: bool,
}

pub type Row = Vec<(String, Style)>;

const FRAME: &str = "▎ ";
const MIN_WIDTH: usize = 16;

/// The rows drawn for an annotation.
pub fn annotation_rows(annotation: &ReviewAnnotation, width: usize) -> Vec<Row> {
    if annotation.hidden {
        Vec::new()
    } else {
        layout_rows(&annotation.rendered, width)
    }
}

/// Wrap a rendered thread into rows of at most `width` columns, each starting with the frame.
///
/// Both the virtual line reservation and the renderer use this function, so the number of
/// rows always matches what is drawn.
pub fn layout_rows(thread: &RenderedThread, width: usize) -> Vec<Row> {
    if width < MIN_WIDTH {
        return Vec::new();
    }
    let text_width = width - str_width(FRAME);
    let mut rows = Vec::new();
    for line in &thread.lines {
        for mut row in wrap_spans(&line.spans, text_width, line.indent.min(text_width / 2)) {
            row.insert(0, (FRAME.to_string(), thread.frame));
            rows.push(row);
        }
    }
    rows
}

pub fn str_width(s: &str) -> usize {
    s.chars().map(|ch| ch.width().unwrap_or(0)).sum()
}

/// Word wrap styled spans to `width` columns. Continuation rows are indented by `indent`
/// columns and words longer than a row are broken.
fn wrap_spans(spans: &[(String, Style)], width: usize, indent: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut row: Row = Vec::new();
    let mut used = 0;
    // Whether `row` has content besides the continuation indent.
    let mut has_content = false;

    fn push(row: &mut Row, text: &str, style: Style) {
        match row.last_mut() {
            Some((last, last_style)) if *last_style == style => last.push_str(text),
            _ => row.push((text.to_string(), style)),
        }
    }
    let new_row = |rows: &mut Vec<Row>, row: &mut Row, used: &mut usize| {
        if let Some((last, _)) = row.last_mut() {
            let trimmed = last.trim_end().len();
            last.truncate(trimmed);
        }
        rows.push(std::mem::take(row));
        if indent > 0 {
            row.push((" ".repeat(indent), Style::default()));
        }
        *used = indent;
    };

    for (text, style) in spans {
        let text = text.replace('\t', "    ");
        for piece in text.split_inclusive(' ') {
            let piece_width = str_width(piece);
            // Trailing spaces may overflow; they are trimmed when the row ends.
            let visible_width = str_width(piece.trim_end());
            if used + visible_width > width && has_content {
                new_row(&mut rows, &mut row, &mut used);
                has_content = false;
                if piece.trim().is_empty() {
                    continue;
                }
            }
            if used + visible_width <= width {
                push(&mut row, piece, *style);
                used += piece_width;
                has_content = true;
                continue;
            }
            // The piece is longer than a row: break it.
            for ch in piece.chars() {
                let ch_width = ch.width().unwrap_or(0);
                if used + ch_width > width && has_content {
                    new_row(&mut rows, &mut row, &mut used);
                }
                let mut buf = [0; 4];
                push(&mut row, ch.encode_utf8(&mut buf), *style);
                used += ch_width;
                has_content = true;
            }
        }
    }
    if let Some((last, _)) = row.last_mut() {
        let trimmed = last.trim_end().len();
        last.truncate(trimmed);
    }
    rows.push(row);
    rows
}

/// The range of annotations anchored on `line`. Annotations must be sorted by `char_idx`.
pub fn annotations_on_line(
    annotations: &[ReviewAnnotation],
    text: RopeSlice,
    line: usize,
) -> std::ops::Range<usize> {
    if line + 1 >= text.len_lines() {
        // Threads are drawn below the newline that ends their line. The last line has none.
        return 0..0;
    }
    let start = text.line_to_char(line);
    let end = text.line_to_char(line + 1);
    let first = annotations.partition_point(|a| a.char_idx < start);
    let last = annotations.partition_point(|a| a.char_idx < end);
    first..last
}

/// Reserves virtual lines below lines with review threads.
pub(crate) struct ReviewThreadLines<'a> {
    doc: &'a Document,
    width: usize,
}

impl<'a> ReviewThreadLines<'a> {
    #[allow(clippy::new_ret_no_self)]
    pub(crate) fn new(doc: &'a Document, width: u16) -> Box<dyn LineAnnotation + 'a> {
        Box::new(ReviewThreadLines {
            doc,
            width: width as usize,
        })
    }
}

impl LineAnnotation for ReviewThreadLines<'_> {
    fn insert_virtual_lines(
        &mut self,
        line_end_char_idx: usize,
        _line_end_visual_pos: Position,
        doc_line: usize,
    ) -> Position {
        let text = self.doc.text().slice(..);
        // Only reserve space after the newline, not at soft wrap points.
        if doc_line + 1 >= text.len_lines() || text.line_to_char(doc_line + 1) != line_end_char_idx
        {
            return Position::new(0, 0);
        }
        let annotations = &self.doc.review_annotations;
        let rows: usize = annotations[annotations_on_line(annotations, text, doc_line)]
            .iter()
            .map(|annotation| annotation_rows(annotation, self.width).len())
            .sum();
        Position::new(rows, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str) -> RenderedLine {
        RenderedLine::new(vec![(text.to_string(), Style::default())])
    }

    fn row_width(row: &Row) -> usize {
        row.iter().map(|(text, _)| str_width(text)).sum()
    }

    #[test]
    fn escapes_round_trip() {
        let body = "first\nsecond \\n literal";
        assert_eq!(unescape_body(&escape_body(body)), body);
        assert_eq!(unescape_body("a\\nb"), "a\nb");
    }

    #[test]
    fn rows_wrap_and_fit_width() {
        let thread = RenderedThread {
            frame: Style::default(),
            lines: vec![
                line("one two three four five six seven eight"),
                line(""),
                line("averyveryveryverylongwordwithoutspaces"),
            ],
        };
        let rows = layout_rows(&thread, 20);
        assert!(rows.len() > 4);
        for row in &rows {
            assert!(row_width(row) <= 20, "{row:?}");
            assert_eq!(row[0].0, FRAME);
        }
        assert!(layout_rows(&thread, 4).is_empty());
    }

    #[test]
    fn continuation_rows_are_indented() {
        let mut item = line("• alpha beta gamma delta epsilon");
        item.indent = 2;
        let thread = RenderedThread {
            frame: Style::default(),
            lines: vec![item],
        };
        let rows = layout_rows(&thread, 18);
        assert!(rows.len() > 1);
        let continuation: String = rows[1].iter().skip(1).map(|(t, _)| t.as_str()).collect();
        assert!(continuation.starts_with("  ") && !continuation.starts_with("   "));
    }

    #[test]
    fn preserves_leading_whitespace() {
        let thread = RenderedThread {
            frame: Style::default(),
            lines: vec![line("    indented code")],
        };
        let rows = layout_rows(&thread, 40);
        let text: String = rows[0].iter().skip(1).map(|(t, _)| t.as_str()).collect();
        assert_eq!(text, "    indented code");
    }
}

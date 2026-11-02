//! Code review mode.
//!
//! `:review` compares the current branch against its base (the pull request base when the
//! branch has a GitHub pull request, otherwise the remote default branch). While a review is
//! active the diff gutter shows the whole branch, files can be marked as viewed, and comments
//! are drawn below the lines they refer to. Comments are drafts until they are submitted to
//! GitHub with `:review-submit` or exported as Markdown with `:review-export`.

use std::{
    collections::HashMap,
    fmt::Write as _,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, bail, Context as _, Result};
use helix_core::{Rope, Selection, Transaction};
use helix_loader::workspace_trust::TrustQuery;
use helix_vcs::ReviewBase;
use helix_view::{
    align_view,
    editor::Action,
    info::Info,
    review::{
        escape_body, new_uid, unescape_body, Comment, FileStatus, PullRequest, RemoteThread,
        ReviewAnnotation, ReviewFile, ReviewSession, Thread, DRAFT_AUTHOR,
    },
    theme::Style,
    Align, DocumentId, Editor,
};
use serde_json::{json, Value};
use tui::text::Span;

use crate::{
    compositor::{self, Compositor},
    job,
    ui::{self, overlay::overlaid, Picker, PickerColumn, Popup, Prompt, PromptEvent},
};

use super::Context;

// --- Processes ------------------------------------------------------------------------------

const COMMAND_TIMEOUT: Duration = Duration::from_secs(180);

async fn run(program: &str, args: &[&str], cwd: &Path, input: Option<String>) -> Result<String> {
    use tokio::io::AsyncWriteExt;

    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        // Never block on credential or confirmation prompts: there is no terminal to answer.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("NO_COLOR", "1")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            anyhow!("`{program}` is not installed or not on PATH")
        } else {
            anyhow!("failed to run `{program}`: {err}")
        }
    })?;
    if let Some(input) = input {
        let mut stdin = child.stdin.take().context("stdin unavailable")?;
        stdin.write_all(input.as_bytes()).await?;
    }
    let output = tokio::time::timeout(COMMAND_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| {
            anyhow!(
                "`{program} {}` timed out; it may still have completed",
                args.iter().take(3).copied().collect::<Vec<_>>().join(" ")
            )
        })??;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let message = [stderr.trim(), stdout.trim()]
            .into_iter()
            .find(|message| !message.is_empty())
            .map(|message| message.lines().take(6).collect::<Vec<_>>().join(" "))
            .unwrap_or_else(|| output.status.to_string());
        let shown: Vec<_> = args.iter().take(3).copied().collect();
        bail!("`{program} {}` failed: {message}", shown.join(" "));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn git(root: &Path, args: &[&str]) -> Result<String> {
    let mut full = vec!["-c", "core.quotepath=false"];
    full.extend_from_slice(args);
    run("git", &full, root, None).await
}

async fn gh(root: &Path, args: &[&str], input: Option<String>) -> Result<String> {
    run("gh", args, root, input).await
}

/// `gh api` arguments that target the pull request's host.
fn gh_api_args<'a>(pr: &'a PullRequest, args: &[&'a str]) -> Vec<&'a str> {
    let mut full = vec!["api", "--hostname", pr.host.as_str()];
    full.extend_from_slice(args);
    full
}

// --- Loading a review -----------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ReviewRequest {
    /// Any directory inside the repository.
    pub dir: PathBuf,
    /// Explicit base revision.
    pub base: Option<String>,
    /// Pull request number or URL.
    pub pr: Option<String>,
    /// Check out the pull request before reviewing it.
    pub checkout: bool,
}

struct Loaded {
    session: ReviewSession,
    notes: Vec<String>,
}

async fn load_review(request: ReviewRequest) -> Result<Loaded> {
    let mut notes = Vec::new();
    let root = git(&request.dir, &["rev-parse", "--show-toplevel"])
        .await
        .context("not inside a Git repository")?;
    let root = std::fs::canonicalize(root.trim()).context("repository root")?;

    if request.checkout {
        let pr = request.pr.as_deref().context("no pull request given")?;
        gh(&root, &["pr", "checkout", pr], None).await.context(
            "checking out the pull request failed (commit or stash local changes first)",
        )?;
    }

    let head_commit = git(&root, &["rev-parse", "HEAD"]).await?.trim().to_string();
    let head_label = match git(&root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await?
        .trim()
    {
        "HEAD" => head_commit[..head_commit.len().min(10)].to_string(),
        branch => branch.to_string(),
    };

    let pr = match &request.pr {
        Some(selector) => Some(fetch_pull_request(&root, Some(selector)).await?),
        // The current branch's pull request, if there is one. Repositories without remotes
        // can not have one, so `gh` is not consulted.
        None if !git(&root, &["remote"]).await?.trim().is_empty() => {
            fetch_pull_request(&root, None).await.ok()
        }
        None => None,
    };

    // The merge base GitHub diffs the pull request against; comments must fall inside it.
    let pr_base = match &pr {
        Some(pr) => match pull_request_base(&root, pr, &mut notes).await {
            Ok(base) => Some(base),
            Err(err) => {
                notes.push(format!("{err:#}"));
                None
            }
        },
        None => None,
    };
    let (base_label, base_commit) = match (&request.base, &pr_base) {
        (Some(base), _) => (base.clone(), merge_base(&root, base).await?),
        (None, Some(pr_base)) => pr_base.clone(),
        (None, None) => default_base(&root).await?,
    };
    let pr_base_commit = pr_base.map(|(_, commit)| commit);

    let files = changed_files(&root, &base_commit).await?;

    let mut threads = Vec::new();
    if let Some(pr) = &pr {
        match fetch_threads(&root, pr).await {
            Ok(remote) => threads = remote,
            Err(err) => notes.push(format!("Could not load review threads: {err:#}")),
        }
        if pr.head_oid != head_commit {
            notes.push(format!(
                "local HEAD differs from PR head {}; push or pull before submitting",
                &pr.head_oid[..pr.head_oid.len().min(8)]
            ));
        }
    }

    let git_dir = git(
        &root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await
    .map(|dir| PathBuf::from(dir.trim()))
    .unwrap_or_else(|_| root.join(".git"));
    // Drafts belong to the branch, so they survive opening a pull request mid-review.
    let key = match (&pr, head_label == head_commit[..head_commit.len().min(10)]) {
        (Some(pr), true) => format!("pr-{}", pr.number),
        _ => sanitize(&head_label),
    };
    let state_file = git_dir.join("helix-review").join(format!("{key}.json"));

    let session = ReviewSession::new(
        root,
        base_label,
        base_commit,
        pr_base_commit,
        head_label,
        head_commit,
        pr,
        files,
        threads,
        state_file,
        request.base,
    );
    Ok(Loaded { session, notes })
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

async fn merge_base(root: &Path, rev: &str) -> Result<String> {
    let commit = git(root, &["merge-base", rev, "HEAD"])
        .await
        .with_context(|| format!("no common history between `{rev}` and HEAD"))?;
    Ok(commit.trim().to_string())
}

async fn default_base(root: &Path) -> Result<(String, String)> {
    let mut candidates = Vec::new();
    if let Ok(head) = git(
        root,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )
    .await
    {
        candidates.push(head.trim().to_string());
    }
    for candidate in [
        "origin/main",
        "origin/master",
        "upstream/main",
        "upstream/master",
        "main",
        "master",
    ] {
        if !candidates.iter().any(|c| c == candidate) {
            candidates.push(candidate.to_string());
        }
    }
    for candidate in candidates {
        if let Ok(commit) = merge_base(root, &candidate).await {
            return Ok((candidate, commit));
        }
    }
    bail!("could not find a base branch; pass one explicitly, for example `:review origin/main`")
}

fn parse_pull_request_url(url: &str) -> Option<(String, String, String)> {
    let rest = url.split_once("://")?.1;
    let mut parts = rest.split('/');
    let host = parts.next()?.to_string();
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.to_string();
    Some((host, owner, repo))
}

async fn fetch_pull_request(root: &Path, selector: Option<&str>) -> Result<PullRequest> {
    let mut args = vec!["pr", "view"];
    if let Some(selector) = selector {
        args.push(selector);
    }
    args.extend([
        "--json",
        "number,title,url,author,body,headRefName,headRefOid,baseRefName,state,isDraft",
    ]);
    let output = gh(root, &args, None).await?;
    let value: Value = serde_json::from_str(&output).context("unexpected `gh pr view` output")?;
    let url = value["url"].as_str().unwrap_or_default().to_string();
    let (host, owner, repo) =
        parse_pull_request_url(&url).context("unexpected pull request URL")?;
    let text = |key: &str| value[key].as_str().unwrap_or_default().to_string();
    Ok(PullRequest {
        number: value["number"].as_u64().context("missing PR number")?,
        host,
        owner,
        repo,
        title: text("title"),
        url,
        author: value["author"]["login"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        body: text("body"),
        head_ref: text("headRefName"),
        head_oid: text("headRefOid"),
        base_ref: text("baseRefName"),
        state: text("state"),
        is_draft: value["isDraft"].as_bool().unwrap_or(false),
    })
}

fn remote_matches(url: &str, pr: &PullRequest) -> bool {
    let url = url
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_lowercase();
    let suffix = format!("{}/{}", pr.owner, pr.repo).to_lowercase();
    url.contains(&pr.host.to_lowercase())
        && (url.ends_with(&format!("/{suffix}")) || url.ends_with(&format!(":{suffix}")))
}

/// The merge base of the pull request, fetching its base branch so it is up to date.
async fn pull_request_base(
    root: &Path,
    pr: &PullRequest,
    notes: &mut Vec<String>,
) -> Result<(String, String)> {
    let remotes = git(root, &["remote", "-v"]).await.unwrap_or_default();
    let remote = remotes.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let name = parts.next()?;
        let url = parts.next()?;
        remote_matches(url, pr).then(|| name.to_string())
    });
    let base = &pr.base_ref;
    match remote {
        Some(remote) => {
            let tracking = format!("{remote}/{base}");
            let refspec = format!("+refs/heads/{base}:refs/remotes/{remote}/{base}");
            if let Err(err) = git(root, &["fetch", "--quiet", "--no-tags", &remote, &refspec]).await
            {
                notes.push(format!("could not update {tracking}: {err:#}"));
            }
            let commit = merge_base(root, &tracking).await?;
            Ok((tracking, commit))
        }
        None => {
            let url = format!("https://{}/{}/{}.git", pr.host, pr.owner, pr.repo);
            let refspec = format!("refs/heads/{base}");
            git(root, &["fetch", "--quiet", "--no-tags", &url, &refspec])
                .await
                .with_context(|| format!("could not fetch {base} from {url}"))?;
            let commit = merge_base(root, "FETCH_HEAD").await?;
            Ok((format!("{}/{}:{base}", pr.owner, pr.repo), commit))
        }
    }
}

async fn changed_files(root: &Path, base: &str) -> Result<Vec<ReviewFile>> {
    let diff_args = ["diff", "--no-ext-diff", "--no-textconv", "-z", "-M"];
    let mut name_status_args = diff_args.to_vec();
    name_status_args.extend(["--name-status", base, "--"]);
    let mut numstat_args = diff_args.to_vec();
    numstat_args.extend(["--numstat", base, "--"]);
    let name_status = git(root, &name_status_args).await?;
    let numstat = git(root, &numstat_args).await?;
    let untracked = git(root, &["ls-files", "-z", "--others", "--exclude-standard"]).await?;

    // numstat: "added\tdeleted\tpath\0", or "added\tdeleted\t\0old\0new\0" for renames.
    let mut stats: HashMap<String, (usize, usize, bool)> = HashMap::new();
    let mut tokens = numstat.split('\0');
    while let Some(token) = tokens.next() {
        let mut fields = token.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let path = if path.is_empty() {
            let _old = tokens.next();
            tokens.next().unwrap_or_default().to_string()
        } else {
            path.to_string()
        };
        let binary = added == "-";
        stats.insert(
            path,
            (
                added.parse().unwrap_or(0),
                deleted.parse().unwrap_or(0),
                binary,
            ),
        );
    }

    let mut files = Vec::new();
    let mut tokens = name_status.split('\0').filter(|token| !token.is_empty());
    while let Some(status) = tokens.next() {
        let (status, old_path) = match status.chars().next() {
            Some('A') => (FileStatus::Added, None),
            Some('D') => (FileStatus::Deleted, None),
            Some('R') | Some('C') => (FileStatus::Renamed, tokens.next().map(PathBuf::from)),
            _ => (FileStatus::Modified, None),
        };
        let Some(path) = tokens.next() else { break };
        let (additions, deletions, binary) = stats.get(path).copied().unwrap_or_default();
        files.push(ReviewFile {
            path: PathBuf::from(path),
            old_path,
            status,
            additions,
            deletions,
            binary,
        });
    }

    for path in untracked.split('\0').filter(|path| !path.is_empty()) {
        let (additions, binary) = match std::fs::read(root.join(path)) {
            Ok(bytes) if bytes.len() <= 4 * 1024 * 1024 && !bytes.contains(&0) => {
                let lines = bytes.iter().filter(|&&b| b == b'\n').count();
                let unterminated = !bytes.is_empty() && !bytes.ends_with(b"\n");
                (lines + unterminated as usize, false)
            }
            _ => (0, true),
        };
        files.push(ReviewFile {
            path: PathBuf::from(path),
            old_path: None,
            status: FileStatus::Untracked,
            additions,
            deletions: 0,
            binary,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

const THREADS_QUERY: &str = r#"
query($owner: String!, $repo: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id isResolved isOutdated path line startLine originalLine originalStartLine diffSide
          comments(first: 100) {
            nodes { databaseId body createdAt author { login } }
          }
        }
      }
    }
  }
}"#;

async fn fetch_threads(root: &Path, pr: &PullRequest) -> Result<Vec<Thread>> {
    let mut threads = Vec::new();
    let mut cursor: Option<String> = None;
    let query = format!("query={THREADS_QUERY}");
    let owner = format!("owner={}", pr.owner);
    let repo = format!("repo={}", pr.repo);
    let number = format!("number={}", pr.number);
    // Pull requests with more than 1000 threads are truncated.
    for _ in 0..10 {
        let cursor_arg = cursor.as_ref().map(|cursor| format!("cursor={cursor}"));
        let mut args = vec![
            "graphql", "-f", &query, "-f", &owner, "-f", &repo, "-F", &number,
        ];
        if let Some(cursor_arg) = &cursor_arg {
            args.extend(["-f", cursor_arg.as_str()]);
        }
        let output = gh(root, &gh_api_args(pr, &args), None).await?;
        let value: Value = serde_json::from_str(&output)?;
        let page = &value["data"]["repository"]["pullRequest"]["reviewThreads"];
        for node in page["nodes"].as_array().into_iter().flatten() {
            if let Some(thread) = parse_thread(node) {
                threads.push(thread);
            }
        }
        if page["pageInfo"]["hasNextPage"].as_bool() != Some(true) {
            break;
        }
        cursor = page["pageInfo"]["endCursor"].as_str().map(String::from);
    }
    Ok(threads)
}

fn parse_thread(node: &Value) -> Option<Thread> {
    let comments: Vec<_> = node["comments"]["nodes"]
        .as_array()?
        .iter()
        .map(|comment| Comment {
            author: comment["author"]["login"]
                .as_str()
                .unwrap_or("ghost")
                .to_string(),
            body: comment["body"]
                .as_str()
                .unwrap_or_default()
                .replace("\r\n", "\n"),
            created_at: comment["createdAt"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            draft: false,
            uid: String::new(),
        })
        .collect();
    let reply_to = node["comments"]["nodes"][0]["databaseId"].as_u64()?;
    let current_line = node["line"].as_u64();
    let line = current_line.or(node["originalLine"].as_u64()).unwrap_or(1) as usize;
    let start_line = node["startLine"]
        .as_u64()
        .or(node["originalStartLine"].as_u64())
        .map_or(line, |start| start as usize)
        .min(line);
    Some(Thread {
        id: 0,
        path: PathBuf::from(node["path"].as_str()?),
        start_line: start_line.max(1),
        line: line.max(1),
        resolved: node["isResolved"].as_bool().unwrap_or(false),
        outdated: node["isOutdated"].as_bool().unwrap_or(false) || current_line.is_none(),
        on_removed_lines: node["diffSide"].as_str() == Some("LEFT"),
        remote: Some(RemoteThread {
            node_id: node["id"].as_str()?.to_string(),
            reply_to,
        }),
        comments,
    })
}

// --- Submission bookkeeping ---

/// Whether a submission is in flight, to prevent posting the same drafts twice.
static SUBMITTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Uids of drafts that were posted, so a review reloaded concurrently drops them as well.
static SUBMITTED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn submitted_drafts() -> Vec<String> {
    SUBMITTED
        .lock()
        .map(|uids| uids.clone())
        .unwrap_or_default()
}

// --- Session state --------------------------------------------------------------------------

fn session(editor: &Editor) -> Result<&ReviewSession> {
    editor
        .review
        .as_deref()
        .ok_or_else(|| anyhow!("No review in progress. Start one with :review or :review-pr"))
}

fn session_mut(editor: &mut Editor) -> Result<&mut ReviewSession> {
    editor
        .review
        .as_deref_mut()
        .ok_or_else(|| anyhow!("No review in progress. Start one with :review or :review-pr"))
}

/// Copy thread positions, which follow edits in open documents, back into the session.
fn pull_anchors(editor: &mut Editor) {
    let Some(review) = editor.review.as_deref_mut() else {
        return;
    };
    for doc in editor.documents.values() {
        let text = doc.text();
        for annotation in &doc.review_annotations {
            let line = text.char_to_line(annotation.char_idx.min(text.len_chars())) + 1;
            if let Some(thread) = review.thread_mut(annotation.thread.id) {
                let delta = line as isize - thread.line as isize;
                if delta != 0 {
                    thread.line = line;
                    thread.start_line = (thread.start_line as isize + delta).max(1) as usize;
                }
            }
        }
    }
}

fn annotations_for(
    review: &ReviewSession,
    relative: &Path,
    text: &Rope,
    theme: &helix_view::Theme,
    loader: &helix_core::syntax::Loader,
) -> Vec<ReviewAnnotation> {
    // Threads are drawn below the newline ending their line, so the last line of a file
    // without a trailing newline shows its threads below the previous line.
    let last_line = text.len_lines().saturating_sub(2);
    let mut annotations: Vec<_> = review
        .threads_in(relative)
        .map(|thread| ReviewAnnotation {
            char_idx: text.line_to_char((thread.line - 1).min(last_line)),
            thread: Arc::new(thread.clone()),
            rendered: Arc::new(ui::review_thread::render_thread(thread, theme, loader)),
            // Hidden threads keep following edits.
            hidden: !review.show_inline,
        })
        .collect();
    annotations.sort_by_key(|annotation| annotation.char_idx);
    annotations
}

/// Keep the positions of a closed document's threads. Unsaved edits are discarded with the
/// document, so their positions are only kept for saved documents.
pub fn document_closed(editor: &mut Editor, doc: &helix_view::Document) {
    if doc.is_modified() || doc.review_annotations.is_empty() {
        return;
    }
    let Some(review) = editor.review.as_deref_mut() else {
        return;
    };
    let text = doc.text();
    for annotation in &doc.review_annotations {
        let line = text.char_to_line(annotation.char_idx.min(text.len_chars())) + 1;
        if let Some(thread) = review.thread_mut(annotation.thread.id) {
            let delta = line as isize - thread.line as isize;
            thread.line = line;
            thread.start_line = (thread.start_line as isize + delta).max(1) as usize;
        }
    }
    if let Err(err) = review.save() {
        editor.set_error(format!("Could not save review state: {err:#}"));
    }
}

/// Persist thread positions once the edits that moved them are saved.
///
/// Viewed marks are reset by changes made outside the editor (for example new commits
/// pulled from the pull request), not by saving your own edits, so a viewed file stays
/// viewed when it is saved.
pub fn document_saved(editor: &mut Editor, path: &Path) {
    let Some(review) = editor.review.as_deref_mut() else {
        return;
    };
    if let Some(relative) = review.relative_path(path) {
        if review.is_viewed(&relative) {
            review.set_viewed(&relative, true);
        }
    }
    commit(editor);
}

/// Update the inline threads of one document.
pub fn sync_document(editor: &mut Editor, doc_id: DocumentId) {
    let Some(doc) = editor.documents.get_mut(&doc_id) else {
        return;
    };
    let loader = editor.syn_loader.load();
    let annotations = match (editor.review.as_deref(), doc.path()) {
        (Some(review), Some(path)) => match review.relative_path(path) {
            Some(relative) => {
                annotations_for(review, &relative, doc.text(), &editor.theme, &loader)
            }
            None => Vec::new(),
        },
        _ => Vec::new(),
    };
    doc.review_annotations = annotations;
}

fn sync_all(editor: &mut Editor) {
    let ids: Vec<_> = editor.documents.keys().copied().collect();
    for id in ids {
        sync_document(editor, id);
    }
}

/// Redraw threads and persist drafts after the session changed.
fn commit(editor: &mut Editor) {
    pull_anchors(editor);
    sync_all(editor);
    if let Some(review) = editor.review.as_deref() {
        if let Err(err) = review.save() {
            editor.set_error(format!("Could not save review state: {err:#}"));
        }
    }
}

/// Recompute the diff gutter of every open document, for example after the review base changed.
fn refresh_diff_bases(editor: &Editor) {
    use std::sync::atomic::{AtomicU64, Ordering};
    // Only the latest refresh may apply, in case an earlier one finishes last.
    static LATEST: AtomicU64 = AtomicU64::new(0);
    let generation = LATEST.fetch_add(1, Ordering::SeqCst) + 1;
    let providers = editor.diff_providers.clone();
    let docs: Vec<_> = editor
        .documents()
        .filter_map(|doc| {
            let path = doc.path()?.to_path_buf();
            let trust = editor
                .workspace_trust
                .query(doc.workspace_root(), TrustQuery::Git)
                .is_trusted();
            Some((doc.id(), path, trust))
        })
        .collect();
    tokio::spawn(async move {
        let Ok(bases) = tokio::task::spawn_blocking(move || {
            docs.into_iter()
                .map(|(id, path, trust)| {
                    let base = providers.get_diff_base(&path, trust);
                    (id, path, base)
                })
                .collect::<Vec<_>>()
        })
        .await
        else {
            return;
        };
        job::dispatch(move |editor, _| {
            if LATEST.load(Ordering::SeqCst) != generation {
                return;
            }
            for (id, path, base) in bases {
                if let Some(doc) = editor.document_mut(id) {
                    if doc.path() == Some(path.as_path()) {
                        match base {
                            Some(base) => doc.set_diff_base(base),
                            None => doc.clear_diff_base(),
                        }
                    }
                }
            }
        })
        .await;
    });
}

/// Directory to discover the repository from: the current file's directory, or the cwd.
fn discovery_dir(editor: &Editor) -> PathBuf {
    let (_, doc) = current_ref!(editor);
    doc.path()
        .and_then(|path| path.parent())
        .filter(|dir| dir.is_dir())
        .map(Path::to_path_buf)
        .unwrap_or_else(helix_stdx::env::current_working_dir)
}

/// Load a review in the background and activate it. Replaces any active review.
pub fn start(editor: &mut Editor, request: ReviewRequest, open_files: bool) {
    pull_anchors(editor);
    if let Some(review) = editor.review.as_deref() {
        if let Err(err) = review.save() {
            editor.set_error(format!("Could not save review state: {err:#}"));
            return;
        }
    }
    editor.set_status(if request.checkout {
        "Checking out pull request…"
    } else {
        "Loading review…"
    });
    tokio::spawn(async move {
        let result = load_review(request).await;
        job::dispatch(move |editor, compositor| activate(editor, compositor, result, open_files))
            .await;
    });
}

fn activate(
    editor: &mut Editor,
    compositor: &mut Compositor,
    result: Result<Loaded>,
    open_files: bool,
) {
    let Loaded { mut session, notes } = match result {
        Ok(loaded) => loaded,
        Err(err) => {
            editor.set_error(format!("Review: {err:#}"));
            return;
        }
    };
    // Carry over drafts and positions that changed while the review was loading.
    pull_anchors(editor);
    session.remove_drafts(&submitted_drafts());
    if let Some(previous) = editor.review.take() {
        session.show_inline = previous.show_inline;
        if previous.root == session.root {
            session.merge_drafts(&previous);
        }
    }
    let renames = session
        .files
        .iter()
        .filter_map(|file| {
            let old = file.old_path.as_ref()?;
            Some((
                session.absolute_path(&file.path),
                session.absolute_path(old),
            ))
        })
        .collect();
    editor.diff_providers.set_review_base(Some(ReviewBase {
        root: session.root.clone(),
        commit: session.base_commit.clone(),
        renames,
    }));
    let mut status = format!(
        "Reviewing {} against {}: {} file{}",
        session.head_label,
        session.base_label,
        session.files.len(),
        if session.files.len() == 1 { "" } else { "s" },
    );
    if let Some(pr) = &session.pr {
        let _ = write!(
            status,
            ", {} open thread(s) on #{}",
            session.open_thread_count(),
            pr.number
        );
    }
    let drafts = session.draft_count();
    if drafts > 0 {
        let _ = write!(status, ", {drafts} draft(s)");
    }
    let empty = session.files.is_empty();
    editor.review = Some(Box::new(session));
    sync_all(editor);
    refresh_diff_bases(editor);

    if notes.is_empty() {
        editor.set_status(status);
    } else {
        editor.set_error(format!("{status} ({})", notes.join("; ")));
    }
    if open_files && !empty {
        if let Some(picker) = files_picker(editor) {
            compositor.push(Box::new(overlaid(picker)));
        }
    }
}

fn close(editor: &mut Editor) -> Result<()> {
    pull_anchors(editor);
    // Keep the review open if its drafts can not be saved.
    session(editor)?
        .save()
        .context("could not save review state; the review stays open")?;
    let review = editor.review.take().context("no review")?;
    editor.diff_providers.set_review_base(None);
    sync_all(editor);
    refresh_diff_bases(editor);
    let drafts = review.draft_count();
    editor.set_status(if drafts > 0 {
        format!("Review closed; {drafts} draft(s) kept for next time")
    } else {
        "Review closed".to_string()
    });
    Ok(())
}

fn refresh(editor: &mut Editor) -> Result<()> {
    let review = session(editor)?;
    let request = ReviewRequest {
        dir: review.root.clone(),
        base: review.requested_base.clone(),
        pr: review.pr.as_ref().map(|pr| pr.number.to_string()),
        checkout: false,
    };
    start(editor, request, false);
    Ok(())
}

// --- Navigation -----------------------------------------------------------------------------

/// The review-relative path of the current document and the cursor's line (1-based).
fn current_location(editor: &Editor) -> Result<(PathBuf, usize)> {
    let review = session(editor)?;
    let (view, doc) = current_ref!(editor);
    let path = doc
        .path()
        .ok_or_else(|| anyhow!("The current buffer has no file"))?;
    let relative = review
        .relative_path(path)
        .ok_or_else(|| anyhow!("The current file is outside the reviewed repository"))?;
    let text = doc.text().slice(..);
    let line = text.char_to_line(doc.selection(view.id).primary().cursor(text)) + 1;
    Ok((relative, line))
}

/// Open `relative` from the review root and put the cursor on `line` (1-based).
fn open_at(
    editor: &mut Editor,
    relative: &Path,
    line: Option<usize>,
    action: Action,
) -> Result<()> {
    let path = session(editor)?.absolute_path(relative);
    if !path.exists() {
        return open_base_version(editor, relative, action);
    }
    {
        let (view, doc) = current!(editor);
        let jump = (doc.id(), doc.selection(view.id).clone());
        view.push_jump(doc, jump);
    }
    editor
        .open(&path, action)
        .with_context(|| format!("unable to open {}", path.display()))?;
    if let Some(line) = line {
        let (view, doc) = current!(editor);
        let text = doc.text();
        let pos = text.line_to_char((line - 1).min(text.len_lines().saturating_sub(1)));
        doc.set_selection(view.id, Selection::point(pos));
        align_view(doc, view, Align::Center);
    }
    Ok(())
}

/// Show the base version of a file deleted by the reviewed changes in a read-only scratch
/// buffer.
fn open_base_version(editor: &mut Editor, relative: &Path, action: Action) -> Result<()> {
    let review = session(editor)?;
    let root = review.root.clone();
    let spec = format!("{}:{}", review.base_commit, relative.display());
    let relative = relative.to_path_buf();
    tokio::spawn(async move {
        let result = git(&root, &["show", &spec]).await;
        job::dispatch(move |editor, _| {
            let contents = match result {
                Ok(contents) => contents,
                Err(err) => {
                    editor.set_error(format!("{err:#}"));
                    return;
                }
            };
            let doc_id = editor.new_file(action);
            let loader = editor.syn_loader.load();
            let view = view_mut!(editor);
            let doc = doc_mut!(editor, &doc_id);
            doc.ensure_view_init(view.id);
            let transaction =
                Transaction::insert(doc.text(), doc.selection(view.id), contents.into())
                    .with_selection(Selection::point(0));
            doc.apply(&transaction, view.id);
            doc.reset_modified();
            if let Some(language) = loader.language_for_filename(&relative) {
                doc.set_language(Some(loader.language(language).config().clone()), &loader);
            }
            doc.readonly = true;
            editor.set_status(format!(
                "{} was deleted; showing the base version (read-only)",
                relative.display()
            ));
        })
        .await;
    });
    Ok(())
}

fn goto_file(editor: &mut Editor, forward: bool) -> Result<()> {
    let review = session(editor)?;
    if review.files.is_empty() {
        bail!("No changed files in this review");
    }
    let current = current_location(editor)
        .ok()
        .and_then(|(path, _)| review.file_index(&path));
    let len = review.files.len();
    let index = match (current, forward) {
        (Some(i), true) => (i + 1) % len,
        (Some(i), false) => (i + len - 1) % len,
        (None, true) => 0,
        (None, false) => len - 1,
    };
    let path = review.files[index].path.clone();
    let line = first_change_line(editor, &path);
    open_at(editor, &path, line, Action::Replace)?;
    let review = session(editor)?;
    editor.set_status(format!(
        "File {}/{}: {}{}",
        index + 1,
        len,
        path.display(),
        if review.is_viewed(&path) { " ✓" } else { "" }
    ));
    Ok(())
}

/// Line of the first change in an already open document, so jumping to a file lands on its
/// first hunk.
fn first_change_line(editor: &Editor, relative: &Path) -> Option<usize> {
    let review = editor.review.as_deref()?;
    let doc = editor.document_by_path(review.absolute_path(relative))?;
    let diff = doc.diff_handle()?.load();
    (!diff.is_empty()).then(|| diff.nth_hunk(0).after.start as usize + 1)
}

fn goto_thread(editor: &mut Editor, forward: bool) -> Result<()> {
    pull_anchors(editor);
    let (path, line) = current_location(editor)?;
    let review = session(editor)?;
    let mut lines: Vec<_> = review.threads_in(&path).map(|t| t.start_line).collect();
    lines.sort_unstable();
    lines.dedup();
    let target = if forward {
        lines.into_iter().find(|&l| l > line)
    } else {
        lines.into_iter().rev().find(|&l| l < line)
    };
    let Some(target) = target else {
        bail!(
            "No {} comment in this file",
            if forward { "next" } else { "previous" }
        );
    };
    open_at(editor, &path, Some(target), Action::Replace)
}

/// Toggle viewed for the current file. Marking a file with unsaved edits as viewed saves it
/// first (like `:write`), so "edit, mark viewed, next file" is a single step.
fn toggle_viewed(cx: &mut compositor::Context) -> Result<()> {
    let (path, _) = current_location(cx.editor)?;
    let marking = !session(cx.editor)?.is_viewed(&path);
    let saved = marking && doc!(cx.editor).is_modified();
    if saved {
        super::typed::write_impl(
            cx,
            None,
            super::typed::WriteOptions {
                force: false,
                auto_format: true,
                code_actions: true,
            },
        )?;
    }
    toggle_viewed_impl(cx.editor, saved)
}

fn toggle_viewed_impl(editor: &mut Editor, saved: bool) -> Result<()> {
    let (path, _) = current_location(editor)?;
    let review = session_mut(editor)?;
    if review.file(&path).is_none() {
        bail!("{} is not changed in this review", path.display());
    }
    let viewed = !review.is_viewed(&path);
    review.set_viewed(&path, viewed);
    let (count, total) = (review.viewed_count(), review.files.len());
    let next = viewed.then(|| next_unviewed(review, &path)).flatten();
    commit(editor);
    if !viewed {
        editor.set_status(format!(
            "Marked {} unviewed ({count}/{total})",
            path.display()
        ));
        return Ok(());
    }
    match next {
        Some(next) => {
            let line = first_change_line(editor, &next);
            open_at(editor, &next, line, Action::Replace)?;
            editor.set_status(format!(
                "{}Viewed {count}/{total}; next: {}",
                if saved { "Saved. " } else { "" },
                next.display()
            ));
        }
        None => editor.set_status(format!(
            "{}All {total} files viewed",
            if saved { "Saved. " } else { "" }
        )),
    }
    Ok(())
}

fn next_unviewed(review: &ReviewSession, after: &Path) -> Option<PathBuf> {
    let start = review.file_index(after).map_or(0, |i| i + 1);
    let len = review.files.len();
    (0..len)
        .map(|offset| &review.files[(start + offset) % len])
        .find(|file| !review.is_viewed(&file.path) && file.path != after)
        .map(|file| file.path.clone())
}

// --- Comments -------------------------------------------------------------------------------

/// The lines (1-based) covered by the primary selection.
fn selected_lines(editor: &Editor) -> (usize, usize) {
    let (view, doc) = current_ref!(editor);
    let text = doc.text().slice(..);
    let (start, end) = doc.selection(view.id).primary().line_range(text);
    (start + 1, end + 1)
}

fn add_comment(editor: &mut Editor, body: &str) -> Result<()> {
    let body = unescape_body(body);
    if body.is_empty() {
        bail!("Comment is empty");
    }
    let (path, _) = current_location(editor)?;
    let (start, end) = selected_lines(editor);
    pull_anchors(editor);
    let review = session_mut(editor)?;
    let id = review.add_draft(path, start, end, body);
    let label = review
        .thread(id)
        .map(Thread::line_label)
        .unwrap_or_default();
    let drafts = review.draft_count();
    commit(editor);
    editor.set_status(format!("Draft comment on {label} ({drafts} draft(s))"));
    Ok(())
}

fn thread_at_cursor(editor: &mut Editor) -> Result<Thread> {
    pull_anchors(editor);
    let (path, line) = current_location(editor)?;
    session(editor)?
        .thread_at(&path, line)
        .cloned()
        .ok_or_else(|| anyhow!("No comment on this line"))
}

fn reply(editor: &mut Editor, body: &str) -> Result<()> {
    let body = unescape_body(body);
    if body.is_empty() {
        bail!("Reply is empty");
    }
    let thread = thread_at_cursor(editor)?;
    if thread.is_draft() {
        bail!("This is your draft; edit it with :review-edit instead");
    }
    let review = session_mut(editor)?;
    if let Some(thread) = review.thread_mut(thread.id) {
        thread.comments.push(Comment {
            author: DRAFT_AUTHOR.to_string(),
            body,
            created_at: String::new(),
            draft: true,
            uid: new_uid(),
        });
    }
    commit(editor);
    editor.set_status("Draft reply added; it is posted with :review-submit");
    Ok(())
}

/// The last draft comment of a thread.
fn last_draft(thread: &Thread) -> Option<usize> {
    thread.comments.iter().rposition(|comment| comment.draft)
}

fn edit(editor: &mut Editor, body: &str) -> Result<()> {
    let body = unescape_body(body);
    let thread = thread_at_cursor(editor)?;
    let index = last_draft(&thread).ok_or_else(|| anyhow!("Only draft comments can be edited"))?;
    if body.is_empty() {
        return delete(editor);
    }
    let review = session_mut(editor)?;
    if let Some(thread) = review.thread_mut(thread.id) {
        thread.comments[index].body = body;
    }
    commit(editor);
    editor.set_status("Draft updated");
    Ok(())
}

fn delete(editor: &mut Editor) -> Result<()> {
    let thread = thread_at_cursor(editor)?;
    let index = last_draft(&thread).ok_or_else(|| anyhow!("Only draft comments can be deleted"))?;
    let review = session_mut(editor)?;
    if let Some(t) = review.thread_mut(thread.id) {
        t.comments.remove(index);
    }
    review
        .threads
        .retain(|t| !(t.id == thread.id && t.comments.is_empty()));
    commit(editor);
    editor.set_status("Draft deleted");
    Ok(())
}

fn toggle_resolved(editor: &mut Editor) -> Result<()> {
    let thread = thread_at_cursor(editor)?;
    let Some(remote) = thread.remote.clone() else {
        bail!("Draft comments can not be resolved; delete them with :review-delete");
    };
    let review = session(editor)?;
    let pr = review.pr.clone().context("no pull request")?;
    let root = review.root.clone();
    let resolve = !thread.resolved;
    editor.set_status(if resolve {
        "Resolving thread…"
    } else {
        "Unresolving thread…"
    });
    let mutation = if resolve {
        "resolveReviewThread"
    } else {
        "unresolveReviewThread"
    };
    let query = format!(
        "query=mutation($id: ID!) {{ {mutation}(input: {{threadId: $id}}) {{ thread {{ isResolved }} }} }}"
    );
    tokio::spawn(async move {
        let id = format!("id={}", remote.node_id);
        let result = gh(
            &root,
            &gh_api_args(&pr, &["graphql", "-f", &query, "-f", &id]),
            None,
        )
        .await;
        job::dispatch(move |editor, _| match result {
            Ok(_) => {
                // Match by GitHub id: the review may have been reloaded meanwhile.
                if let Some(review) = editor.review.as_deref_mut() {
                    for t in &mut review.threads {
                        if t.remote
                            .as_ref()
                            .is_some_and(|r| r.node_id == remote.node_id)
                        {
                            t.resolved = resolve;
                        }
                    }
                }
                commit(editor);
                editor.set_status(if resolve {
                    "Thread resolved"
                } else {
                    "Thread unresolved"
                });
            }
            Err(err) => editor.set_error(format!("{err:#}")),
        })
        .await;
    });
    Ok(())
}

fn toggle_inline(editor: &mut Editor) -> Result<()> {
    let review = session_mut(editor)?;
    review.show_inline = !review.show_inline;
    let shown = review.show_inline;
    pull_anchors(editor);
    sync_all(editor);
    editor.set_status(if shown {
        "Inline comments shown"
    } else {
        "Inline comments hidden"
    });
    Ok(())
}

// --- Export and submit ----------------------------------------------------------------------

fn export_markdown(editor: &mut Editor) -> Result<String> {
    pull_anchors(editor);
    let review = session(editor)?;
    let read_lines = |relative: &Path, start: usize, end: usize| {
        let path = review.absolute_path(relative);
        let text = match editor.document_by_path(&path) {
            Some(doc) => doc.text().clone(),
            None => Rope::from(std::fs::read_to_string(&path).ok()?),
        };
        let last = text.len_lines();
        if start == 0 || start > last {
            return None;
        }
        let end = end.min(last);
        let from = text.line_to_char(start - 1);
        let to = text.line_to_char(end.min(last));
        Some(text.slice(from..to).to_string())
    };
    Ok(review.to_markdown(read_lines))
}

fn export(editor: &mut Editor, path: Option<&str>) -> Result<()> {
    let markdown = export_markdown(editor)?;
    let count = session(editor)?
        .threads
        .iter()
        .filter(|thread| !thread.resolved)
        .count();
    match path {
        Some(path) => {
            let path = helix_stdx::path::expand_tilde(Path::new(path));
            std::fs::write(&path, markdown)
                .with_context(|| format!("could not write {}", path.display()))?;
            editor.set_status(format!("Exported {count} thread(s) to {}", path.display()));
        }
        None => {
            editor.registers.write('+', vec![markdown])?;
            editor.set_status(format!(
                "Copied {count} thread(s) as Markdown to the clipboard"
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewEvent {
    Comment,
    Approve,
    RequestChanges,
}

impl ReviewEvent {
    fn parse(input: &str) -> Option<Self> {
        match input {
            "comment" | "c" => Some(Self::Comment),
            "approve" | "a" => Some(Self::Approve),
            "request-changes" | "request_changes" | "changes" | "r" => Some(Self::RequestChanges),
            _ => None,
        }
    }

    fn api_name(self) -> &'static str {
        match self {
            Self::Comment => "COMMENT",
            Self::Approve => "APPROVE",
            Self::RequestChanges => "REQUEST_CHANGES",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Comment => "comment",
            Self::Approve => "approve",
            Self::RequestChanges => "request changes",
        }
    }
}

struct DraftThread {
    uid: String,
    path: PathBuf,
    start_line: usize,
    line: usize,
    body: String,
}

struct DraftReply {
    uid: String,
    reply_to: u64,
    body: String,
}

#[derive(Default)]
struct Submitted {
    review_url: Option<String>,
    /// Uids of the drafts that were posted.
    posted: Vec<String>,
    moved_to_body: usize,
    errors: Vec<String>,
}

/// New-side line ranges per file in a unified diff, i.e. the lines GitHub accepts comments on.
fn parse_commentable_ranges(diff: &str) -> HashMap<PathBuf, Vec<(usize, usize)>> {
    let mut ranges: HashMap<PathBuf, Vec<(usize, usize)>> = HashMap::new();
    let mut current: Option<PathBuf> = None;
    // File headers only appear between `diff --git` and the first hunk. Inside hunks, an
    // added line starting with "++ " looks like a header and must be ignored.
    let mut in_header = false;
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            in_header = true;
            current = None;
        } else if in_header && line.starts_with("+++ ") {
            let path = line[4..].trim_end_matches('\t');
            current = path.strip_prefix("b/").map(PathBuf::from);
        } else if let Some(header) = line.strip_prefix("@@ ") {
            in_header = false;
            let Some(path) = &current else { continue };
            // @@ -a,b +c,d @@
            let Some(new) = header.split_whitespace().find(|s| s.starts_with('+')) else {
                continue;
            };
            let mut parts = new[1..].split(',');
            let start: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let len: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1);
            if len > 0 && start > 0 {
                ranges
                    .entry(path.clone())
                    .or_default()
                    .push((start, start + len - 1));
            }
        }
    }
    ranges
}

async fn commentable_ranges(
    root: &Path,
    base: &str,
    head: &str,
) -> Result<HashMap<PathBuf, Vec<(usize, usize)>>> {
    let diff = git(
        root,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            // Override `diff.noprefix` and `diff.mnemonicPrefix`.
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "-U3",
            "-M",
            base,
            head,
        ],
    )
    .await?;
    Ok(parse_commentable_ranges(&diff))
}

async fn submit_review(
    root: PathBuf,
    pr: PullRequest,
    base: String,
    event: ReviewEvent,
    mut body: String,
    threads: Vec<DraftThread>,
    replies: Vec<DraftReply>,
) -> Result<Submitted> {
    if !threads.is_empty() {
        // Draft lines refer to the working tree, while GitHub places them in the PR head.
        let head = git(&root, &["rev-parse", "HEAD"]).await?.trim().to_string();
        if head != pr.head_oid {
            bail!(
                "local HEAD {} differs from the PR head {}; push or pull, then :review-refresh",
                &head[..head.len().min(8)],
                &pr.head_oid[..pr.head_oid.len().min(8)]
            );
        }
        let mut args = vec!["status", "--porcelain", "--"];
        let paths: Vec<_> = threads
            .iter()
            .map(|thread| thread.path.to_string_lossy().into_owned())
            .collect();
        args.extend(paths.iter().map(String::as_str));
        let dirty = git(&root, &args).await?;
        if !dirty.trim().is_empty() {
            let files: Vec<_> = dirty.lines().filter_map(|line| line.get(3..)).collect();
            bail!(
                "commented files have uncommitted changes ({}); commit and push them first so \
                 comments land on the right lines",
                files.join(", ")
            );
        }
    }
    let ranges = commentable_ranges(&root, &base, &pr.head_oid).await?;
    let mut submitted = Submitted::default();

    // GitHub only accepts line comments inside the diff. Others go into the review body.
    let mut comments = Vec::new();
    let mut outside = String::new();
    for thread in &threads {
        let inside = ranges.get(&thread.path).is_some_and(|ranges| {
            ranges
                .iter()
                .any(|&(from, to)| from <= thread.start_line && thread.line <= to)
        });
        let path = thread.path.to_string_lossy().replace('\\', "/");
        if inside {
            let mut comment = json!({
                "path": path,
                "body": thread.body,
                "line": thread.line,
                "side": "RIGHT",
            });
            if thread.start_line < thread.line {
                comment["start_line"] = json!(thread.start_line);
                comment["start_side"] = json!("RIGHT");
            }
            comments.push(comment);
        } else {
            let label = if thread.start_line < thread.line {
                format!("L{}-{}", thread.start_line, thread.line)
            } else {
                format!("L{}", thread.line)
            };
            let _ = write!(outside, "\n\n**`{path}` {label}**\n\n{}", thread.body);
            submitted.moved_to_body += 1;
        }
    }
    if !outside.is_empty() {
        body.push_str(&outside);
        body = body.trim().to_string();
    }

    let needs_review = event != ReviewEvent::Comment || !comments.is_empty() || !body.is_empty();
    if !needs_review && replies.is_empty() {
        bail!("Nothing to submit: add comments or a message, or choose approve");
    }
    if event == ReviewEvent::RequestChanges && body.is_empty() {
        bail!("Requesting changes needs a message");
    }

    if needs_review {
        let mut payload = json!({
            "commit_id": pr.head_oid,
            "event": event.api_name(),
            "comments": comments,
        });
        if !body.is_empty() {
            payload["body"] = json!(body);
        }
        let endpoint = format!("repos/{}/{}/pulls/{}/reviews", pr.owner, pr.repo, pr.number);
        let output = gh(
            &root,
            &gh_api_args(&pr, &["--method", "POST", &endpoint, "--input", "-"]),
            Some(payload.to_string()),
        )
        .await?;
        let response: Value = serde_json::from_str(&output).unwrap_or_default();
        submitted.review_url = response["html_url"].as_str().map(String::from);
        submitted
            .posted
            .extend(threads.iter().map(|thread| thread.uid.clone()));
    }

    for reply in replies {
        let endpoint = format!(
            "repos/{}/{}/pulls/{}/comments/{}/replies",
            pr.owner, pr.repo, pr.number, reply.reply_to
        );
        let result = gh(
            &root,
            &gh_api_args(&pr, &["--method", "POST", &endpoint, "--input", "-"]),
            Some(json!({ "body": reply.body }).to_string()),
        )
        .await;
        match result {
            Ok(_) => submitted.posted.push(reply.uid),
            Err(err) => submitted.errors.push(format!("{err:#}")),
        }
    }
    Ok(submitted)
}

fn submit(editor: &mut Editor, event: ReviewEvent, body: &str) -> Result<()> {
    use std::sync::atomic::Ordering;

    pull_anchors(editor);
    let review = session(editor)?;
    let Some(pr) = review.pr.clone() else {
        bail!("This branch has no pull request. Use :review-export to copy the comments instead");
    };
    let threads: Vec<_> = review
        .threads
        .iter()
        .filter(|thread| thread.is_draft())
        .filter_map(|thread| {
            let comment = thread.comments.first()?;
            Some(DraftThread {
                uid: comment.uid.clone(),
                path: thread.path.clone(),
                start_line: thread.start_line,
                line: thread.line,
                body: comment.body.clone(),
            })
        })
        .collect();
    // Unsaved edits move draft lines away from the committed code.
    let unsaved: Vec<_> = threads
        .iter()
        .filter(|thread| {
            editor
                .document_by_path(review.absolute_path(&thread.path))
                .is_some_and(|doc| doc.is_modified())
        })
        .map(|thread| thread.path.display().to_string())
        .collect();
    if !unsaved.is_empty() {
        bail!(
            "Save, commit and push {} before submitting",
            unsaved.join(", ")
        );
    }
    let replies: Vec<_> = review
        .threads
        .iter()
        .filter_map(|thread| Some((thread, thread.remote.as_ref()?)))
        .flat_map(|(thread, remote)| {
            thread
                .comments
                .iter()
                .filter(|comment| comment.draft)
                .map(move |comment| DraftReply {
                    uid: comment.uid.clone(),
                    reply_to: remote.reply_to,
                    body: comment.body.clone(),
                })
        })
        .collect();
    let root = review.root.clone();
    let base = review
        .pr_base_commit
        .clone()
        .unwrap_or_else(|| review.base_commit.clone());
    let body = unescape_body(body);
    if SUBMITTING.swap(true, Ordering::SeqCst) {
        bail!("A review is already being submitted");
    }
    editor.set_status(format!("Submitting review ({})…", event.label()));
    tokio::spawn(async move {
        let result = submit_review(root, pr, base, event, body, threads, replies).await;
        job::dispatch(move |editor, _| {
            SUBMITTING.store(false, Ordering::SeqCst);
            finish_submit(editor, event, result)
        })
        .await;
    });
    Ok(())
}

fn finish_submit(editor: &mut Editor, event: ReviewEvent, result: Result<Submitted>) {
    let submitted = match result {
        Ok(submitted) => submitted,
        Err(err) => {
            editor.set_error(format!("Review not submitted: {err:#}"));
            return;
        }
    };
    if let Ok(mut uids) = SUBMITTED.lock() {
        uids.extend(submitted.posted.iter().cloned());
    }
    pull_anchors(editor);
    if let Some(review) = editor.review.as_deref_mut() {
        review.remove_drafts(&submitted.posted);
    }
    commit(editor);
    let mut status = format!("Review submitted ({})", event.label());
    if submitted.moved_to_body > 0 {
        let _ = write!(
            status,
            "; {} comment(s) outside the diff were added to the review body",
            submitted.moved_to_body
        );
    }
    if let Some(url) = &submitted.review_url {
        let _ = write!(status, ": {url}");
    }
    if submitted.errors.is_empty() {
        editor.set_status(status);
    } else {
        editor.set_error(format!(
            "{status}; failed replies: {}",
            submitted.errors.join("; ")
        ));
    }
    // Load the posted comments as threads.
    let _ = refresh(editor);
}

// --- Pickers and popups ---------------------------------------------------------------------

struct FileRow {
    file: ReviewFile,
    absolute: PathBuf,
    viewed: bool,
    threads: usize,
    drafts: usize,
}

struct FilePickerData {
    added: Style,
    modified: Style,
    deleted: Style,
    renamed: Style,
    viewed: Style,
    drafts: Style,
}

fn files_picker(editor: &mut Editor) -> Option<Picker<FileRow, FilePickerData>> {
    pull_anchors(editor);
    let review = editor.review.as_deref_mut()?;
    review.revalidate_viewed();
    let rows: Vec<_> = review
        .files
        .iter()
        .map(|file| {
            let threads: Vec<_> = review.threads_in(&file.path).collect();
            FileRow {
                file: file.clone(),
                absolute: review.absolute_path(&file.path),
                viewed: review.is_viewed(&file.path),
                threads: threads.iter().filter(|t| !t.resolved).count(),
                drafts: threads.iter().filter(|t| t.has_drafts()).count(),
            }
        })
        .collect();
    let theme = &editor.theme;
    let data = FilePickerData {
        added: theme.get("diff.plus"),
        modified: theme.get("diff.delta"),
        deleted: theme.get("diff.minus"),
        renamed: theme.get("diff.delta.moved"),
        viewed: theme.get("diff.plus"),
        drafts: theme.get("warning"),
    };

    let columns = [
        PickerColumn::new("viewed", |row: &FileRow, data: &FilePickerData| {
            if row.viewed {
                Span::styled("✓", data.viewed).into()
            } else {
                Span::raw(" ").into()
            }
        })
        .without_filtering(),
        PickerColumn::new("status", |row: &FileRow, data: &FilePickerData| {
            let style = match row.file.status {
                FileStatus::Added | FileStatus::Untracked => data.added,
                FileStatus::Modified => data.modified,
                FileStatus::Deleted => data.deleted,
                FileStatus::Renamed => data.renamed,
            };
            Span::styled(row.file.status.label(), style).into()
        }),
        PickerColumn::new("changes", |row: &FileRow, data: &FilePickerData| {
            if row.file.binary {
                return Span::raw("binary").into();
            }
            tui::text::Spans::from(vec![
                Span::styled(format!("+{}", row.file.additions), data.added),
                Span::raw(" "),
                Span::styled(format!("-{}", row.file.deletions), data.deleted),
            ])
            .into()
        })
        .without_filtering(),
        PickerColumn::new("comments", |row: &FileRow, data: &FilePickerData| {
            let mut spans = Vec::new();
            if row.threads > 0 {
                spans.push(Span::raw(format!("{} thread(s)", row.threads)));
            }
            if row.drafts > 0 {
                if !spans.is_empty() {
                    spans.push(Span::raw(" "));
                }
                spans.push(Span::styled(
                    format!("{} draft(s)", row.drafts),
                    data.drafts,
                ));
            }
            tui::text::Spans::from(spans).into()
        })
        .without_filtering(),
        PickerColumn::new("path", |row: &FileRow, _: &FilePickerData| {
            match &row.file.old_path {
                Some(old) => format!("{} → {}", old.display(), row.file.path.display()),
                None => row.file.path.display().to_string(),
            }
            .into()
        }),
    ];

    let picker = Picker::new(columns, 4, rows, data, |cx, row: &FileRow, action| {
        let line = first_change_line(cx.editor, &row.file.path);
        if let Err(err) = open_at(cx.editor, &row.file.path, line, action) {
            cx.editor.set_error(format!("{err:#}"));
        }
    })
    .with_preview(|_editor, row: &FileRow| {
        // Deleted files have no preview.
        row.absolute
            .exists()
            .then(|| (row.absolute.as_path().into(), None))
    });
    Some(picker)
}

struct ThreadRow {
    thread: Thread,
    absolute: PathBuf,
}

struct ThreadPickerData {
    draft: Style,
    resolved: Style,
    open: Style,
}

fn comments_picker(editor: &mut Editor) -> Result<Picker<ThreadRow, ThreadPickerData>> {
    pull_anchors(editor);
    let review = session(editor)?;
    if review.threads.is_empty() {
        bail!("No comments yet. Select lines and press Space v c to comment");
    }
    let rows: Vec<_> = review
        .threads
        .iter()
        .map(|thread| ThreadRow {
            thread: thread.clone(),
            absolute: review.absolute_path(&thread.path),
        })
        .collect();
    let theme = &editor.theme;
    let data = ThreadPickerData {
        draft: theme.get("warning"),
        resolved: theme.get("comment"),
        open: theme.get("info"),
    };
    let columns = [
        PickerColumn::new("state", |row: &ThreadRow, data: &ThreadPickerData| {
            let label = row.thread.state_label();
            let style = match label {
                "draft" | "reply" => data.draft,
                "resolved" | "outdated" => data.resolved,
                _ => data.open,
            };
            Span::styled(label, style).into()
        }),
        PickerColumn::new("location", |row: &ThreadRow, _: &ThreadPickerData| {
            format!("{}:{}", row.thread.path.display(), row.thread.start_line).into()
        }),
        PickerColumn::new("author", |row: &ThreadRow, _: &ThreadPickerData| {
            row.thread
                .comments
                .first()
                .map_or(String::new(), |comment| comment.author.clone())
                .into()
        }),
        PickerColumn::new("comment", |row: &ThreadRow, _: &ThreadPickerData| {
            let last = row.thread.comments.last();
            let text = last.map_or("", |comment| comment.body.lines().next().unwrap_or(""));
            let replies = row.thread.comments.len().saturating_sub(1);
            if replies > 0 {
                format!("{text} (+{replies})").into()
            } else {
                text.to_string().into()
            }
        }),
    ];
    let picker = Picker::new(columns, 3, rows, data, |cx, row: &ThreadRow, action| {
        let thread = &row.thread;
        if let Err(err) = open_at(cx.editor, &thread.path, Some(thread.start_line), action) {
            cx.editor.set_error(format!("{err:#}"));
        }
    })
    .with_preview(|_editor, row: &ThreadRow| {
        let range = (row.thread.start_line - 1, row.thread.line - 1);
        row.absolute
            .exists()
            .then(|| (row.absolute.as_path().into(), Some(range)))
    });
    Ok(picker)
}

/// A popup with the base and current version of the change under the cursor.
fn hunk_popup(editor: &Editor) -> Result<Popup<ui::Markdown>> {
    let (view, doc) = current_ref!(editor);
    let handle = doc
        .diff_handle()
        .ok_or_else(|| anyhow!("No diff for this buffer"))?;
    let diff = handle.load();
    let text = doc.text().slice(..);
    let line = text.char_to_line(doc.selection(view.id).primary().cursor(text)) as u32;
    let index = diff
        .hunk_at(line, true)
        .ok_or_else(|| anyhow!("No change on this line"))?;
    let hunk = diff.nth_hunk(index);
    let base = diff.diff_base();
    let current = diff.doc();

    const MAX_LINES: u32 = 300;
    let mut out = format!(
        "```diff\n@@ -{},{} +{},{} @@\n",
        hunk.before.start + 1,
        hunk.before.len(),
        hunk.after.start + 1,
        hunk.after.len()
    );
    let mut push = |prefix: char, rope: &Rope, lines: std::ops::Range<u32>| {
        for line in lines.clone().take(MAX_LINES as usize) {
            out.push(prefix);
            out.push_str(&rope.line(line as usize).to_string());
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
        if lines.len() > MAX_LINES as usize {
            let _ = writeln!(
                out,
                "{prefix} … {} more lines",
                lines.len() - MAX_LINES as usize
            );
        }
    };
    push('-', base, hunk.before.clone());
    push('+', current, hunk.after.clone());
    out.push_str("```");
    let what = if hunk.before.is_empty() {
        "Added"
    } else if hunk.after.is_empty() {
        "Removed"
    } else {
        "Changed"
    };
    let title = format!("{what} lines · change {} of {}\n\n", index + 1, diff.len());
    let contents = ui::Markdown::new(title + out.as_str(), editor.syn_loader.clone());
    Ok(Popup::new("review-hunk", contents).auto_close(true))
}

fn info_popup(editor: &Editor) -> Result<Popup<ui::Markdown>> {
    let review = session(editor)?;
    let mut out = String::new();
    match &review.pr {
        Some(pr) => {
            let _ = writeln!(out, "## #{} {}\n", pr.number, pr.title);
            let mut state = pr.state.to_lowercase();
            if pr.is_draft {
                state.push_str(", draft");
            }
            let _ = writeln!(out, "{} · by @{} · {state}\n", pr.url, pr.author);
        }
        None => {
            let _ = writeln!(out, "## Local review\n");
        }
    }
    let _ = writeln!(
        out,
        "`{}` against `{}` (merge base `{}`)\n",
        review.head_label,
        review.base_label,
        &review.base_commit[..review.base_commit.len().min(10)]
    );
    let additions: usize = review.files.iter().map(|f| f.additions).sum();
    let deletions: usize = review.files.iter().map(|f| f.deletions).sum();
    let _ = writeln!(
        out,
        "- Files: {} viewed of {} (+{additions} -{deletions})",
        review.viewed_count(),
        review.files.len()
    );
    let resolved = review
        .threads
        .iter()
        .filter(|t| !t.is_draft() && t.resolved)
        .count();
    let _ = writeln!(
        out,
        "- Threads: {} open, {resolved} resolved",
        review.open_thread_count()
    );
    let _ = writeln!(out, "- Drafts: {}\n", review.draft_count());
    if let Some(pr) = &review.pr {
        let body = pr.body.trim();
        if !body.is_empty() {
            let _ = writeln!(out, "---\n\n{}\n", body.replace("\r\n", "\n"));
        }
    }
    out.push_str(
        "---\n\n\
        `Space v` then: `v` files · `n`/`p` next/prev file · `m` toggle viewed · \
        `c` comment on selection · `r` reply · `e` edit · `d` delete · `x` resolve · \
        `C` all comments · `h` show change · `t` toggle inline · `y` copy as Markdown · \
        `s` submit · `R` refresh · `q` close. `]r`/`[r` jump between comments, `]g`/`[g` between changes.",
    );
    let contents = ui::Markdown::new(out, editor.syn_loader.clone());
    Ok(Popup::new("review-info", contents).auto_close(true))
}

// --- Static commands ------------------------------------------------------------------------

fn report(cx: &mut Context, result: Result<()>) {
    if let Err(err) = result {
        cx.editor.set_error(format!("{err:#}"));
    }
}

fn prompt(
    cx: &mut Context,
    label: String,
    initial: Option<String>,
    on_submit: impl Fn(&mut Editor, &str) -> Result<()> + 'static,
) {
    let prompt = Prompt::new(
        label.into(),
        None,
        |_, _| Vec::new(),
        move |cx: &mut compositor::Context, input: &str, event: PromptEvent| {
            if event != PromptEvent::Validate {
                return;
            }
            if let Err(err) = on_submit(cx.editor, input) {
                cx.editor.set_error(format!("{err:#}"));
            }
        },
    );
    let prompt = match initial {
        Some(initial) => prompt.with_line(initial, cx.editor),
        None => prompt,
    };
    cx.push_layer(Box::new(prompt));
}

pub fn review_files_picker(cx: &mut Context) {
    if cx.editor.review.is_none() {
        let dir = discovery_dir(cx.editor);
        start(
            cx.editor,
            ReviewRequest {
                dir,
                ..Default::default()
            },
            true,
        );
        return;
    }
    if session(cx.editor).is_ok_and(|review| review.files.is_empty()) {
        cx.editor.set_status("No changed files in this review");
        return;
    }
    if let Some(picker) = files_picker(cx.editor) {
        cx.push_layer(Box::new(overlaid(picker)));
    }
}

pub fn review_comments_picker(cx: &mut Context) {
    match comments_picker(cx.editor) {
        Ok(picker) => cx.push_layer(Box::new(overlaid(picker))),
        Err(err) => cx.editor.set_error(format!("{err:#}")),
    }
}

pub fn review_comment(cx: &mut Context) {
    if let Err(err) = current_location(cx.editor) {
        return cx.editor.set_error(format!("{err:#}"));
    }
    let (start, end) = selected_lines(cx.editor);
    let label = if start < end {
        format!("comment L{start}-{end}: ")
    } else {
        format!("comment L{start}: ")
    };
    prompt(cx, label, None, add_comment);
}

pub fn review_reply(cx: &mut Context) {
    match thread_at_cursor(cx.editor) {
        Ok(thread) if thread.is_draft() => {
            cx.editor
                .set_error("This is your draft; edit it with Space v e instead");
        }
        Ok(thread) => {
            let author = thread
                .comments
                .first()
                .map_or("thread", |c| c.author.as_str());
            prompt(cx, format!("reply to {author}: "), None, reply);
        }
        Err(err) => cx.editor.set_error(format!("{err:#}")),
    }
}

pub fn review_edit_comment(cx: &mut Context) {
    match thread_at_cursor(cx.editor) {
        Ok(thread) => match last_draft(&thread) {
            Some(index) => {
                let body = escape_body(&thread.comments[index].body);
                prompt(cx, "edit draft: ".to_string(), Some(body), edit);
            }
            None => cx.editor.set_error("Only draft comments can be edited"),
        },
        Err(err) => cx.editor.set_error(format!("{err:#}")),
    }
}

pub fn review_delete_comment(cx: &mut Context) {
    let result = delete(cx.editor);
    report(cx, result);
}

pub fn review_toggle_resolved(cx: &mut Context) {
    let result = toggle_resolved(cx.editor);
    report(cx, result);
}

pub fn review_toggle_viewed(cx: &mut Context) {
    let mut ctx = compositor::Context {
        editor: cx.editor,
        jobs: cx.jobs,
        scroll: None,
    };
    let result = toggle_viewed(&mut ctx);
    report(cx, result);
}

pub fn review_next_file(cx: &mut Context) {
    let result = goto_file(cx.editor, true);
    report(cx, result);
}

pub fn review_prev_file(cx: &mut Context) {
    let result = goto_file(cx.editor, false);
    report(cx, result);
}

pub fn goto_next_review_comment(cx: &mut Context) {
    let result = goto_thread(cx.editor, true);
    report(cx, result);
}

pub fn goto_prev_review_comment(cx: &mut Context) {
    let result = goto_thread(cx.editor, false);
    report(cx, result);
}

pub fn review_show_change(cx: &mut Context) {
    match hunk_popup(cx.editor) {
        Ok(popup) => cx.push_layer(Box::new(popup)),
        Err(err) => cx.editor.set_error(format!("{err:#}")),
    }
}

pub fn review_toggle_inline(cx: &mut Context) {
    let result = toggle_inline(cx.editor);
    report(cx, result);
}

pub fn review_info(cx: &mut Context) {
    match info_popup(cx.editor) {
        Ok(popup) => cx.push_layer(Box::new(popup)),
        Err(err) => cx.editor.set_error(format!("{err:#}")),
    }
}

pub fn review_export(cx: &mut Context) {
    let result = export(cx.editor, None);
    report(cx, result);
}

pub fn review_refresh(cx: &mut Context) {
    let result = refresh(cx.editor);
    report(cx, result);
}

pub fn review_close(cx: &mut Context) {
    let result = close(cx.editor);
    report(cx, result);
}

pub fn review_submit(cx: &mut Context) {
    let review = match session(cx.editor) {
        Ok(review) => review,
        Err(err) => return cx.editor.set_error(format!("{err:#}")),
    };
    if review.pr.is_none() {
        return cx.editor.set_error(
            "This branch has no pull request. Press Space v y to copy the comments as Markdown",
        );
    }
    let drafts = review.draft_count();
    cx.editor.autoinfo = Some(Info::new(
        format!("Submit review ({drafts} draft(s))"),
        &[("c", "Comment"), ("a", "Approve"), ("r", "Request changes")],
    ));
    cx.on_next_key(move |cx, event| {
        cx.editor.autoinfo = None;
        let Some(event) = event
            .char()
            .and_then(|ch| ReviewEvent::parse(&ch.to_string()))
        else {
            return;
        };
        let label = format!("{} message (optional): ", event.label());
        prompt(cx, label, None, move |editor, body| {
            submit(editor, event, body)
        });
    });
}

// --- Typed commands -------------------------------------------------------------------------

pub mod typed {
    use super::*;
    use helix_core::command_line::Args;

    fn validate(event: PromptEvent) -> bool {
        event == PromptEvent::Validate
    }

    pub fn review(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> Result<()> {
        if !validate(event) {
            return Ok(());
        }
        let dir = discovery_dir(cx.editor);
        start(
            cx.editor,
            ReviewRequest {
                dir,
                base: args.first().map(|base| base.to_string()),
                ..Default::default()
            },
            true,
        );
        Ok(())
    }

    pub fn review_pr(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> Result<()> {
        if !validate(event) {
            return Ok(());
        }
        let dir = discovery_dir(cx.editor);
        start(
            cx.editor,
            ReviewRequest {
                dir,
                pr: Some(args[0].to_string()),
                checkout: !args.has_flag("no-checkout"),
                ..Default::default()
            },
            true,
        );
        Ok(())
    }

    pub fn review_close(
        cx: &mut compositor::Context,
        _args: Args,
        event: PromptEvent,
    ) -> Result<()> {
        if validate(event) {
            close(cx.editor)?;
        }
        Ok(())
    }

    pub fn review_refresh(
        cx: &mut compositor::Context,
        _args: Args,
        event: PromptEvent,
    ) -> Result<()> {
        if validate(event) {
            refresh(cx.editor)?;
        }
        Ok(())
    }

    pub fn review_files(
        cx: &mut compositor::Context,
        _args: Args,
        event: PromptEvent,
    ) -> Result<()> {
        if !validate(event) {
            return Ok(());
        }
        session(cx.editor)?;
        push(cx, |editor| {
            let picker = files_picker(editor).context("no review")?;
            Ok(Box::new(overlaid(picker)))
        });
        Ok(())
    }

    pub fn review_comments(
        cx: &mut compositor::Context,
        _args: Args,
        event: PromptEvent,
    ) -> Result<()> {
        if !validate(event) {
            return Ok(());
        }
        session(cx.editor)?;
        push(cx, |editor| {
            Ok(Box::new(overlaid(comments_picker(editor)?)))
        });
        Ok(())
    }

    type Build = fn(&mut Editor) -> Result<Box<dyn compositor::Component>>;

    /// Typed commands can not push layers directly; build the component in a callback.
    fn push(cx: &mut compositor::Context, build: Build) {
        let callback = async move {
            let call: job::Callback = job::Callback::EditorCompositor(Box::new(
                move |editor: &mut Editor, compositor: &mut Compositor| match build(editor) {
                    Ok(component) => compositor.push(component),
                    Err(err) => editor.set_error(format!("{err:#}")),
                },
            ));
            Ok(call)
        };
        cx.jobs.callback(callback);
    }

    pub fn review_comment(
        cx: &mut compositor::Context,
        args: Args,
        event: PromptEvent,
    ) -> Result<()> {
        if validate(event) {
            add_comment(cx.editor, &args[0])?;
        }
        Ok(())
    }

    pub fn review_reply(
        cx: &mut compositor::Context,
        args: Args,
        event: PromptEvent,
    ) -> Result<()> {
        if validate(event) {
            reply(cx.editor, &args[0])?;
        }
        Ok(())
    }

    pub fn review_edit(cx: &mut compositor::Context, args: Args, event: PromptEvent) -> Result<()> {
        if validate(event) {
            edit(cx.editor, &args[0])?;
        }
        Ok(())
    }

    pub fn review_delete(
        cx: &mut compositor::Context,
        _args: Args,
        event: PromptEvent,
    ) -> Result<()> {
        if validate(event) {
            delete(cx.editor)?;
        }
        Ok(())
    }

    pub fn review_resolve(
        cx: &mut compositor::Context,
        _args: Args,
        event: PromptEvent,
    ) -> Result<()> {
        if validate(event) {
            toggle_resolved(cx.editor)?;
        }
        Ok(())
    }

    pub fn review_viewed(
        cx: &mut compositor::Context,
        _args: Args,
        event: PromptEvent,
    ) -> Result<()> {
        if validate(event) {
            toggle_viewed(cx)?;
        }
        Ok(())
    }

    pub fn review_export(
        cx: &mut compositor::Context,
        args: Args,
        event: PromptEvent,
    ) -> Result<()> {
        if validate(event) {
            export(cx.editor, args.first())?;
        }
        Ok(())
    }

    pub fn review_submit(
        cx: &mut compositor::Context,
        args: Args,
        event: PromptEvent,
    ) -> Result<()> {
        if !validate(event) {
            return Ok(());
        }
        let review_event = match args.first() {
            None => ReviewEvent::Comment,
            Some(name) => ReviewEvent::parse(name).ok_or_else(|| {
                anyhow!("unknown review action `{name}`; use comment, approve or request-changes")
            })?,
        };
        let body = args.get(1).unwrap_or_default();
        submit(cx.editor, review_event, body)
    }

    pub fn review_info(
        cx: &mut compositor::Context,
        _args: Args,
        event: PromptEvent,
    ) -> Result<()> {
        if !validate(event) {
            return Ok(());
        }
        session(cx.editor)?;
        push(cx, |editor| Ok(Box::new(info_popup(editor)?)));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr() -> PullRequest {
        PullRequest {
            number: 1,
            host: "github.com".into(),
            owner: "Owner".into(),
            repo: "repo".into(),
            title: String::new(),
            url: String::new(),
            author: String::new(),
            body: String::new(),
            head_ref: String::new(),
            head_oid: String::new(),
            base_ref: "main".into(),
            state: String::new(),
            is_draft: false,
        }
    }

    #[test]
    fn matches_remotes() {
        let pr = pr();
        assert!(remote_matches("git@github.com:owner/repo.git", &pr));
        assert!(remote_matches("https://github.com/Owner/repo", &pr));
        assert!(remote_matches("ssh://git@github.com/owner/repo.git", &pr));
        assert!(!remote_matches("git@github.com:owner/repo2.git", &pr));
        assert!(!remote_matches("git@github.com:other/repo.git", &pr));
    }

    #[test]
    fn parses_pull_request_urls() {
        assert_eq!(
            parse_pull_request_url("https://github.com/helix-editor/helix/pull/1"),
            Some(("github.com".into(), "helix-editor".into(), "helix".into()))
        );
    }

    #[test]
    fn parses_review_events() {
        assert_eq!(ReviewEvent::parse("a"), Some(ReviewEvent::Approve));
        assert_eq!(
            ReviewEvent::parse("request-changes"),
            Some(ReviewEvent::RequestChanges)
        );
        assert_eq!(ReviewEvent::parse("x"), None);
    }

    #[test]
    fn parses_commentable_ranges() {
        let diff = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,4 @@\n x\n+++ y\n x\n x\n@@ -20,0 +21,2 @@ fn f()\n+a\n+b\ndiff --git a/gone.rs b/gone.rs\n--- a/gone.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\n";
        let ranges = parse_commentable_ranges(diff);
        assert_eq!(ranges[Path::new("a.rs")], [(1, 4), (21, 22)]);
        assert_eq!(ranges.len(), 1);
    }

    #[test]
    fn parses_threads() {
        let node = json!({
            "id": "T1", "isResolved": false, "isOutdated": false, "path": "src/a.rs",
            "line": 12, "startLine": 10, "originalLine": 11, "originalStartLine": null,
            "diffSide": "RIGHT",
            "comments": { "nodes": [
                { "databaseId": 5, "body": "a\r\nb", "createdAt": "", "author": { "login": "x" } }
            ] }
        });
        let thread = parse_thread(&node).unwrap();
        assert_eq!((thread.start_line, thread.line), (10, 12));
        assert_eq!(thread.comments[0].body, "a\nb");
        assert_eq!(thread.remote.unwrap().reply_to, 5);

        let mut outdated = node.clone();
        outdated["line"] = Value::Null;
        outdated["startLine"] = Value::Null;
        let thread = parse_thread(&outdated).unwrap();
        assert!(thread.outdated);
        assert_eq!(thread.line, 11);
    }
}

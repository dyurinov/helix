use std::{
    path::Path,
    time::{Duration, Instant},
};

use helix_term::application::Application;
use helix_view::{doc, input::parse_macro};
use tokio_stream::wrappers::UnboundedReceiverStream;

#[cfg(windows)]
use crossterm::event::{Event, KeyEvent};
#[cfg(not(windows))]
use termina::event::{Event, KeyEvent};

use super::*;

fn git(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// Send `keys` and run the event loop until `done` holds. Review commands run Git in the
/// background, so a single idle period is not enough.
async fn run_until(
    app: &mut Application,
    keys: &str,
    done: impl Fn(&Application) -> bool,
) -> anyhow::Result<()> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut stream = UnboundedReceiverStream::new(rx);
    for key in parse_macro(keys)? {
        tx.send(Ok(Event::Key(KeyEvent::from(key))))?;
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        // The idle timer fires once; re-arm it so the loop returns while waiting for jobs.
        app.editor.reset_idle_timer();
        app.event_loop_until_idle(&mut stream).await;
        if done(app) {
            return Ok(());
        }
        if Instant::now() > deadline {
            anyhow::bail!("timed out, status: {:?}", app.editor.get_status());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(feature = "git")]
#[tokio::test(flavor = "multi_thread")]
async fn review_lifecycle() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = std::fs::canonicalize(dir.path())?;
    let a = root.join("a.txt");
    git(&root, &["init", "--quiet"]);
    std::fs::write(&a, "one\ntwo\nthree\n")?;
    std::fs::write(root.join("old.txt"), "renamed\ncontent\nhere\n")?;
    git(&root, &["add", "."]);
    git(&root, &["commit", "--quiet", "-m", "base"]);
    std::fs::write(&a, "one\nTWO\nthree\n")?;
    git(&root, &["mv", "old.txt", "new.txt"]);
    git(&root, &["commit", "--quiet", "-am", "change"]);
    std::fs::write(root.join("b.txt"), "new\nfile\n")?;

    let mut app = AppBuilder::new().with_file(&a, None).build()?;

    // Start a review against the first commit.
    run_until(&mut app, ":review HEAD~1<ret>", |app| {
        app.editor.review.is_some()
    })
    .await?;
    {
        let review = app.editor.review.as_deref().unwrap();
        let files: Vec<_> = review
            .files
            .iter()
            .map(|file| (file.path.display().to_string(), file.status.label()))
            .collect();
        assert_eq!(
            files,
            [
                ("a.txt".into(), "M"),
                ("b.txt".into(), "?"),
                ("new.txt".into(), "R")
            ]
        );
        assert_eq!(
            (review.files[0].additions, review.files[0].deletions),
            (1, 1)
        );
        // The gutter diffs against the review base instead of HEAD.
        let base = app.editor.diff_providers.get_diff_base(&a, true).unwrap();
        assert_eq!(base, b"one\ntwo\nthree\n");
        let new_file = app
            .editor
            .diff_providers
            .get_diff_base(&root.join("b.txt"), true)
            .unwrap();
        assert!(new_file.is_empty());
        // Renamed files are diffed against their old path.
        let renamed = app
            .editor
            .diff_providers
            .get_diff_base(&root.join("new.txt"), true)
            .unwrap();
        assert_eq!(renamed, b"renamed\ncontent\nhere\n");
    }

    // Comment on line 2.
    run_until(
        &mut app,
        "<esc>ggjx:review-comment needs a test\\nplease<ret>",
        |app| {
            app.editor
                .review
                .as_deref()
                .is_some_and(|review| review.draft_count() == 1)
        },
    )
    .await?;
    {
        let review = app.editor.review.as_deref().unwrap();
        let thread = &review.threads[0];
        assert_eq!((thread.start_line, thread.line), (2, 2));
        assert_eq!(thread.comments[0].body, "needs a test\nplease");
        assert_eq!(doc!(app.editor).review_annotations.len(), 1);
        assert!(review.state_file.exists());
    }

    // The thread is drawn below its line.
    let screen = app.screen();
    let lines: Vec<_> = screen.lines().collect();
    let anchor = lines
        .iter()
        .position(|line| line.contains("TWO"))
        .expect(&screen);
    assert!(
        lines[anchor + 1].contains("▎ ● you · L2 · draft"),
        "{screen}"
    );
    assert!(lines[anchor + 2].contains("▎ needs a test"), "{screen}");
    assert!(lines[anchor + 3].contains("▎ please"), "{screen}");
    assert!(lines[anchor + 4].contains("three"), "{screen}");
    eprintln!("{}", lines[..anchor + 6].join("\n"));

    // Threads follow edits above them, even while they are hidden.
    let export = root.join("review.md");
    run_until(&mut app, "<space>vtggOzero<esc><space>vt", |app| {
        doc!(app.editor)
            .review_annotations
            .iter()
            .all(|a| !a.hidden)
    })
    .await?;
    run_until(
        &mut app,
        &format!(":review-export {}<ret>", export.display()),
        |_| export.exists(),
    )
    .await?;
    let markdown = std::fs::read_to_string(&export)?;
    assert!(markdown.contains("### L3 (draft)"), "{markdown}");
    assert!(markdown.contains("TWO"), "{markdown}");
    assert!(markdown.contains("needs a test\nplease"), "{markdown}");

    // ]r jumps to the next comment.
    run_until(&mut app, "gg]r", |app| {
        let (view, doc) = helix_view::current_ref!(app.editor);
        let text = doc.text().slice(..);
        text.char_to_line(doc.selection(view.id).primary().cursor(text)) == 2
    })
    .await?;

    // Marking a file viewed saves its edits and moves on to the next unviewed file.
    run_until(&mut app, ":review-viewed<ret>", |app| {
        doc!(app.editor)
            .path()
            .is_some_and(|p| p.ends_with("b.txt"))
            && std::fs::read_to_string(&a).is_ok_and(|text| text.starts_with("zero\n"))
            && app.editor.documents().all(|doc| !doc.is_modified())
    })
    .await?;
    assert_eq!(app.editor.review.as_deref().unwrap().viewed_count(), 1);

    // Drafts survive closing and reopening the review.
    run_until(&mut app, ":review-close<ret>", |app| {
        app.editor.review.is_none()
    })
    .await?;
    assert!(app.editor.diff_providers.review_base().is_none());
    run_until(&mut app, ":review HEAD~1<ret>", |app| {
        app.editor.review.is_some()
    })
    .await?;
    let review = app.editor.review.as_deref().unwrap();
    assert_eq!(review.draft_count(), 1);
    assert_eq!(review.threads[0].line, 3);
    // Saving your own edits keeps the file viewed.
    assert_eq!(review.viewed_count(), 1);

    // A viewed file that changes on disk becomes unviewed.
    std::fs::write(&a, "one\nTWO\nthree\nfour\n")?;
    run_until(&mut app, "<esc>:review-refresh<ret>", |app| {
        app.editor
            .review
            .as_deref()
            .is_some_and(|review| review.viewed_count() == 0)
    })
    .await?;

    run_until(&mut app, "<esc>:review-close<ret>", |app| {
        app.editor.review.is_none()
    })
    .await?;
    Ok(())
}

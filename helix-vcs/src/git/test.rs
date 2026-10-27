use std::{fs::File, io::Write, path::Path, process::Command};

use tempfile::TempDir;

use crate::{git, FileChange};

fn changed_files(root: &Path) -> Vec<(std::path::PathBuf, &'static str)> {
    let changes = std::cell::RefCell::new(Vec::new());
    git::for_each_changed_file(root, true, |change| {
        let change = change.unwrap();
        let kind = match change {
            FileChange::Untracked { .. } => "untracked",
            FileChange::Modified { .. } => "modified",
            FileChange::Conflict { .. } => "conflict",
            FileChange::Deleted { .. } => "deleted",
            FileChange::Renamed { .. } => "renamed",
        };
        changes
            .borrow_mut()
            .push((change.path().strip_prefix(root).unwrap().to_owned(), kind));
        true
    })
    .unwrap();
    let mut changes = changes.into_inner();
    changes.sort_unstable();
    changes
}

fn exec_git_cmd(args: &str, git_dir: &Path) {
    let res = Command::new("git")
        .arg("-C")
        .arg(git_dir) // execute the git command in this directory
        .args(args.split_whitespace())
        .env_remove("GIT_DIR")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .env("GIT_TERMINAL_PROMPT", "false")
        .env("GIT_AUTHOR_DATE", "2000-01-01 00:00:00 +0000")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_AUTHOR_NAME", "author")
        .env("GIT_COMMITTER_DATE", "2000-01-02 00:00:00 +0000")
        .env("GIT_COMMITTER_EMAIL", "committer@example.com")
        .env("GIT_COMMITTER_NAME", "committer")
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "commit.gpgsign")
        .env("GIT_CONFIG_VALUE_0", "false")
        .env("GIT_CONFIG_KEY_1", "init.defaultBranch")
        .env("GIT_CONFIG_VALUE_1", "main")
        .output()
        .unwrap_or_else(|_| panic!("`git {args}` failed"));
    if !res.status.success() {
        println!("{}", String::from_utf8_lossy(&res.stdout));
        eprintln!("{}", String::from_utf8_lossy(&res.stderr));
        panic!("`git {args}` failed (see output above)")
    }
}

fn create_commit(repo: &Path, add_modified: bool) {
    if add_modified {
        exec_git_cmd("add -A", repo);
    }
    exec_git_cmd("commit -m message", repo);
}

fn empty_git_repo() -> TempDir {
    let tmp = tempfile::tempdir().expect("create temp dir for git testing");
    exec_git_cmd("init", tmp.path());
    exec_git_cmd("config user.email test@helix.org", tmp.path());
    exec_git_cmd("config user.name helix-test", tmp.path());
    tmp
}

#[test]
fn missing_file() {
    let temp_git = empty_git_repo();
    let file = temp_git.path().join("file.txt");
    File::create(&file).unwrap().write_all(b"foo").unwrap();

    assert!(git::get_diff_base(&file, true).is_err());
}

#[test]
fn unmodified_file() {
    let temp_git = empty_git_repo();
    let file = temp_git.path().join("file.txt");
    let contents = b"foo".as_slice();
    File::create(&file).unwrap().write_all(contents).unwrap();
    create_commit(temp_git.path(), true);
    assert_eq!(
        git::get_diff_base(&file, true).unwrap(),
        Vec::from(contents)
    );
}

#[test]
fn modified_file() {
    let temp_git = empty_git_repo();
    let file = temp_git.path().join("file.txt");
    let contents = b"foo".as_slice();
    File::create(&file).unwrap().write_all(contents).unwrap();
    create_commit(temp_git.path(), true);
    File::create(&file).unwrap().write_all(b"bar").unwrap();

    assert_eq!(
        git::get_diff_base(&file, true).unwrap(),
        Vec::from(contents)
    );
}

#[test]
fn changed_file_picker_labels_unstaged_deletions_and_hides_staged_deletions() {
    let repo = empty_git_repo();
    let root = gix::path::realpath(repo.path()).unwrap();
    for name in [
        "staged.txt",
        "unstaged.txt",
        "modified-then-deleted.txt",
        "modified.txt",
    ] {
        std::fs::write(root.join(name), "original\n").unwrap();
    }
    create_commit(&root, true);
    std::fs::remove_file(root.join("staged.txt")).unwrap();
    std::fs::write(root.join("modified-then-deleted.txt"), "staged edit\n").unwrap();
    std::fs::write(root.join("modified.txt"), "staged edit\n").unwrap();
    exec_git_cmd("add -A", &root);
    std::fs::remove_file(root.join("unstaged.txt")).unwrap();
    std::fs::remove_file(root.join("modified-then-deleted.txt")).unwrap();
    std::fs::write(root.join("modified.txt"), "unstaged edit\n").unwrap();
    assert_eq!(
        changed_files(&root),
        vec![
            ("modified-then-deleted.txt".into(), "deleted"),
            ("modified.txt".into(), "modified"),
            ("unstaged.txt".into(), "deleted"),
        ]
    );
}

#[test]
fn changed_file_picker_shows_only_worktree_changes_after_staging() {
    let repo = empty_git_repo();
    let root = gix::path::realpath(repo.path()).unwrap();
    for name in ["modified.txt", "renamed.txt", "restored.txt"] {
        std::fs::write(root.join(name), format!("original {name}\n")).unwrap();
    }
    create_commit(&root, true);
    std::fs::write(root.join("modified.txt"), "staged modification\n").unwrap();
    std::fs::rename(root.join("renamed.txt"), root.join("renamed-new.txt")).unwrap();
    std::fs::remove_file(root.join("restored.txt")).unwrap();
    std::fs::write(root.join("added.txt"), "staged addition\n").unwrap();
    exec_git_cmd("add -A", &root);
    std::fs::write(root.join("added.txt"), "edited after staging\n").unwrap();
    std::fs::write(root.join("renamed-new.txt"), "edited after rename\n").unwrap();
    std::fs::write(
        root.join("restored.txt"),
        "recreated after staged deletion\n",
    )
    .unwrap();
    std::fs::write(root.join("untracked.txt"), "untracked\n").unwrap();
    assert_eq!(
        changed_files(&root),
        vec![
            ("added.txt".into(), "modified"),
            ("renamed-new.txt".into(), "modified"),
            ("restored.txt".into(), "untracked"),
            ("untracked.txt".into(), "untracked"),
        ]
    );
}

#[test]
fn changed_file_picker_works_before_the_first_commit() {
    let repo = empty_git_repo();
    let root = gix::path::realpath(repo.path()).unwrap();
    std::fs::write(root.join("added.txt"), "new\n").unwrap();
    std::fs::write(root.join("deleted.txt"), "new\n").unwrap();
    exec_git_cmd("add -A", &root);
    std::fs::remove_file(root.join("deleted.txt")).unwrap();
    assert_eq!(
        changed_files(&root),
        vec![("deleted.txt".into(), "deleted")]
    );
}

#[test]
fn changed_file_picker_keeps_intent_to_add_untracked() {
    let repo = empty_git_repo();
    let root = gix::path::realpath(repo.path()).unwrap();
    std::fs::write(root.join("new.txt"), "new\n").unwrap();
    exec_git_cmd("add -N new.txt", &root);
    assert_eq!(changed_files(&root), vec![("new.txt".into(), "untracked")]);
}

#[test]
fn diff_base_survives_deleted_files_and_parent_directories() {
    let repo = empty_git_repo();
    let root = repo.path();
    std::fs::create_dir_all(root.join("nested/deeper")).unwrap();
    let file = root.join("nested/deeper/file.txt");
    std::fs::write(&file, "original\n").unwrap();
    create_commit(root, true);
    std::fs::remove_dir_all(root.join("nested")).unwrap();
    for staged in [false, true] {
        if staged {
            exec_git_cmd("add -A", root);
        }
        assert_eq!(git::get_diff_base(&file, true).unwrap(), b"original\n");
        assert!(git::get_current_head_name(&file, true).is_ok());
        assert!(git::search_files(root, true).unwrap().is_empty());
    }
}

#[test]
fn search_files_include_staged_unstaged_and_untracked_paths() {
    let repo = empty_git_repo();
    let root = repo.path();
    std::fs::create_dir(root.join("nested")).unwrap();
    std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
    for name in [
        "clean.txt",
        "modified.txt",
        "staged.txt",
        "deleted.txt",
        "nested/staged.txt",
    ] {
        std::fs::write(root.join(name), "before\n").unwrap();
    }
    create_commit(root, true);
    std::fs::write(root.join("modified.txt"), "modified content\n").unwrap();
    std::fs::write(root.join("staged.txt"), "staged content\n").unwrap();
    std::fs::write(root.join("nested/staged.txt"), "nested staged content\n").unwrap();
    exec_git_cmd("add staged.txt nested/staged.txt", root);
    std::fs::write(root.join("staged.txt"), "also modified after staging\n").unwrap();
    std::fs::write(root.join("new space \u{00e9}.txt"), "new\n").unwrap();
    std::fs::write(root.join("ignored.txt"), "ignored\n").unwrap();
    std::fs::remove_file(root.join("deleted.txt")).unwrap();
    let root = gix::path::realpath(root).unwrap();
    let paths = git::search_files(&root, true).unwrap();
    let mut expected: Vec<_> = [
        "modified.txt",
        "staged.txt",
        "nested/staged.txt",
        "new space \u{00e9}.txt",
    ]
    .into_iter()
    .map(|name| root.join(name))
    .collect();
    expected.sort_unstable();
    assert_eq!(paths, expected);
    assert_eq!(
        git::search_files(&root.join("nested"), true).unwrap(),
        [root.join("nested/staged.txt")]
    );
}

#[test]
fn search_files_work_before_the_first_commit() {
    let repo = empty_git_repo();
    std::fs::write(repo.path().join("staged.txt"), "staged\n").unwrap();
    exec_git_cmd("add staged.txt", repo.path());
    std::fs::write(repo.path().join("untracked.txt"), "untracked\n").unwrap();
    let root = gix::path::realpath(repo.path()).unwrap();
    assert_eq!(
        git::search_files(&root, true).unwrap(),
        [root.join("staged.txt"), root.join("untracked.txt")]
    );
}

/// Test that `get_file_head` does not return content for a directory.
/// This is important to correctly cover cases where a directory is removed and replaced by a file.
/// If the contents of the directory object were returned a diff between a path and the directory children would be produced.
#[test]
fn directory() {
    let temp_git = empty_git_repo();
    let dir = temp_git.path().join("file.txt");
    std::fs::create_dir(&dir).expect("");
    let file = dir.join("file.txt");
    let contents = b"foo".as_slice();
    File::create(file).unwrap().write_all(contents).unwrap();

    create_commit(temp_git.path(), true);

    std::fs::remove_dir_all(&dir).unwrap();
    File::create(&dir).unwrap().write_all(b"bar").unwrap();
    assert!(git::get_diff_base(&dir, true).is_err());
}

/// Test that `get_diff_base` resolves symlinks so that the same diff base is
/// used as the target file.
///
/// This is important to correctly cover cases where a symlink is removed and
/// replaced by a file. If the contents of the symlink object were returned
/// a diff between a literal file path and the actual file content would be
/// produced (bad ui).
#[cfg(any(unix, windows))]
#[test]
fn symlink() {
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    #[cfg(not(unix))]
    use std::os::windows::fs::symlink_file as symlink;

    let temp_git = empty_git_repo();
    let file = temp_git.path().join("file.txt");
    let contents = Vec::from(b"foo");
    File::create(&file).unwrap().write_all(&contents).unwrap();
    let file_link = temp_git.path().join("file_link.txt");

    symlink("file.txt", &file_link).unwrap();
    create_commit(temp_git.path(), true);

    assert_eq!(git::get_diff_base(&file_link, true).unwrap(), contents);
    assert_eq!(git::get_diff_base(&file, true).unwrap(), contents);
}

/// Test that `get_diff_base` returns content when the file is a symlink to
/// another file that is in a git repo, but the symlink itself is not.
#[cfg(any(unix, windows))]
#[test]
fn symlink_to_git_repo() {
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    #[cfg(not(unix))]
    use std::os::windows::fs::symlink_file as symlink;

    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let temp_git = empty_git_repo();

    let file = temp_git.path().join("file.txt");
    let contents = Vec::from(b"foo");
    File::create(&file).unwrap().write_all(&contents).unwrap();
    create_commit(temp_git.path(), true);

    let file_link = temp_dir.path().join("file_link.txt");
    symlink(&file, &file_link).unwrap();

    assert_eq!(git::get_diff_base(&file_link, true).unwrap(), contents);
    assert_eq!(git::get_diff_base(&file, true).unwrap(), contents);
}

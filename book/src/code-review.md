## Code review

Review mode turns Helix into a code review tool for Git branches and GitHub pull
requests. While a review is active:

- The diff gutter, `]g`/`[g` and the changed-file statusline compare files against
  the review base (the merge base of the branch) instead of `HEAD`, so you see every
  change on the branch, committed or not.
- Comment threads are drawn below the lines they belong to and stay attached to
  their lines while you edit.
- The statusline shows `review 3/12 ✓ · 2 drafts`: viewed files, whether the current
  file is viewed, and the number of unsubmitted drafts.

Everything you write is a **draft** until you submit it. Drafts and viewed files are
stored in `.git/helix-review/` and survive restarts, so you can close a review and
continue later.

GitHub features use the [GitHub CLI](https://cli.github.com) (`gh`), which must be
installed and authenticated (`gh auth login`). Local reviews only need `git`.

### Starting a review

| Command | Description |
| --- | --- |
| `Space v v` | Open the review file picker, starting a review of the current branch if none is active |
| `:review [base]` | Review the current branch. Without a base, Helix uses the branch's pull request base if it has one, otherwise the remote default branch (`origin/HEAD`, `origin/main`, ...). `:review HEAD` reviews only uncommitted changes. |
| `:review-pr <number\|url\|branch>` | Check out a pull request with `gh pr checkout` and review it. Add `--no-checkout` to review the currently checked out code. |
| `:review-refresh` | Reload changed files and GitHub threads, for example after pushing |
| `:review-close` | End the review and restore normal `HEAD` diffs. Drafts are kept. |

### Navigating

| Key | Description |
| --- | --- |
| `Space v v` | Changed files with status, `+added -removed` counts, viewed marks and comment counts |
| `Space v n` / `Space v p` | Next / previous changed file (jumps to its first change) |
| `Space v m` | Toggle viewed for the current file and go to the next unviewed file |
| `]g` / `[g` | Next / previous change in the file |
| `Space v h` | Show the base and current version of the change under the cursor |
| `]r` / `[r` | Next / previous comment in the file |
| `Space v C` | All comments, drafts and threads in a picker |
| `Space v i` | Summary, pull request description and a key reference |

A viewed file becomes unviewed again when its contents change. Deleted files open
as a read-only buffer with their base version.

### Commenting

| Key | Command | Description |
| --- | --- | --- |
| `Space v c` | `:review-comment <text>` | Comment on the selected lines (select several lines with `x` for a multi-line comment) |
| `Space v r` | `:review-reply <text>` | Reply to the thread under the cursor |
| `Space v e` | `:review-edit <text>` | Edit your draft under the cursor (an empty text deletes it) |
| `Space v d` | `:review-delete` | Delete your draft under the cursor |
| `Space v x` | `:review-resolve` | Resolve or unresolve the GitHub thread under the cursor (applies immediately) |
| `Space v t` | | Show or hide inline comments |

The comment prompt is a single line. Type `\n` for a line break; Markdown, including
GitHub ` ```suggestion ` blocks, is sent as written.

### Submitting

`Space v s` asks whether to **c**omment, **a**pprove or **r**equest changes, then
for an optional message, and submits all drafts as a single GitHub review. Draft
replies are posted to their threads. The same is available as
`:review-submit [comment|approve|request-changes] [message]`.

GitHub only accepts line comments on lines that are part of the pull request diff.
Drafts on other lines are added to the review message with their file and line, so
nothing is lost. Requesting changes needs a message.

Because comments are placed on the pull request's head commit, Helix refuses to
submit new comments when your local `HEAD` differs from it, or when commented files
have unsaved or uncommitted changes, rather than posting them on the wrong lines.
Push (or pull) and `:review-refresh` first. Only one submission runs at a time.

Without a pull request, `Space v y` (or `:review-export [file]`) copies all comments
as Markdown, including the code they refer to, for pasting into a pull request,
an issue or a chat.

### Theming

Inline threads use `ui.virtual.review.frame`, `ui.virtual.review.author`,
`ui.virtual.review.body`, `ui.virtual.review.meta`, `ui.virtual.review.draft` and
`ui.virtual.review.resolved`, falling back to `info`, `ui.text`, `comment` and
`warning`. The statusline element is styled with `ui.statusline.review`, falling
back to `info`.

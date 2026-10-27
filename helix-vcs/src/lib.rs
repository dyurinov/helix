//! `helix_vcs` provides types for working with diffs from a Version Control System (VCS).
//! Currently `git` is the only supported provider for diffs, but this architecture allows
//! for other providers to be added in the future.

use anyhow::{anyhow, bail, Result};
use arc_swap::ArcSwap;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[cfg(feature = "git")]
mod git;

mod diff;

pub use diff::{DiffHandle, Hunk};

mod status;

pub use status::FileChange;

/// Existing staged, unstaged and untracked files under a Git search root.
pub fn git_search_files(root: &Path, trust_full: bool) -> Result<Vec<PathBuf>> {
    #[cfg(feature = "git")]
    return git::search_files(root, trust_full);
    #[cfg(not(feature = "git"))]
    {
        let _ = (root, trust_full);
        bail!("Git support is not enabled")
    }
}

/// A commit that replaces `HEAD` as the diff base for files in a reviewed worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewBase {
    /// Canonical path of the reviewed worktree.
    pub root: PathBuf,
    /// Full hex object id of the base commit.
    pub commit: String,
    /// Renamed files: absolute new path to absolute path at the base commit.
    pub renames: std::collections::HashMap<PathBuf, PathBuf>,
}

/// Contains all active diff providers. Diff providers are compiled in via features. Currently
/// only `git` is supported.
#[derive(Clone)]
pub struct DiffProviderRegistry {
    providers: Vec<DiffProvider>,
    /// Shared by all clones so background diff refreshes observe review mode.
    review_base: Arc<ArcSwap<Option<ReviewBase>>>,
}

impl DiffProviderRegistry {
    /// Diff files inside `base.root` against `base.commit` instead of `HEAD`.
    /// `None` restores regular `HEAD` diffs.
    pub fn set_review_base(&self, base: Option<ReviewBase>) {
        self.review_base.store(Arc::new(base));
    }

    pub fn review_base(&self) -> Option<ReviewBase> {
        Option::clone(&self.review_base.load())
    }

    /// Get the given file from the VCS. This provides the unedited document as a "base"
    /// for a diff to be created.
    pub fn get_diff_base(&self, file: &Path, trust_full: bool) -> Option<Vec<u8>> {
        #[cfg(feature = "git")]
        if let Some(base) = self.review_base.load().as_ref() {
            match git::get_diff_base_at(file, base, trust_full) {
                Ok(Some(res)) => return Some(res),
                Ok(None) => {}
                Err(err) => {
                    log::debug!("{err:#?}");
                    log::debug!("failed to open review diff base for {}", file.display());
                    return None;
                }
            }
        }
        self.providers
            .iter()
            .find_map(|provider| match provider.get_diff_base(file, trust_full) {
                Ok(res) => Some(res),
                Err(err) => {
                    log::debug!("{err:#?}");
                    log::debug!("failed to open diff base for {}", file.display());
                    None
                }
            })
    }

    /// Get the current name of the current [HEAD](https://stackoverflow.com/questions/2304087/what-is-head-in-git).
    pub fn get_current_head_name(
        &self,
        file: &Path,
        trust_full: bool,
    ) -> Option<Arc<ArcSwap<Box<str>>>> {
        self.providers.iter().find_map(|provider| {
            match provider.get_current_head_name(file, trust_full) {
                Ok(res) => Some(res),
                Err(err) => {
                    log::debug!("{err:#?}");
                    log::debug!("failed to obtain current head name for {}", file.display());
                    None
                }
            }
        })
    }

    /// Fire-and-forget changed file iteration. Runs everything in a background task. Keeps
    /// iteration until `on_change` returns `false`.
    pub fn for_each_changed_file(
        self,
        cwd: PathBuf,
        trust_full: bool,
        f: impl Fn(Result<FileChange>) -> bool + Send + 'static,
    ) {
        tokio::task::spawn_blocking(move || {
            if self
                .providers
                .iter()
                .find_map(|provider| provider.for_each_changed_file(&cwd, trust_full, &f).ok())
                .is_none()
            {
                f(Err(anyhow!("no diff provider returns success")));
            }
        });
    }
}

impl Default for DiffProviderRegistry {
    fn default() -> Self {
        // currently only git is supported
        // TODO make this configurable when more providers are added
        let providers = vec![
            #[cfg(feature = "git")]
            DiffProvider::Git,
            DiffProvider::None,
        ];
        DiffProviderRegistry {
            providers,
            review_base: Arc::new(ArcSwap::from_pointee(None)),
        }
    }
}

/// A union type that includes all types that implement [DiffProvider]. We need this type to allow
/// cloning [DiffProviderRegistry] as `Clone` cannot be used in trait objects.
///
/// `Copy` is simply to ensure the `clone()` call is the simplest it can be.
#[derive(Copy, Clone)]
enum DiffProvider {
    #[cfg(feature = "git")]
    Git,
    None,
}

impl DiffProvider {
    fn get_diff_base(&self, file: &Path, trust_full: bool) -> Result<Vec<u8>> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::get_diff_base(file, trust_full),
            Self::None => bail!("No diff support compiled in"),
        }
    }

    fn get_current_head_name(
        &self,
        file: &Path,
        trust_full: bool,
    ) -> Result<Arc<ArcSwap<Box<str>>>> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::get_current_head_name(file, trust_full),
            Self::None => bail!("No diff support compiled in"),
        }
    }

    fn for_each_changed_file(
        &self,
        cwd: &Path,
        trust_full: bool,
        f: impl Fn(Result<FileChange>) -> bool,
    ) -> Result<()> {
        match self {
            #[cfg(feature = "git")]
            Self::Git => git::for_each_changed_file(cwd, trust_full, f),
            Self::None => bail!("No diff support compiled in"),
        }
    }
}

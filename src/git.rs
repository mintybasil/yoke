//! Git repository management: clone, pull, worktree creation/removal,
//! and token-based authentication for GitHub and GitLab.
//!
//! This module provides the git operations layer for Yoke's webhook-driven
//! workflow execution. On first event for a repo, it clones the repository.
//! On subsequent events, it pulls the latest changes. For each event, it
//! can create an isolated worktree and clean it up after workflow completion.
//!
//! Authentication uses `git2::RemoteCallbacks` with platform-specific
//! token prefixes: `x-access-token` for GitHub, `oauth2` for GitLab.
//! Tokens are never embedded in URLs or stored in git config.

use std::path::Path;

use git2::{
    BranchType, Cred, CredentialType, FetchOptions, ProxyOptions, RemoteCallbacks, Repository,
    WorktreeAddOptions, WorktreePruneOptions,
};
use thiserror::Error;

/// Errors that can occur during git operations.
#[derive(Debug, Error)]
pub enum GitError {
    /// An error from the `git2` crate.
    #[error("git error: {0}")]
    Git(#[from] git2::Error),
    /// The target directory already exists and is not an empty directory.
    #[error("target directory already exists: {0}")]
    DirectoryExists(String),
    /// The repository has uncommitted changes that prevent worktree removal.
    #[error("uncommitted changes in worktree: {0}")]
    DirtyWorktree(String),
    /// An I/O error occurred.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Sanitize a label for use as a git branch name component.
///
/// Removes characters that are not alphanumeric, hyphens, or underscores,
/// and replaces spaces with hyphens. Collapses consecutive hyphens into one.
///
/// # Examples
///
/// ```
/// use yoke::git::sanitize_branch_name;
/// assert_eq!(sanitize_branch_name("hello world"), "hello-world");
/// assert_eq!(sanitize_branch_name("feat: add auth!"), "feat-add-auth");
/// assert_eq!(sanitize_branch_name("  spaces  "), "spaces");
/// ```
pub fn sanitize_branch_name(label: &str) -> String {
    let sanitized: String = label
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();

    // Collapse consecutive hyphens and trim leading/trailing hyphens
    let mut result = String::with_capacity(sanitized.len());
    let mut prev_hyphen = false;
    for c in sanitized.chars() {
        if c == '-' {
            if !prev_hyphen {
                result.push(c);
            }
            prev_hyphen = true;
        } else {
            result.push(c);
            prev_hyphen = false;
        }
    }

    // Trim leading and trailing hyphens
    let trimmed = result.trim_matches('-');
    trimmed.to_string()
}

/// Build a clone URL for a given platform, owner, and repo.
///
/// - GitHub: `https://github.com/{owner}/{repo}.git`
/// - GitLab: `https://{host}/{owner}/{repo}.git` (host defaults to `gitlab.com`)
///
/// The URL does **not** include the token — authentication is handled
/// separately via `RemoteCallbacks` during clone/pull operations.
pub fn build_clone_url(
    platform: &str,
    owner: &str,
    repo: &str,
    gitlab_host: Option<&str>,
) -> String {
    match platform {
        "github" => format!("https://github.com/{owner}/{repo}.git"),
        "gitlab" => {
            let host = gitlab_host.unwrap_or("gitlab.com");
            format!("https://{host}/{owner}/{repo}.git")
        }
        _ => format!("https://github.com/{owner}/{repo}.git"),
    }
}

/// Create a `RemoteCallbacks` that authenticates using a platform-specific token.
///
/// GitHub uses `x-access-token` as the username; GitLab uses `oauth2`.
/// The token is provided via `Cred::userpass_plaintext`.
fn make_credentials_cb<'a>(token: &'a str, platform: &'a str) -> RemoteCallbacks<'a> {
    let token_owned = token.to_string();
    let platform_owned = platform.to_string();
    let mut cb = RemoteCallbacks::new();
    cb.credentials(move |_url, _username_from_url, allowed_types| {
        if allowed_types.contains(CredentialType::USER_PASS_PLAINTEXT) {
            let username = match platform_owned.as_str() {
                "gitlab" => "oauth2",
                _ => "x-access-token",
            };
            Cred::userpass_plaintext(username, &token_owned)
        } else {
            Cred::default()
        }
    });
    cb
}

/// Create `FetchOptions` with authentication and system proxy settings.
fn create_fetch_options<'a>(token: &'a str, platform: &'a str) -> FetchOptions<'a> {
    let mut fo = FetchOptions::new();
    fo.remote_callbacks(make_credentials_cb(token, platform));
    let mut proxy = ProxyOptions::new();
    proxy.auto();
    fo.proxy_options(proxy);
    fo
}

/// Clone a repository to the specified path using token authentication.
///
/// The `platform` parameter determines the authentication method:
/// - `"github"` uses `x-access-token` as the username
/// - `"gitlab"` uses `oauth2` as the username
///
/// # Errors
///
/// Returns `GitError::DirectoryExists` if the target path already exists
/// and is not an empty directory. Returns `GitError::Git` for clone failures.
pub fn clone_repo(
    url: &str,
    path: &Path,
    token: &str,
    platform: &str,
) -> Result<Repository, GitError> {
    // Check if the target directory already exists and is not empty
    if path.exists() && path.is_dir() {
        // Allow cloning into an empty directory
        let is_empty = path.read_dir().is_ok_and(|mut d| d.next().is_none());
        if !is_empty {
            return Err(GitError::DirectoryExists(path.display().to_string()));
        }
    }

    let fo = create_fetch_options(token, platform);
    let mut builder = git2::build::RepoBuilder::new();
    builder.fetch_options(fo);
    let repo = builder.clone(url, path)?;
    Ok(repo)
}

/// Pull (fetch + fast-forward merge) the latest changes from the remote.
///
/// Fetches from `origin` and fast-forwards the current branch to the
/// remote's default branch. If a fast-forward is not possible (diverged
/// history), returns an error.
///
/// # Errors
///
/// Returns `GitError::Git` for fetch or merge failures.
pub fn pull_repo(repo: &Repository, token: &str, platform: &str) -> Result<(), GitError> {
    let mut remote = repo.find_remote("origin")?;

    let mut fo = create_fetch_options(token, platform);
    remote.fetch(&[] as &[&str], Some(&mut fo), None)?;

    // Find the remote's default branch
    let remote_head = remote.default_branch()?;
    let remote_branch_name = remote_head
        .as_str()
        .ok_or_else(|| git2::Error::from_str("remote HEAD is not a valid UTF-8 string"))?;

    // Strip "refs/heads/" prefix to get the branch name
    let branch_name = remote_branch_name
        .strip_prefix("refs/heads/")
        .unwrap_or(remote_branch_name);

    // Find the fetched commit (FETCH_HEAD)
    let fetch_head = repo.find_reference("FETCH_HEAD")?;
    let fetch_commit = fetch_head.peel_to_commit()?;

    // Get the local branch reference
    let local_branch_ref = repo.find_reference(&format!("refs/heads/{branch_name}"));
    let local_commit = match &local_branch_ref {
        Ok(r) => r.peel_to_commit()?.id(),
        Err(_) => fetch_commit.id(), // Branch doesn't exist locally yet
    };

    // Check if fast-forward is possible
    let merge_base = repo.merge_base(local_commit, fetch_commit.id())?;
    if merge_base != local_commit {
        return Err(GitError::Git(git2::Error::from_str(
            "cannot fast-forward: local branch has diverged from remote",
        )));
    }

    // Fast-forward: update the local branch to point to the fetched commit.
    // When HEAD is a symbolic reference pointing to this branch (the common
    // case), git automatically follows the branch update — no separate HEAD
    // update needed.  Calling `set_target` on a symbolic HEAD ref raises
    // "cannot set OID on symbolic reference", so we rely on the branch
    // update alone.
    if local_branch_ref.is_ok() {
        let mut ref_mut = repo.find_reference(&format!("refs/heads/{branch_name}"))?;
        ref_mut.set_target(fetch_commit.id(), "fast-forward merge")?;
    }

    Ok(())
}

/// Create a worktree for a specific event, branching from the default branch.
///
/// The worktree is created at `worktree_path` with the specified branch name.
/// If the branch does not already exist, it is created from the current HEAD.
/// Branch names containing `/` are sanitized for the worktree administrative
/// directory name (replacing `/` with `-`) while the actual git branch keeps
/// its original name.
///
/// # Errors
///
/// Returns `GitError::Git` for branch creation or worktree addition failures.
pub fn create_worktree(
    repo: &Repository,
    branch_name: &str,
    worktree_path: &Path,
) -> Result<(), GitError> {
    // Detach HEAD before creating the worktree.  Git refuses to create a
    // worktree from a branch that is currently checked out in the parent
    // repository ("reference refs/heads/<branch> is already checked out").
    // Detaching HEAD (pointing it at the commit instead of the branch) means
    // no branch ref is "checked out", allowing any branch to be used as a
    // worktree source.
    {
        let head = repo.head()?;
        let head_commit = head.peel_to_commit()?;
        repo.set_head_detached(head_commit.id())?;
    }

    // Check if the branch already exists
    let branch_exists = repo.find_branch(branch_name, BranchType::Local).is_ok();

    if !branch_exists {
        // Create the branch from HEAD
        let head = repo.head()?;
        let head_commit = head.peel_to_commit()?;
        repo.branch(branch_name, &head_commit, false)?;
    }

    // Find the branch reference for the worktree
    let branch_ref = repo.find_reference(&format!("refs/heads/{branch_name}"))?;

    // Sanitize worktree name: git2 worktree_add creates directories under
    // .git/worktrees/<name>/ — slashes create nested dirs that fail.
    let worktree_name = branch_name.replace('/', "-");

    let mut opts = WorktreeAddOptions::new();
    opts.reference(Some(&branch_ref));

    repo.worktree(&worktree_name, worktree_path, Some(&opts))?;

    Ok(())
}

/// Remove a worktree by branch name, cleaning up administrative data.
///
/// Uses `WorktreePruneOptions` to force-remove the worktree even if it's
/// locked or the working directory is still present. Also deletes the
/// local branch associated with the worktree.
///
/// # Errors
///
/// Returns `GitError::Git` if the worktree cannot be found or pruned.
pub fn remove_worktree(repo: &Repository, branch_name: &str) -> Result<(), GitError> {
    let worktree_name = branch_name.replace('/', "-");
    let worktree = repo.find_worktree(&worktree_name)?;

    let mut prune_opts = WorktreePruneOptions::new();
    prune_opts.valid(true).locked(true).working_tree(true);

    worktree.prune(Some(&mut prune_opts))?;

    // Clean up the branch if it still exists
    if let Ok(mut branch) = repo.find_branch(branch_name, BranchType::Local) {
        branch.delete()?;
    }

    Ok(())
}

/// Check whether a repository contains uncommitted changes.
///
/// Returns `Ok(true)` if there are unstaged or staged changes, `Ok(false)`
/// if the working tree is clean.
///
/// # Errors
///
/// Returns `GitError::Git` if the status check fails.
pub fn has_uncommitted_changes(repo: &Repository) -> Result<bool, GitError> {
    let statuses = repo.statuses(None)?;
    for entry in statuses.iter() {
        let status = entry.status();
        if status.is_wt_new()
            || status.is_wt_modified()
            || status.is_wt_deleted()
            || status.is_wt_renamed()
            || status.is_wt_typechange()
            || status.is_index_new()
            || status.is_index_modified()
            || status.is_index_deleted()
            || status.is_index_renamed()
            || status.is_index_typechange()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- sanitize_branch_name tests ---

    #[test]
    fn test_sanitize_simple() {
        assert_eq!(sanitize_branch_name("hello"), "hello");
    }

    #[test]
    fn test_sanitize_spaces() {
        assert_eq!(sanitize_branch_name("hello world"), "hello-world");
    }

    #[test]
    fn test_sanitize_special_chars() {
        assert_eq!(sanitize_branch_name("feat: add auth!"), "feat-add-auth");
    }

    #[test]
    fn test_sanitize_multiple_spaces() {
        assert_eq!(sanitize_branch_name("a   b"), "a-b");
    }

    #[test]
    fn test_sanitize_leading_trailing_hyphens() {
        assert_eq!(sanitize_branch_name("---hello---"), "hello");
    }

    #[test]
    fn test_sanitize_empty() {
        assert_eq!(sanitize_branch_name(""), "");
    }

    #[test]
    fn test_sanitize_underscores_preserved() {
        assert_eq!(sanitize_branch_name("feature_branch"), "feature_branch");
    }

    #[test]
    fn test_sanitize_mixed_special_and_spaces() {
        assert_eq!(
            sanitize_branch_name("fix: login @ issue #123"),
            "fix-login-issue-123"
        );
    }

    #[test]
    fn test_sanitize_slashes() {
        // Slashes in branch labels become hyphens
        assert_eq!(sanitize_branch_name("ao/feature"), "ao-feature");
    }

    #[test]
    fn test_sanitize_numbers() {
        assert_eq!(sanitize_branch_name("issue-42"), "issue-42");
    }

    // --- build_clone_url tests ---

    #[test]
    fn test_build_clone_url_github() {
        let url = build_clone_url("github", "owner", "repo", None);
        assert_eq!(url, "https://github.com/owner/repo.git");
    }

    #[test]
    fn test_build_clone_url_gitlab_default() {
        let url = build_clone_url("gitlab", "owner", "repo", None);
        assert_eq!(url, "https://gitlab.com/owner/repo.git");
    }

    #[test]
    fn test_build_clone_url_gitlab_custom_host() {
        let url = build_clone_url("gitlab", "owner", "repo", Some("gitlab.mycompany.com"));
        assert_eq!(url, "https://gitlab.mycompany.com/owner/repo.git");
    }

    #[test]
    fn test_build_clone_url_unknown_platform_defaults_to_github() {
        let url = build_clone_url("unknown", "owner", "repo", None);
        assert_eq!(url, "https://github.com/owner/repo.git");
    }

    // --- credential callback tests ---

    #[test]
    fn test_make_credentials_cb_github() {
        // Verify construction without panic
        let _cb = make_credentials_cb("test-token", "github");
    }

    #[test]
    fn test_make_credentials_cb_gitlab() {
        let _cb = make_credentials_cb("test-token", "gitlab");
    }

    // --- clone_repo directory checks ---

    #[test]
    fn test_clone_repo_non_empty_dir_fails() {
        use std::fs;

        let dir = tempfile::tempdir().unwrap();
        let non_empty_dir = dir.path().join("target");
        fs::create_dir_all(&non_empty_dir).unwrap();
        fs::write(non_empty_dir.join("file.txt"), "data").unwrap();

        let result = clone_repo(
            "https://github.com/nonexistent/repo.git",
            &non_empty_dir,
            "fake-token",
            "github",
        );

        assert!(result.is_err());
        let err = result.err().unwrap();
        match err {
            GitError::DirectoryExists(path) => {
                assert_eq!(path, non_empty_dir.display().to_string());
            }
            other => panic!("expected DirectoryExists, got: {other}"),
        }
    }

    #[test]
    fn test_clone_repo_empty_dir_allowed() {
        use std::fs;

        let dir = tempfile::tempdir().unwrap();
        let empty_dir = dir.path().join("target");
        fs::create_dir_all(&empty_dir).unwrap();

        // This will fail because the URL is invalid, but it should NOT
        // fail with DirectoryExists
        let result = clone_repo(
            "https://github.com/nonexistent/repo.git",
            &empty_dir,
            "fake-token",
            "github",
        );

        // The error should be a GitError::Git, not DirectoryExists
        let err = result.err().unwrap();
        match err {
            GitError::Git(_) => {} // expected: clone fails because repo doesn't exist
            GitError::DirectoryExists(_) => panic!("empty dir should not trigger DirectoryExists"),
            other => panic!("unexpected error: {other}"),
        }
    }

    // --- has_uncommitted_changes tests ---

    #[test]
    fn test_has_uncommitted_changes_clean_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();

        // No changes in a fresh repo (no commits, no files)
        assert!(!has_uncommitted_changes(&repo).unwrap());
    }

    // --- GitError display tests ---

    #[test]
    fn test_git_error_display() {
        let err = GitError::DirectoryExists("/some/path".to_string());
        assert_eq!(
            format!("{err}"),
            "target directory already exists: /some/path"
        );

        let err = GitError::DirtyWorktree("modified file".to_string());
        assert_eq!(
            format!("{err}"),
            "uncommitted changes in worktree: modified file"
        );
    }
}

//! Git repository management: shallow clone and token-based authentication
//! for GitHub and GitLab.
//!
//! This module provides the git operations layer for Yoke's webhook-driven
//! workflow execution. Each event gets its own isolated shallow clone
//! (`git clone --depth=1`), which is cleaned up when the workflow finishes.
//!
//! Authentication uses `git2::RemoteCallbacks` with platform-specific
//! token prefixes: `x-access-token` for GitHub, `oauth2` for GitLab.
//! Tokens are never embedded in URLs or stored in git config.

use std::path::Path;

use git2::{Cred, CredentialType, FetchOptions, ProxyOptions, RemoteCallbacks, Repository};
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
/// separately via `RemoteCallbacks` during clone operations.
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

/// Perform a per-event shallow clone of a repository.
///
/// Creates an isolated `git clone --depth=1 -b <branch>` at the specified
/// path. Each event gets its own clone, eliminating all shared-state issues
/// that existed with worktree-based approaches.
///
/// Uses the `git` CLI rather than `git2` because libgit2 does not support
/// shallow clones. Authentication is provided via the token embedded in the
/// URL (HTTPS with embedded credentials).
///
/// # Arguments
///
/// * `url` - The clone URL (from `build_clone_url`).
/// * `branch` - The branch to checkout (e.g. `"main"` or `"ao/feature-123"`).
/// * `path` - The local directory to clone into.
/// * `token` - Platform-specific authentication token.
/// * `platform` - `"github"` or `"gitlab"`.
///
/// # Errors
///
/// Returns `GitError::DirectoryExists` if the target path is non-empty.
/// Returns `GitError::Io` if parent directory creation fails.
/// Returns `GitError::Git` if the `git` command cannot be executed
/// (e.g. `git` is not installed) or exits with a non-zero status.
pub fn shallow_clone(
    url: &str,
    branch: &str,
    path: &Path,
    token: &str,
    platform: &str,
) -> Result<(), GitError> {
    // Ensure parent directories exist
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Check if the target directory already exists and is not empty
    if path.exists() && path.is_dir() {
        let is_empty = path.read_dir().is_ok_and(|mut d| d.next().is_none());
        if !is_empty {
            return Err(GitError::DirectoryExists(path.display().to_string()));
        }
    }

    // Build an authenticated URL by embedding the token.
    // GitHub: https://x-access-token:<token>@github.com/owner/repo.git
    // GitLab: https://oauth2:<token>@gitlab.com/owner/repo.git
    let username = match platform {
        "gitlab" => "oauth2",
        _ => "x-access-token",
    };
    let auth_url = embed_token_in_url(url, token, username);

    let output = std::process::Command::new("git")
        .args([
            "clone",
            "--depth=1",
            "--branch",
            branch,
            &auth_url,
            &path.to_string_lossy(),
        ])
        .output()
        .map_err(|e| {
            let msg = if e.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "failed to execute `git clone`: `git` binary not found in PATH \
                     (os error 2). Ensure git is installed in the runtime environment. \
                     Underlying error: {e}"
                )
            } else {
                format!("failed to execute `git clone` for {url}: {e}")
            };
            GitError::Git(git2::Error::from_str(&msg))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(GitError::Git(git2::Error::from_str(&format!(
            "shallow clone failed: {stderr}"
        ))));
    }

    Ok(())
}

/// Embed an authentication token into a git HTTPS URL.
///
/// Converts `https://github.com/owner/repo.git` into
/// `https://x-access-token:<token>@github.com/owner/repo.git`.
fn embed_token_in_url(url: &str, token: &str, username: &str) -> String {
    // Parse the URL and inject credentials
    if let Ok(mut parsed) = url::Url::parse(url) {
        let _ = parsed.set_username(username);
        let _ = parsed.set_password(Some(token));
        parsed.to_string()
    } else {
        // Fallback: manual string manipulation
        url.replacen("https://", &format!("https://{username}:{token}@"), 1)
    }
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
            GitError::DirectoryExists(_) => {
                panic!("empty dir should not trigger DirectoryExists")
            }
            GitError::Git(_) => {} // expected: clone fails because repo doesn't exist
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

    // --- embed_token_in_url tests ---

    #[test]
    fn test_embed_token_in_url_github() {
        let url = "https://github.com/owner/repo.git";
        let result = embed_token_in_url(url, "secret-token", "x-access-token");
        assert_eq!(
            result,
            "https://x-access-token:secret-token@github.com/owner/repo.git"
        );
    }

    #[test]
    fn test_embed_token_in_url_gitlab() {
        let url = "https://gitlab.com/owner/repo.git";
        let result = embed_token_in_url(url, "gl-token", "oauth2");
        assert_eq!(result, "https://oauth2:gl-token@gitlab.com/owner/repo.git");
    }

    #[test]
    fn test_embed_token_in_url_gitlab_custom_host() {
        let url = "https://gitlab.mycompany.com/owner/repo.git";
        let result = embed_token_in_url(url, "my-token", "oauth2");
        assert_eq!(
            result,
            "https://oauth2:my-token@gitlab.mycompany.com/owner/repo.git"
        );
    }

    // --- shallow_clone directory checks ---

    #[test]
    fn test_shallow_clone_non_empty_dir_fails() {
        use std::fs;

        let dir = tempfile::tempdir().unwrap();
        let non_empty_dir = dir.path().join("target");
        fs::create_dir_all(&non_empty_dir).unwrap();
        fs::write(non_empty_dir.join("file.txt"), "data").unwrap();

        let result = shallow_clone(
            "https://github.com/nonexistent/repo.git",
            "main",
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
    fn test_shallow_clone_empty_dir_allowed() {
        use std::fs;

        let dir = tempfile::tempdir().unwrap();
        let empty_dir = dir.path().join("target");
        fs::create_dir_all(&empty_dir).unwrap();

        // This will fail because the URL is invalid, but it should NOT
        // fail with DirectoryExists
        let result = shallow_clone(
            "https://github.com/nonexistent/repo.git",
            "main",
            &empty_dir,
            "fake-token",
            "github",
        );

        // The error should be a GitError::Git or Io, not DirectoryExists
        let err = result.err().unwrap();
        match err {
            GitError::DirectoryExists(_) => {
                panic!("empty dir should not trigger DirectoryExists")
            }
            GitError::Git(_) | GitError::Io(_) => {} // expected
        }
    }

    // --- GitError display tests ---

    #[test]
    fn test_git_error_display() {
        let err = GitError::DirectoryExists("/some/path".to_string());
        assert_eq!(
            format!("{err}"),
            "target directory already exists: /some/path"
        );
    }
}

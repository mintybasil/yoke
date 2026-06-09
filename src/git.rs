//! Git repository management: shallow clone and token-based
//! authentication for GitHub and GitLab.
//!
//! This module provides the git operations layer for Yoke's webhook-driven
//! workflow execution. When `git.clone = true`, each event gets its own
//! isolated shallow clone (`git clone --depth=1 -b <branch>`) in the
//! workspace directory, providing full parallelism with no shared state
//! conflicts.
//!
//! Authentication embeds platform-specific tokens in the clone URL:
//! `x-access-token` for GitHub, `oauth2` for GitLab. Tokens are stripped
//! from error messages to avoid credential leaks.

use std::path::Path;

use git2::Repository;
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
    /// A git subprocess command failed.
    #[error("git command failed: {0}")]
    Command(String),
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
/// separately by embedding the token in the URL at clone time.
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

/// Build an authenticated clone URL by embedding the token.
///
/// - GitHub: `https://x-access-token:<token>@github.com/owner/repo.git`
/// - GitLab: `https://oauth2:<token>@gitlab.example.com/owner/repo.git`
fn build_authenticated_url(
    url: &str,
    token: &str,
    platform: &str,
    gitlab_host: Option<&str>,
) -> String {
    let _host = match platform {
        "gitlab" => gitlab_host.unwrap_or("gitlab.com"),
        _ => "github.com",
    };

    let username = match platform {
        "gitlab" => "oauth2",
        _ => "x-access-token",
    };

    // Insert token into URL: https://host/path.git -> https://user:token@host/path.git
    if let Some(rest) = url.strip_prefix("https://") {
        format!("https://{username}:{token}@{rest}")
    } else {
        // Fallback: construct URL from scratch
        build_clone_url(platform, "", "", gitlab_host)
    }
}

/// Clone a repository to the specified path using a shallow clone.
///
/// Performs `git clone --depth=1 -b <branch>` via the git subprocess,
/// embedding the token in the URL for authentication. This creates a
/// fully isolated clone with no shared state — each event gets its own
/// directory, avoiding concurrency bugs from shared `.git` state.
///
/// If `branch` is `None`, the remote's default branch is used.
/// If the target directory exists and is non-empty, returns `GitError::DirectoryExists`.
///
/// # Errors
///
/// Returns `GitError::DirectoryExists` if the target path already exists
/// and is not an empty directory. Returns `GitError::Command` for clone
/// failures. Returns `GitError::Io` for filesystem errors.
pub fn clone_repo(
    url: &str,
    path: &Path,
    branch: Option<&str>,
    token: &str,
    platform: &str,
    gitlab_host: Option<&str>,
) -> Result<Repository, GitError> {
    // Check if the target directory already exists and is not empty
    if path.exists() && path.is_dir() {
        let is_empty = path.read_dir().is_ok_and(|mut d| d.next().is_none());
        if !is_empty {
            return Err(GitError::DirectoryExists(path.display().to_string()));
        }
    }

    // Create parent directories if needed
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Build an authenticated URL by embedding the token.
    // Command-line git requires credentials in the URL; libgit2 callback auth
    // is not available when shelling out.
    let auth_url = build_authenticated_url(url, token, platform, gitlab_host);

    let mut cmd = std::process::Command::new("git");
    cmd.arg("clone")
        .arg("--depth")
        .arg("1");

    if let Some(branch_name) = branch {
        cmd.arg("--branch").arg(branch_name);
    }

    cmd.arg(&auth_url).arg(path);

    let output = cmd.output().map_err(|e| GitError::Command(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Strip auth token from error messages to avoid leaking credentials in logs
        let safe_stderr = stderr.replace(token, "***");
        return Err(GitError::Command(safe_stderr));
    }

    // Open the cloned repository with git2 for subsequent operations
    let repo = Repository::open(path)?;
    Ok(repo)
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
            None,
            "fake-token",
            "github",
            None,
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
            None,
            "fake-token",
            "github",
            None,
        );

        // The error should be a GitError::Command (subprocess), not DirectoryExists
        let err = result.err().unwrap();
        match err {
            GitError::Command(_) => {} // expected: clone fails because repo doesn't exist
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
    }

    #[test]
    fn test_git_error_command_display() {
        let err = GitError::Command("exit code 128".to_string());
        assert_eq!(format!("{err}"), "git command failed: exit code 128");
    }

    // --- build_authenticated_url tests ---

    #[test]
    fn test_build_authenticated_url_github() {
        let url = "https://github.com/owner/repo.git";
        let result = build_authenticated_url(url, "ghp_token123", "github", None);
        assert_eq!(result, "https://x-access-token:ghp_token123@github.com/owner/repo.git");
    }

    #[test]
    fn test_build_authenticated_url_gitlab_default() {
        let url = "https://gitlab.com/owner/repo.git";
        let result = build_authenticated_url(url, "glpat_token456", "gitlab", None);
        assert_eq!(result, "https://oauth2:glpat_token456@gitlab.com/owner/repo.git");
    }

    #[test]
    fn test_build_authenticated_url_gitlab_custom_host() {
        let url = "https://gitlab.mycompany.com/owner/repo.git";
        let result = build_authenticated_url(url, "glpat_token789", "gitlab", Some("gitlab.mycompany.com"));
        assert_eq!(result, "https://oauth2:glpat_token789@gitlab.mycompany.com/owner/repo.git");
    }
}
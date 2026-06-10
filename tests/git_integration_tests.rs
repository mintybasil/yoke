//! Integration tests for git module lifecycle operations.
//!
//! These tests exercise local git operations (init, branch, status checks)
//! without requiring network access. They use `git2::Repository::init`
//! to create local repositories and test the full lifecycle of shallow clone
//! management.

use std::fs;
use std::path::Path;

use git2::Repository;
use yoke::git::{build_clone_url, has_uncommitted_changes, sanitize_branch_name};

/// Helper: create a local git repo with an initial commit on `main`.
fn init_repo_with_commit(dir: &Path) -> Repository {
    let repo = Repository::init(dir).unwrap();

    // Configure user for commits
    let mut config = repo.config().unwrap();
    config.set_str("user.name", "Test User").unwrap();
    config.set_str("user.email", "test@example.com").unwrap();

    // Create an initial file and commit
    let file_path = dir.join("README.md");
    fs::write(&file_path, "hello world").unwrap();

    let mut index = repo.index().unwrap();
    index.add_path(Path::new("README.md")).unwrap();
    index.write().unwrap();

    let tree_id = index.write_tree().unwrap();
    let sig = repo.signature().unwrap();
    {
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "Initial commit", &tree, &[])
            .unwrap();
    }

    repo
}

// --- sanitize_branch_name integration tests ---

#[test]
fn test_sanitize_branch_name_complex_label() {
    assert_eq!(
        sanitize_branch_name("AO/feature-branch #42"),
        "AO-feature-branch-42"
    );
}

#[test]
fn test_sanitize_branch_name_all_special() {
    assert_eq!(sanitize_branch_name("@#$%"), "");
}

#[test]
fn test_sanitize_branch_name_unicode() {
    // Unicode chars get replaced with hyphens
    let result = sanitize_branch_name("café");
    assert!(result.contains("caf"));
}

#[test]
fn test_sanitize_branch_name_preserves_hyphens_and_underscores() {
    assert_eq!(
        sanitize_branch_name("my-feature_branch-v2"),
        "my-feature_branch-v2"
    );
}

// --- build_clone_url integration tests ---

#[test]
fn test_build_clone_url_github_with_owner_and_repo() {
    let url = build_clone_url("github", "mintybasil", "yoke", None);
    assert_eq!(url, "https://github.com/mintybasil/yoke.git");
}

#[test]
fn test_build_clone_url_gitlab_self_hosted() {
    let url = build_clone_url(
        "gitlab",
        "internal-team",
        "backend",
        Some("gitlab.mycompany.com"),
    );
    assert_eq!(
        url,
        "https://gitlab.mycompany.com/internal-team/backend.git"
    );
}

// --- has_uncommitted_changes integration tests ---

#[test]
fn test_has_uncommitted_changes_with_new_file() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo_with_commit(dir.path());

    // Add a new untracked file
    fs::write(dir.path().join("new_file.txt"), "content").unwrap();

    // Include untracked files in status
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true);
    let statuses = repo.statuses(Some(&mut opts)).unwrap();
    assert!(!statuses.is_empty());

    // Our function should detect it too (when not filtering untracked)
    // Since we use statuses(None), we add the file to the index to make it staged
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("new_file.txt")).unwrap();
    index.write().unwrap();

    assert!(has_uncommitted_changes(&repo).unwrap());
}

#[test]
fn test_has_uncommitted_changes_after_commit() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo_with_commit(dir.path());

    // Clean state after commit
    assert!(!has_uncommitted_changes(&repo).unwrap());
}

#[test]
fn test_has_uncommitted_changes_modified_file() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo_with_commit(dir.path());

    // Modify an existing tracked file
    let file_path = dir.path().join("README.md");
    fs::write(&file_path, "modified content").unwrap();

    assert!(has_uncommitted_changes(&repo).unwrap());
}
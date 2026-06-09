//! Integration tests for git module lifecycle operations.
//!
//! These tests exercise local git operations (init, branch, worktree add/remove,
//! status checks) without requiring network access. They use `git2::Repository::init`
//! to create local repositories and test the full lifecycle of worktree management.

use std::fs;
use std::path::Path;

use git2::Repository;
use yoke::git::{
    build_clone_url, create_worktree, has_uncommitted_changes, pull_repo, remove_worktree,
    sanitize_branch_name,
};

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

// --- Worktree integration tests ---

#[test]
fn test_create_and_remove_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo_with_commit(dir.path());

    let worktree_dir = tempfile::tempdir().unwrap();
    let worktree_path = worktree_dir.path().join("worktree-test");

    // Create a worktree
    let result = create_worktree(&repo, "ao/test-branch", &worktree_path);
    assert!(
        result.is_ok(),
        "worktree creation failed: {:?}",
        result.err()
    );

    // Verify worktree directory exists
    assert!(worktree_path.exists());
    assert!(worktree_path.join("README.md").exists());

    // Check for uncommitted changes (should be clean)
    assert!(!has_uncommitted_changes(&repo).unwrap());

    // Remove the worktree
    let result = remove_worktree(&repo, "ao/test-branch");
    assert!(
        result.is_ok(),
        "worktree removal failed: {:?}",
        result.err()
    );

    // Verify worktree directory is gone
    assert!(!worktree_path.exists());

    // Verify branch was cleaned up
    assert!(
        repo.find_branch("ao/test-branch", git2::BranchType::Local)
            .is_err()
    );
}

#[test]
fn test_create_worktree_branch_already_exists() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo_with_commit(dir.path());

    // Create a branch manually first
    let head = repo.head().unwrap();
    let head_commit = head.peel_to_commit().unwrap();
    repo.branch("existing-branch", &head_commit, false).unwrap();

    let worktree_dir = tempfile::tempdir().unwrap();
    let worktree_path = worktree_dir.path().join("wt-existing");

    // Should succeed even though branch exists
    let result = create_worktree(&repo, "existing-branch", &worktree_path);
    assert!(
        result.is_ok(),
        "create_worktree failed for existing branch: {:?}",
        result.err()
    );

    // Clean up
    remove_worktree(&repo, "existing-branch").unwrap();
}

#[test]
fn test_create_worktree_with_slashes_in_branch_name() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo_with_commit(dir.path());

    let worktree_dir = tempfile::tempdir().unwrap();
    let worktree_path = worktree_dir.path().join("wt-slash");

    // Branch name with slashes should work (sanitized for worktree dir)
    let result = create_worktree(&repo, "ao/feature-123", &worktree_path);
    assert!(
        result.is_ok(),
        "worktree with slashes failed: {:?}",
        result.err()
    );

    // Verify worktree directory exists
    assert!(worktree_path.exists());

    // Clean up
    let result = remove_worktree(&repo, "ao/feature-123");
    assert!(result.is_ok(), "remove_worktree failed: {:?}", result.err());
}

#[test]
fn test_remove_worktree_with_dirty_tree_is_forced() {
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo_with_commit(dir.path());

    let worktree_dir = tempfile::tempdir().unwrap();
    let worktree_path = worktree_dir.path().join("wt-dirty");

    create_worktree(&repo, "dirty-branch", &worktree_path).unwrap();

    // Modify a file in the worktree
    let modified_file = worktree_path.join("README.md");
    fs::write(&modified_file, "modified content").unwrap();

    // Check that the worktree repo detects changes
    let wt_repo = Repository::open(&worktree_path).unwrap();
    assert!(has_uncommitted_changes(&wt_repo).unwrap());

    // Force-remove the worktree (the prune options handle dirty trees)
    let result = remove_worktree(&repo, "dirty-branch");
    // Should succeed because we use aggressive prune options (working_tree: true)
    assert!(
        result.is_ok(),
        "forced removal should succeed: {:?}",
        result.err()
    );
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

// --- Regression tests for issue #190 ---

#[test]
fn test_create_worktree_from_checked_out_branch() {
    // Regression: creating a worktree from a branch that is currently
    // checked out in the parent repo used to fail with
    // "reference refs/heads/main is already checked out".
    // The fix detaches HEAD before creating the worktree.
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo_with_commit(dir.path());

    // HEAD points to the default branch (master or main depending on
    // git config) — this is the exact scenario that triggered the bug.
    let head = repo.head().unwrap();
    let branch_name = head.shorthand().expect("HEAD should point to a branch");
    assert!(
        branch_name == "main" || branch_name == "master",
        "expected default branch, got: {branch_name}"
    );

    let worktree_dir = tempfile::tempdir().unwrap();
    let worktree_path = worktree_dir.path().join("wt-default");

    // This should succeed — no "already checked out" error.
    let result = create_worktree(&repo, branch_name, &worktree_path);
    assert!(
        result.is_ok(),
        "create_worktree from checked-out branch failed: {:?}",
        result.err()
    );

    // Verify worktree directory exists and has content
    assert!(worktree_path.exists());
    assert!(worktree_path.join("README.md").exists());

    remove_worktree(&repo, branch_name).unwrap();
}

#[test]
fn test_create_worktree_detaches_head() {
    // Verify that create_worktree leaves the base repo in a detached HEAD
    // state (which is safe for subsequent worktree and pull operations).
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo_with_commit(dir.path());

    let worktree_dir = tempfile::tempdir().unwrap();
    let worktree_path = worktree_dir.path().join("wt-detach");

    create_worktree(&repo, "feature-branch", &worktree_path).unwrap();

    // After create_worktree, HEAD should be detached
    let head = repo.head().unwrap();
    assert!(
        head.is_branch() == false || head.shorthand().is_none() || !head.is_branch(),
        "Expected detached HEAD after create_worktree, but HEAD points to: {:?}",
        head.shorthand()
    );

    remove_worktree(&repo, "feature-branch").unwrap();
}

#[test]
fn test_create_multiple_worktrees_same_base() {
    // Verify that creating multiple worktrees from the same base repo works
    // (this is the real-world dispatcher scenario where multiple events
    // trigger workflows for the same repo).
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo_with_commit(dir.path());

    let wt_dir1 = tempfile::tempdir().unwrap();
    let wt_path1 = wt_dir1.path().join("wt-1");
    let wt_dir2 = tempfile::tempdir().unwrap();
    let wt_path2 = wt_dir2.path().join("wt-2");

    create_worktree(&repo, "branch-1", &wt_path1).unwrap();
    assert!(wt_path1.exists());

    create_worktree(&repo, "branch-2", &wt_path2).unwrap();
    assert!(wt_path2.exists());

    remove_worktree(&repo, "branch-1").unwrap();
    remove_worktree(&repo, "branch-2").unwrap();

    assert!(!wt_path1.exists());
    assert!(!wt_path2.exists());
}

// --- Detached-HEAD + pull regression tests ---

#[test]
fn test_create_worktree_then_pull_advances_detached_head() {
    // Regression: after create_worktree detaches HEAD, pull_repo must
    // advance HEAD to the fetched tip.  Otherwise new branches created
    // in the next create_worktree call start from a stale commit.
    //
    // We can't easily test full remote pull in a unit test (no remote),
    // but we can verify the detached-HEAD contract:
    // 1. create_worktree detaches HEAD
    // 2. After creating a new branch in a detached-HEAD repo, the branch
    //    starts from HEAD's commit
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo_with_commit(dir.path());

    // Record the commit HEAD points to before worktree creation
    let head_oid_before = repo.head().unwrap().peel_to_commit().unwrap().id();

    // Create a worktree — this detaches HEAD
    let wt_dir = tempfile::tempdir().unwrap();
    let wt_path = wt_dir.path().join("wt-1");
    create_worktree(&repo, "branch-1", &wt_path).unwrap();

    // HEAD should be detached but still point to the same commit
    let head_after = repo.head().unwrap();
    assert!(
        !head_after.is_branch(),
        "HEAD should be detached after create_worktree"
    );
    assert_eq!(
        head_after.peel_to_commit().unwrap().id(),
        head_oid_before,
        "Detached HEAD should point to the same commit as before"
    );

    // A new branch created from this detached HEAD should start from the
    // same commit — this is what create_worktree does internally
    let new_branch = repo
        .branch("branch-2", &head_after.peel_to_commit().unwrap(), false)
        .unwrap();
    assert_eq!(
        new_branch.get().target().unwrap(),
        head_oid_before,
        "New branch should start from HEAD's commit"
    );

    remove_worktree(&repo, "branch-1").unwrap();
    repo.find_branch("branch-2", git2::BranchType::Local)
        .unwrap()
        .delete()
        .unwrap();
}

// --- pull_repo error tests ---

#[test]
fn test_pull_repo_on_fresh_repo_no_remote() {
    let dir = tempfile::tempdir().unwrap();
    let _repo = init_repo_with_commit(dir.path());

    // A local-only repo has no "origin" remote, so pull should fail
    let repo = Repository::open(dir.path()).unwrap();
    let result = pull_repo(&repo, "fake-token", "github");
    assert!(result.is_err());
}

#[test]
fn test_pull_repo_force_resets_diverged_branch() {
    // Verify that a branch reference can be force-reset to an earlier commit,
    // which is what pull_repo does when the local branch has diverged from
    // remote.  This tests the core git2 operation, not the full pull_repo
    // flow (which requires a remote).
    use git2::Signature;

    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo_with_commit(dir.path());

    // Save the initial commit OID — we'll reset back to this.
    let original_commit = repo.head().unwrap().peel_to_commit().unwrap().id();

    // Create a second commit to simulate local divergence.
    let sig = Signature::now("Test", "test@example.com").unwrap();
    let tree_id = {
        let mut index = repo.index().unwrap();
        index.write_tree().unwrap()
    };
    let tree = repo.find_tree(tree_id).unwrap();
    let parent = repo.find_commit(original_commit).unwrap();
    repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        "diverged commit",
        &tree,
        &[&parent],
    )
    .unwrap();

    // HEAD/branch now points at the diverged commit.  Find the branch ref
    // and force-reset it back to the original commit.
    let mut branch_ref = repo
        .find_reference("refs/heads/master")
        .or_else(|_| repo.find_reference("refs/heads/main"))
        .unwrap();
    branch_ref
        .set_target(original_commit, "force-reset to remote tip")
        .unwrap();

    // Re-read the reference from disk (not from the in-memory object).
    let branch_name = branch_ref.name().unwrap();
    let branch_ref_after = repo.find_reference(branch_name).unwrap();
    let branch_commit = branch_ref_after.peel_to_commit().unwrap();
    assert_eq!(
        branch_commit.id(),
        original_commit,
        "Branch should be force-reset to the original commit"
    );
}

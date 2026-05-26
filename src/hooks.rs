//! Pre/post step hooks for validating file conditions.
//!
//! Hooks are configured per-step in workflow TOML files and executed before
//! (pre_hooks) and after (post_hooks) each workflow step. A hook failure
//! stops the workflow and produces a clear, descriptive error message.

use std::path::Path;

/// A pre/post step hook that validates a file condition.
///
/// Hooks are configured per-step in workflow TOML files:
///
/// ```toml
/// [[steps]]
/// name = "Plan"
/// agent = "pm"
/// prompt_template = "Plan the issue"
/// pre_hooks = [{ type = "file_not_empty", path = "plan.md" }]
/// post_hooks = [{ type = "file_contains", path = "plan.md", text = "implementation" }]
/// ```
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Hook {
    /// Checks that a file exists and has non-zero content.
    ///
    /// Fails if the file is missing or empty. The error message identifies
    /// the file by its relative path.
    FileNotEmpty { path: String },

    /// Checks that a file contains a specific substring.
    ///
    /// Fails if the file is missing, empty, or does not contain the
    /// specified text. The error message identifies both the file and
    /// the missing text.
    FileContains { path: String, text: String },
}

/// Errors that can occur when running a hook.
#[derive(Debug, Clone, PartialEq)]
pub enum HookError {
    /// The file was not found at the expected path.
    FileNotFound { path: String },
    /// The file exists but is empty (zero bytes).
    FileEmpty { path: String },
    /// The file exists but does not contain the expected text.
    FileMissingText { path: String, text: String },
}

impl std::fmt::Display for HookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HookError::FileNotFound { path } => write!(f, "File '{}' not found", path),
            HookError::FileEmpty { path } => write!(f, "File '{}' is empty", path),
            HookError::FileMissingText { path, text } => {
                write!(f, "File '{}' does not contain '{}'", path, text)
            }
        }
    }
}

impl std::error::Error for HookError {}

/// Run a hook against the given workspace directory.
///
/// The `path` field in each hook variant is resolved relative to
/// `workspace_dir`. Returns `Ok(())` if the hook passes, or
/// `Err(HookError)` with a clear, descriptive message if it fails.
pub fn run_hook(hook: &Hook, workspace_dir: &Path) -> Result<(), HookError> {
    match hook {
        Hook::FileNotEmpty { path } => {
            let full_path = workspace_dir.join(path);
            let metadata = std::fs::metadata(&full_path)
                .map_err(|_| HookError::FileNotFound { path: path.clone() })?;

            if metadata.len() == 0 {
                return Err(HookError::FileEmpty { path: path.clone() });
            }
            Ok(())
        }
        Hook::FileContains { path, text } => {
            let full_path = workspace_dir.join(path);
            let content = std::fs::read_to_string(&full_path)
                .map_err(|_| HookError::FileNotFound { path: path.clone() })?;

            if content.is_empty() {
                return Err(HookError::FileEmpty { path: path.clone() });
            }

            if !content.contains(text) {
                return Err(HookError::FileMissingText {
                    path: path.clone(),
                    text: text.clone(),
                });
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Helper to create a temp dir.
    fn setup() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    // --- FileNotEmpty tests ---

    #[test]
    fn file_not_empty_passes_with_content() {
        let dir = setup();
        fs::write(dir.path().join("file.txt"), "some content").unwrap();

        let hook = Hook::FileNotEmpty {
            path: "file.txt".to_string(),
        };
        assert!(run_hook(&hook, dir.path()).is_ok());
    }

    #[test]
    fn file_not_empty_fails_on_empty_file() {
        let dir = setup();
        fs::write(dir.path().join("empty.txt"), "").unwrap();

        let hook = Hook::FileNotEmpty {
            path: "empty.txt".to_string(),
        };
        let result = run_hook(&hook, dir.path());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(
            err,
            HookError::FileEmpty {
                path: "empty.txt".to_string()
            }
        );
        assert_eq!(err.to_string(), "File 'empty.txt' is empty");
    }

    #[test]
    fn file_not_empty_fails_on_missing_file() {
        let dir = setup();

        let hook = Hook::FileNotEmpty {
            path: "missing.txt".to_string(),
        };
        let result = run_hook(&hook, dir.path());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(
            err,
            HookError::FileNotFound {
                path: "missing.txt".to_string()
            }
        );
        assert_eq!(err.to_string(), "File 'missing.txt' not found");
    }

    #[test]
    fn file_not_empty_passes_with_single_byte() {
        let dir = setup();
        fs::write(dir.path().join("a.txt"), "x").unwrap();

        let hook = Hook::FileNotEmpty {
            path: "a.txt".to_string(),
        };
        assert!(run_hook(&hook, dir.path()).is_ok());
    }

    // --- FileContains tests ---

    #[test]
    fn file_contains_passes_when_text_present() {
        let dir = setup();
        fs::write(dir.path().join("doc.md"), "Hello world").unwrap();

        let hook = Hook::FileContains {
            path: "doc.md".to_string(),
            text: "Hello".to_string(),
        };
        assert!(run_hook(&hook, dir.path()).is_ok());
    }

    #[test]
    fn file_contains_passes_when_text_equals_content() {
        let dir = setup();
        fs::write(dir.path().join("exact.txt"), "exact match").unwrap();

        let hook = Hook::FileContains {
            path: "exact.txt".to_string(),
            text: "exact match".to_string(),
        };
        assert!(run_hook(&hook, dir.path()).is_ok());
    }

    #[test]
    fn file_contains_fails_when_text_absent() {
        let dir = setup();
        fs::write(dir.path().join("doc.md"), "Hello world").unwrap();

        let hook = Hook::FileContains {
            path: "doc.md".to_string(),
            text: "Goodbye".to_string(),
        };
        let result = run_hook(&hook, dir.path());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(
            err,
            HookError::FileMissingText {
                path: "doc.md".to_string(),
                text: "Goodbye".to_string()
            }
        );
        assert_eq!(err.to_string(), "File 'doc.md' does not contain 'Goodbye'");
    }

    #[test]
    fn file_contains_fails_on_missing_file() {
        let dir = setup();

        let hook = Hook::FileContains {
            path: "gone.md".to_string(),
            text: "anything".to_string(),
        };
        let result = run_hook(&hook, dir.path());
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            HookError::FileNotFound {
                path: "gone.md".to_string()
            }
        );
    }

    #[test]
    fn file_contains_fails_on_empty_file() {
        let dir = setup();
        fs::write(dir.path().join("blank.md"), "").unwrap();

        let hook = Hook::FileContains {
            path: "blank.md".to_string(),
            text: "something".to_string(),
        };
        let result = run_hook(&hook, dir.path());
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            HookError::FileEmpty {
                path: "blank.md".to_string()
            }
        );
    }

    // --- TOML deserialization tests ---

    #[test]
    fn deserialize_file_not_empty_hook() {
        let toml = r#"
            type = "file_not_empty"
            path = "plan.md"
        "#;
        let hook: Hook = toml::from_str(toml).unwrap();
        assert_eq!(
            hook,
            Hook::FileNotEmpty {
                path: "plan.md".to_string()
            }
        );
    }

    #[test]
    fn deserialize_file_contains_hook() {
        let toml = r#"
            type = "file_contains"
            path = "output.md"
            text = "Done"
        "#;
        let hook: Hook = toml::from_str(toml).unwrap();
        assert_eq!(
            hook,
            Hook::FileContains {
                path: "output.md".to_string(),
                text: "Done".to_string()
            }
        );
    }

    #[test]
    fn serialize_file_not_empty_hook() {
        let hook = Hook::FileNotEmpty {
            path: "plan.md".to_string(),
        };
        let serialized = toml::to_string(&hook).unwrap();
        assert!(serialized.contains("file_not_empty"));
        assert!(serialized.contains("plan.md"));
    }

    #[test]
    fn serialize_file_contains_hook() {
        let hook = Hook::FileContains {
            path: "output.md".to_string(),
            text: "implementation".to_string(),
        };
        let serialized = toml::to_string(&hook).unwrap();
        assert!(serialized.contains("file_contains"));
        assert!(serialized.contains("output.md"));
        assert!(serialized.contains("implementation"));
    }
}

//! Workflow logging: write `.prompt` and `.log` files for each step.
//!
//! Files are written to the per-event workspace directory:
//! `{workdir}/{owner}/{repo}/{event_id}/`
//!
//! - `XX_<name>.prompt` — the rendered prompt template (for auditing)
//! - `XX_<name>.log` — the full HTTP exchange: request body, response body,
//!   and the extracted final message in a human-readable format

use std::fs;
use std::io;
use std::path::Path;

/// Write a rendered prompt to a `.prompt` file in the workspace directory.
///
/// The file is named `{step_num:02}_{step_name}.prompt`, e.g. `00_Plan.prompt`.
pub fn write_prompt_file(
    step_num: usize,
    step_name: &str,
    prompt: &str,
    workspace_dir: &Path,
) -> io::Result<()> {
    let filename = format!("{step_num:02}_{step_name}.prompt");
    let path = workspace_dir.join(filename);
    // Ensure the workspace directory exists
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, prompt)
}

/// Write a log file capturing the full HTTP exchange and extracted message.
///
/// The file is named `{step_num:02}_{step_name}.log`, e.g. `00_Plan.log`.
///
/// The extracted message is automatically capitalized (first letter uppercased)
/// before writing. If the message is empty or already starts with an uppercase
/// letter or non-alphabetic character, it is written as-is.
///
/// The content format is:
/// ```text
/// REQUEST:
/// <raw request body>
///
/// RESPONSE:
/// <raw response body>
///
/// FINAL MESSAGE:
/// <extracted message>
/// ```
pub fn write_log_file(
    step_num: usize,
    step_name: &str,
    request: &str,
    response: &str,
    extracted_message: &str,
    workspace_dir: &Path,
) -> io::Result<()> {
    let filename = format!("{step_num:02}_{step_name}.log");
    let path = workspace_dir.join(filename);
    // Ensure the workspace directory exists
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut final_msg = extracted_message.to_string();
    if let Some(first_char) = final_msg.chars().next()
        && first_char.is_lowercase()
    {
        final_msg.replace_range(
            ..first_char.len_utf8(),
            &first_char.to_uppercase().to_string(),
        );
    }

    let content =
        format!("REQUEST:\n{request}\n\nRESPONSE:\n{response}\n\nFINAL MESSAGE:\n{final_msg}");
    fs::write(path, content)
}

/// Write a log file with just the request portion, before the API call completes.
///
/// This ensures the request is logged even if the API call fails. After a
/// successful API call, `write_log_file` should be called to overwrite this
/// with the full exchange (request + response + extracted message).
///
/// The file is named `{step_num:02}_{step_name}.log`, same as `write_log_file`,
/// so a subsequent successful call will update it in place.
pub fn write_request_log_file(
    step_num: usize,
    step_name: &str,
    request: &str,
    workspace_dir: &Path,
) -> io::Result<()> {
    let filename = format!("{step_num:02}_{step_name}.log");
    let path = workspace_dir.join(filename);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let content = format!("REQUEST:\n{request}");
    fs::write(path, content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write_prompt_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        write_prompt_file(0, "Plan", "Hello Prompt", path).unwrap();

        let content = fs::read_to_string(path.join("00_Plan.prompt")).unwrap();
        assert_eq!(content, "Hello Prompt");
    }

    #[test]
    fn test_write_prompt_file_creates_directory() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("nested").join("dir");
        write_prompt_file(2, "Review", "Review prompt", &nested).unwrap();

        let content = fs::read_to_string(nested.join("02_Review.prompt")).unwrap();
        assert_eq!(content, "Review prompt");
    }

    #[test]
    fn test_write_log_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        write_log_file(1, "Implement", "req", "res", "msg", path).unwrap();

        let content = fs::read_to_string(path.join("01_Implement.log")).unwrap();
        assert!(content.contains("REQUEST:\nreq"));
        assert!(content.contains("RESPONSE:\nres"));
        assert!(content.contains("FINAL MESSAGE:\nMsg"));
    }

    #[test]
    fn test_write_log_file_creates_directory() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("deep").join("path");
        write_log_file(3, "Test", "r", "s", "m", &nested).unwrap();

        let content = fs::read_to_string(nested.join("03_Test.log")).unwrap();
        assert!(content.contains("REQUEST:\nr"));
        assert!(content.contains("FINAL MESSAGE:\nM"));
    }

    #[test]
    fn test_write_prompt_file_step_number_formatting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();

        write_prompt_file(0, "Plan", "p0", path).unwrap();
        write_prompt_file(9, "Review", "p9", path).unwrap();
        write_prompt_file(42, "Deploy", "p42", path).unwrap();

        assert!(path.join("00_Plan.prompt").exists());
        assert!(path.join("09_Review.prompt").exists());
        assert!(path.join("42_Deploy.prompt").exists());
    }

    #[test]
    fn test_write_log_file_full_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();

        let request = serde_json::json!({
            "instructions": "You are an expert.",
            "input": "Fix the bug.",
            "store": true
        })
        .to_string();

        write_log_file(0, "Plan", &request, "response body", "final answer", path).unwrap();

        let content = fs::read_to_string(path.join("00_Plan.log")).unwrap();
        assert!(content.starts_with("REQUEST:\n"));
        assert!(content.contains(&request));
        assert!(content.contains("RESPONSE:\nresponse body"));
        assert!(content.contains("FINAL MESSAGE:\nFinal answer"));
    }

    #[test]
    fn test_write_multistep_workflow_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();

        write_prompt_file(0, "Plan", "plan prompt", path).unwrap();
        write_log_file(
            0,
            "Plan",
            "plan request",
            "plan response",
            "plan message",
            path,
        )
        .unwrap();
        write_prompt_file(1, "Implement", "impl prompt", path).unwrap();
        write_log_file(
            1,
            "Implement",
            "impl request",
            "impl response",
            "impl message",
            path,
        )
        .unwrap();

        assert!(path.join("00_Plan.prompt").exists());
        assert!(path.join("00_Plan.log").exists());
        assert!(path.join("01_Implement.prompt").exists());
        assert!(path.join("01_Implement.log").exists());

        let plan_log = fs::read_to_string(path.join("00_Plan.log")).unwrap();
        let impl_log = fs::read_to_string(path.join("01_Implement.log")).unwrap();

        assert!(plan_log.contains("Plan message"));
        assert!(impl_log.contains("Impl message"));
    }

    #[test]
    fn test_write_request_log_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        write_request_log_file(0, "Plan", "request body", path).unwrap();

        let content = fs::read_to_string(path.join("00_Plan.log")).unwrap();
        assert!(content.starts_with("REQUEST:\n"));
        assert!(content.contains("request body"));
        // Should NOT contain RESPONSE or FINAL MESSAGE sections yet
        assert!(!content.contains("RESPONSE:"));
        assert!(!content.contains("FINAL MESSAGE:"));
    }

    #[test]
    fn test_write_request_log_then_full_log_overwrites() {
        // Simulates the real flow: write request log, then overwrite with full log
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();

        // First: write request log (before API call)
        write_request_log_file(0, "Plan", "request body", path).unwrap();

        let request_only = fs::read_to_string(path.join("00_Plan.log")).unwrap();
        assert!(request_only.starts_with("REQUEST:\n"));
        assert!(!request_only.contains("RESPONSE:"));

        // Then: overwrite with full log (after successful API call)
        write_log_file(0, "Plan", "request body", "response body", "message", path).unwrap();

        let full_log = fs::read_to_string(path.join("00_Plan.log")).unwrap();
        assert!(full_log.contains("REQUEST:\nrequest body"));
        assert!(full_log.contains("RESPONSE:\nresponse body"));
        assert!(full_log.contains("FINAL MESSAGE:\nMessage"));
    }

    #[test]
    fn test_write_log_file_capitalizes_first_letter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let lowercase_msg = "this is a lowercase message";

        write_log_file(0, "Plan", "req", "res", lowercase_msg, path).unwrap();

        let content = fs::read_to_string(path.join("00_Plan.log")).unwrap();
        assert!(content.contains("FINAL MESSAGE:\nThis is a lowercase message"));
    }

    #[test]
    fn test_write_log_file_already_capitalized_stays_same() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let msg = "Already capitalized";

        write_log_file(0, "Plan", "req", "res", msg, path).unwrap();

        let content = fs::read_to_string(path.join("00_Plan.log")).unwrap();
        assert!(content.contains("FINAL MESSAGE:\nAlready capitalized"));
    }

    #[test]
    fn test_write_log_file_empty_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();

        write_log_file(0, "Plan", "req", "res", "", path).unwrap();

        let content = fs::read_to_string(path.join("00_Plan.log")).unwrap();
        assert!(content.contains("FINAL MESSAGE:\n"));
    }

    #[test]
    fn test_write_log_file_non_ascii_first_char() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let msg = "écran"; // non-ASCII lowercase first char

        write_log_file(0, "Plan", "req", "res", msg, path).unwrap();

        let content = fs::read_to_string(path.join("00_Plan.log")).unwrap();
        assert!(content.contains("FINAL MESSAGE:\nÉcran"));
    }
}

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
    let filename = format!("{:02}_{}.prompt", step_num, step_name);
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
    let filename = format!("{:02}_{}.log", step_num, step_name);
    let path = workspace_dir.join(filename);
    // Ensure the workspace directory exists
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let content = format!(
        "REQUEST:\n{}\n\nRESPONSE:\n{}\n\nFINAL MESSAGE:\n{}",
        request, response, extracted_message
    );
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
        assert!(content.contains("FINAL MESSAGE:\nmsg"));
    }

    #[test]
    fn test_write_log_file_creates_directory() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("deep").join("path");
        write_log_file(3, "Test", "r", "s", "m", &nested).unwrap();

        let content = fs::read_to_string(nested.join("03_Test.log")).unwrap();
        assert!(content.contains("REQUEST:\nr"));
        assert!(content.contains("FINAL MESSAGE:\nm"));
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
        assert!(content.contains("FINAL MESSAGE:\nfinal answer"));
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

        assert!(plan_log.contains("plan message"));
        assert!(impl_log.contains("impl message"));
    }
}

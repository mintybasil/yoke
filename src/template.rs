use std::collections::HashMap;
use std::fmt;

/// Errors that can occur during template rendering.
#[derive(Debug)]
pub enum TemplateError {
    /// A referenced variable is not in the provided map.
    UnknownVariable { name: String },
    /// Template has malformed syntax.
    SyntaxError { message: String },
    /// The rendered output is empty or whitespace-only.
    EmptyTemplate,
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TemplateError::UnknownVariable { name } => {
                write!(f, "unknown variable: {name}")
            }
            TemplateError::SyntaxError { message } => {
                write!(f, "syntax error: {message}")
            }
            TemplateError::EmptyTemplate => write!(f, "empty template"),
        }
    }
}

impl std::error::Error for TemplateError {}

/// Render a template by substituting `{{variable}}` placeholders.
///
/// - `{{key}}` is replaced with the value of `key` from `vars`.
/// - Returns `Err(TemplateError::UnknownVariable)` if a variable is not found in `vars`.
/// - Returns `Err(TemplateError::SyntaxError)` on malformed template syntax
///   (unclosed braces, empty placeholder).
/// - Returns `Err(TemplateError::EmptyTemplate)` if the rendered result is empty or
///   whitespace-only.
///
/// # Errors
///
/// - `UnknownVariable` — a referenced variable is not in `vars`.
/// - `SyntaxError` — a `{{` without matching `}}`, or a `{{}}` with no variable name.
/// - `EmptyTemplate` — the rendered output is empty or whitespace-only.
pub fn render(template: &str, vars: &HashMap<String, String>) -> Result<String, TemplateError> {
    let mut result = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let len = bytes.len();
    let mut pos = 0;

    while pos < len {
        if bytes[pos] == b'{' && pos + 1 < len && bytes[pos + 1] == b'{' {
            // Double-brace `{{...}}`
            let open_len = 2;
            let close_marker = b"}}";
            let close_len = 2;
            let after_open = pos + open_len;

            // Find the closing `}}`
            let mut found = false;
            let mut scan = after_open;
            while scan + close_len <= len {
                if &bytes[scan..scan + close_len] == close_marker {
                    let var_name =
                        std::str::from_utf8(&bytes[after_open..scan]).expect("valid utf8");
                    if var_name.is_empty() {
                        return Err(TemplateError::SyntaxError {
                            message: "empty placeholder".to_string(),
                        });
                    }
                    let value =
                        vars.get(var_name)
                            .ok_or_else(|| TemplateError::UnknownVariable {
                                name: var_name.to_string(),
                            })?;
                    result.push_str(value);
                    pos = scan + close_len;
                    found = true;
                    break;
                }
                scan += 1;
            }
            if !found {
                return Err(TemplateError::SyntaxError {
                    message: format!("unclosed placeholder at position {pos}"),
                });
            }
        } else {
            result.push(char::from(bytes[pos]));
            pos += 1;
        }
    }

    // Check for empty/whitespace-only result
    if result.trim().is_empty() {
        return Err(TemplateError::EmptyTemplate);
    }

    Ok(result)
}

/// Extract all `{{variable}}` placeholder names from a template string.
///
/// Returns a `Vec` of variable names in order of appearance, or a
/// `TemplateError::SyntaxError` if the template contains malformed
/// placeholders (unclosed braces, empty `{{}}`).
///
/// # Errors
///
/// - `SyntaxError` — a `{{` without matching `}}`, or a `{{}}` with no variable name.
pub fn extract_variables(template: &str) -> Result<Vec<String>, TemplateError> {
    let mut variables = Vec::new();
    let bytes = template.as_bytes();
    let len = bytes.len();
    let mut pos = 0;

    while pos < len {
        if bytes[pos] == b'{' && pos + 1 < len && bytes[pos + 1] == b'{' {
            let after_open = pos + 2;
            let mut found = false;
            let mut scan = after_open;
            while scan + 2 <= len {
                if &bytes[scan..scan + 2] == b"}}" {
                    let var_name =
                        std::str::from_utf8(&bytes[after_open..scan]).expect("valid utf8");
                    if var_name.is_empty() {
                        return Err(TemplateError::SyntaxError {
                            message: "empty placeholder".to_string(),
                        });
                    }
                    variables.push(var_name.to_string());
                    pos = scan + 2;
                    found = true;
                    break;
                }
                scan += 1;
            }
            if !found {
                return Err(TemplateError::SyntaxError {
                    message: format!("unclosed placeholder at position {pos}"),
                });
            }
        } else {
            pos += 1;
        }
    }
    Ok(variables)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_basic() {
        let mut vars = HashMap::new();
        vars.insert("name".to_string(), "world".to_string());
        assert_eq!(render("Hello {{name}}", &vars).unwrap(), "Hello world");
    }

    #[test]
    fn test_substitution() {
        let mut vars = HashMap::new();
        vars.insert("owner".to_string(), "mintybasil".to_string());
        vars.insert("repo".to_string(), "yoke".to_string());
        assert_eq!(
            render("{{owner}}/{{repo}}", &vars).unwrap(),
            "mintybasil/yoke"
        );
    }

    #[test]
    fn test_exit_criteria_issue_path() {
        let mut vars = HashMap::new();
        vars.insert("owner".to_string(), "mintybasil".to_string());
        vars.insert("repo".to_string(), "yoke".to_string());
        vars.insert("issue_number".to_string(), "12".to_string());
        assert_eq!(
            render("{{owner}}/{{repo}}#{{issue_number}}", &vars).unwrap(),
            "mintybasil/yoke#12"
        );
    }

    #[test]
    fn test_no_placeholders() {
        let vars = HashMap::new();
        assert_eq!(render("plain text", &vars).unwrap(), "plain text");
    }

    #[test]
    fn test_empty_value_substitution() {
        let mut vars = HashMap::new();
        vars.insert("name".to_string(), String::new());
        assert_eq!(render("Hello {{name}}!", &vars).unwrap(), "Hello !");
    }

    #[test]
    fn test_result_is_not_empty_after_trim() {
        let mut vars = HashMap::new();
        vars.insert("v".to_string(), "content".to_string());
        assert_eq!(render("  {{v}}  ", &vars).unwrap(), "  content  ");
    }

    #[test]
    fn test_adjacent_placeholders() {
        let mut vars = HashMap::new();
        vars.insert("a".to_string(), "hello".to_string());
        vars.insert("b".to_string(), "world".to_string());
        assert_eq!(render("{{a}}{{b}}", &vars).unwrap(), "helloworld");
    }

    #[test]
    fn test_literal_braces_not_touching() {
        let vars = HashMap::new();
        assert_eq!(
            render("{not a placeholder}", &vars).unwrap(),
            "{not a placeholder}"
        );
    }

    #[test]
    fn test_unknown_variable_error() {
        let vars = HashMap::new();
        let err = render("Hello {{unknown}}", &vars).unwrap_err();
        match err {
            TemplateError::UnknownVariable { name } => assert_eq!(name, "unknown"),
            other => panic!("expected UnknownVariable, got {other:?}"),
        }
    }

    #[test]
    fn test_malformed_unclosed() {
        let vars = HashMap::new();
        let err = render("Hello {{var", &vars).unwrap_err();
        match err {
            TemplateError::SyntaxError { message } => {
                assert!(message.contains("unclosed"));
            }
            other => panic!("expected SyntaxError, got {other:?}"),
        }
    }

    #[test]
    fn test_malformed_empty() {
        let vars = HashMap::new();
        let err = render("Hello {{}}", &vars).unwrap_err();
        match err {
            TemplateError::SyntaxError { message } => {
                assert!(message.contains("empty"));
            }
            other => panic!("expected SyntaxError, got {other:?}"),
        }
    }

    #[test]
    fn test_empty_template() {
        let mut vars = HashMap::new();
        vars.insert("v".to_string(), " ".to_string());
        let err = render("  {{v}}  ", &vars).unwrap_err();
        assert!(matches!(err, TemplateError::EmptyTemplate));
    }

    #[test]
    fn test_whitespace_only_template() {
        let vars = HashMap::new();
        let err = render("   ", &vars).unwrap_err();
        assert!(matches!(err, TemplateError::EmptyTemplate));
    }

    #[test]
    fn test_whitespace_only_with_variable() {
        let mut vars = HashMap::new();
        vars.insert("v".to_string(), "\t\n".to_string());
        let err = render(" {{v}} ", &vars).unwrap_err();
        assert!(matches!(err, TemplateError::EmptyTemplate));
    }

    // --- extract_variables tests ---

    #[test]
    fn test_extract_variables_two_vars() {
        let vars = extract_variables("{{var1}} and {{var2}}").unwrap();
        assert_eq!(vars, vec!["var1", "var2"]);
    }

    #[test]
    fn test_extract_variables_single_var() {
        let vars = extract_variables("Hello {{name}}").unwrap();
        assert_eq!(vars, vec!["name"]);
    }

    #[test]
    fn test_extract_variables_no_vars() {
        let vars = extract_variables("plain text").unwrap();
        assert!(vars.is_empty());
    }

    #[test]
    fn test_extract_variables_empty_placeholder() {
        let err = extract_variables("{{}}").unwrap_err();
        match err {
            TemplateError::SyntaxError { message } => {
                assert!(message.contains("empty"));
            }
            other => panic!("expected SyntaxError, got {other:?}"),
        }
    }

    #[test]
    fn test_extract_variables_unclosed_placeholder() {
        let err = extract_variables("{{unclosed").unwrap_err();
        match err {
            TemplateError::SyntaxError { message } => {
                assert!(message.contains("unclosed"));
            }
            other => panic!("expected SyntaxError, got {other:?}"),
        }
    }

    #[test]
    fn test_extract_variables_adjacent() {
        let vars = extract_variables("{{a}}{{b}}").unwrap();
        assert_eq!(vars, vec!["a", "b"]);
    }

    #[test]
    fn test_extract_variables_literal_braces() {
        let vars = extract_variables("{not a placeholder}").unwrap();
        assert!(vars.is_empty());
    }

    #[test]
    fn test_extract_variables_repeated_var() {
        let vars = extract_variables("{{owner}}/{{repo}}#{{owner}}").unwrap();
        assert_eq!(vars, vec!["owner", "repo", "owner"]);
    }
}

use std::collections::HashMap;

/// Render a template by substituting `{{variable}}` and `{{{variable}}}` placeholders.
///
/// - `{{key}}` is replaced with the value of `key` from `vars`.
/// - `{{{key}}}` is replaced with `{value}` — the value surrounded by literal braces.
/// - Panics if a variable is not found in `vars` (unknown variable).
/// - Panics on malformed template syntax (unclosed braces, empty placeholder).
/// - Panics if the rendered result is empty or whitespace-only.
///
/// # Panics
///
/// - `unknown variable: <name>` — a referenced variable is not in `vars`.
/// - `syntax error: unclosed placeholder at position <n>` — a `{{` without matching `}}`.
/// - `syntax error: empty placeholder` — a `{{}}` with no variable name.
/// - `empty template` — the rendered output is empty or whitespace-only.
#[allow(dead_code)]
pub fn render(template: &str, vars: &HashMap<String, String>) -> String {
    let mut result = String::with_capacity(template.len());
    let chars = template.as_bytes();
    let len = chars.len();
    let mut pos = 0;

    while pos < len {
        if chars[pos] == b'{' && pos + 1 < len && chars[pos + 1] == b'{' {
            // We have at least `{{`. Check for `{{{` (triple-brace).
            let is_triple = pos + 2 < len && chars[pos + 2] == b'{';

            if is_triple {
                // Look for closing `}}}`
                let open_len = 3;
                let close_marker = b"}}}";
                let close_len = 3;
                let after_open = pos + open_len;

                // Find the closing `}}}`
                let mut found = false;
                let mut scan = after_open;
                while scan + close_len <= len {
                    if &chars[scan..scan + close_len] == close_marker {
                        // Found closing `}}}`
                        let var_name =
                            std::str::from_utf8(&chars[after_open..scan]).expect("valid utf8");
                        if var_name.is_empty() {
                            panic!("syntax error: empty placeholder");
                        }
                        let value = vars
                            .get(var_name)
                            .unwrap_or_else(|| panic!("unknown variable: {var_name}"));
                        result.push('{');
                        result.push_str(value);
                        result.push('}');
                        pos = scan + close_len;
                        found = true;
                        break;
                    }
                    scan += 1;
                }
                if !found {
                    panic!("syntax error: unclosed placeholder at position {}", pos);
                }
            } else {
                // Double-brace `{{...}}`
                let open_len = 2;
                let close_marker = b"}}";
                let close_len = 2;
                let after_open = pos + open_len;

                // Find the closing `}}`
                let mut found = false;
                let mut scan = after_open;
                while scan + close_len <= len {
                    if &chars[scan..scan + close_len] == close_marker {
                        // But we need to make sure this `}}` isn't actually part of `}}}`.
                        // Since we're in double-brace mode, `}}` at the end is the close of `{{`.
                        // However, `}}}` could be close of `{{` + literal `}`, which is ambiguous.
                        // We'll treat the first `}}` we find as the close for `{{`.
                        let var_name =
                            std::str::from_utf8(&chars[after_open..scan]).expect("valid utf8");
                        if var_name.is_empty() {
                            panic!("syntax error: empty placeholder");
                        }
                        let value = vars
                            .get(var_name)
                            .unwrap_or_else(|| panic!("unknown variable: {var_name}"));
                        result.push_str(value);
                        pos = scan + close_len;
                        found = true;
                        break;
                    }
                    scan += 1;
                }
                if !found {
                    panic!("syntax error: unclosed placeholder at position {}", pos);
                }
            }
        } else {
            result.push(char::from(chars[pos]));
            pos += 1;
        }
    }

    // Check for empty/whitespace-only result
    if result.trim().is_empty() {
        panic!("empty template");
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_basic() {
        let mut vars = HashMap::new();
        vars.insert("name".to_string(), "world".to_string());
        assert_eq!(render("Hello {{name}}", &vars), "Hello world");
    }

    #[test]
    fn test_substitution() {
        let mut vars = HashMap::new();
        vars.insert("owner".to_string(), "mintybasil".to_string());
        vars.insert("repo".to_string(), "yoke".to_string());
        assert_eq!(render("{{owner}}/{{repo}}", &vars), "mintybasil/yoke");
    }

    #[test]
    fn test_nested_braces() {
        let mut vars = HashMap::new();
        vars.insert("issue_body".to_string(), "content".to_string());
        assert_eq!(render("{{{issue_body}}}", &vars), "{content}");
    }

    #[test]
    fn test_nested_braces_with_surrounding_text() {
        let mut vars = HashMap::new();
        vars.insert("var".to_string(), "val".to_string());
        assert_eq!(
            render("prefix {{{var}}} suffix", &vars),
            "prefix {val} suffix"
        );
    }

    #[test]
    fn test_multiple_nested_braces() {
        let mut vars = HashMap::new();
        vars.insert("x".to_string(), "1".to_string());
        vars.insert("y".to_string(), "2".to_string());
        assert_eq!(render("{{{x}}} and {{{y}}}", &vars), "{1} and {2}");
    }

    #[test]
    fn test_exit_criteria_issue_path() {
        let mut vars = HashMap::new();
        vars.insert("owner".to_string(), "mintybasil".to_string());
        vars.insert("repo".to_string(), "yoke".to_string());
        vars.insert("issue_number".to_string(), "12".to_string());
        assert_eq!(
            render("{{owner}}/{{repo}}#{{issue_number}}", &vars),
            "mintybasil/yoke#12"
        );
    }

    #[test]
    fn test_no_placeholders() {
        let vars = HashMap::new();
        assert_eq!(render("plain text", &vars), "plain text");
    }

    #[test]
    fn test_empty_value_substitution() {
        let mut vars = HashMap::new();
        vars.insert("name".to_string(), String::new());
        assert_eq!(render("Hello {{name}}!", &vars), "Hello !");
    }

    #[test]
    fn test_result_is_not_empty_after_trim() {
        let mut vars = HashMap::new();
        vars.insert("v".to_string(), "content".to_string());
        assert_eq!(render("  {{v}}  ", &vars), "  content  ");
    }

    #[test]
    fn test_adjacent_placeholders() {
        let mut vars = HashMap::new();
        vars.insert("a".to_string(), "hello".to_string());
        vars.insert("b".to_string(), "world".to_string());
        assert_eq!(render("{{a}}{{b}}", &vars), "helloworld");
    }

    #[test]
    fn test_literal_braces_not_touching() {
        let vars = HashMap::new();
        assert_eq!(render("{not a placeholder}", &vars), "{not a placeholder}");
    }

    #[test]
    fn test_double_brace_with_triple_close() {
        // `{{var}}}` should render as `value}` — `{{var}}` + literal `}`
        let mut vars = HashMap::new();
        vars.insert("var".to_string(), "value".to_string());
        assert_eq!(render("{{var}}}", &vars), "value}");
    }

    #[test]
    #[should_panic(expected = "unknown variable: unknown")]
    fn test_unknown_variable_panic() {
        let vars = HashMap::new();
        render("Hello {{unknown}}", &vars);
    }

    #[test]
    #[should_panic(expected = "unknown variable: missing")]
    fn test_unknown_variable_in_nested_braces() {
        let vars = HashMap::new();
        render("Hello {{{missing}}}", &vars);
    }

    #[test]
    #[should_panic(expected = "syntax error")]
    fn test_malformed_unclosed() {
        let vars = HashMap::new();
        render("Hello {{var", &vars);
    }

    #[test]
    #[should_panic(expected = "syntax error")]
    fn test_malformed_empty() {
        let vars = HashMap::new();
        render("Hello {{}}", &vars);
    }

    #[test]
    #[should_panic(expected = "syntax error")]
    fn test_nested_brace_empty_placeholder() {
        let vars = HashMap::new();
        render("Hello {{{}}}", &vars);
    }

    #[test]
    #[should_panic(expected = "syntax error")]
    fn test_unclosed_nested_brace() {
        let vars = HashMap::new();
        render("Hello {{{var", &vars);
    }

    #[test]
    #[should_panic(expected = "empty template")]
    fn test_empty_template() {
        let mut vars = HashMap::new();
        vars.insert("v".to_string(), " ".to_string());
        render("  {{v}}  ", &vars);
    }

    #[test]
    #[should_panic(expected = "empty template")]
    fn test_whitespace_only_template() {
        let vars = HashMap::new();
        render("   ", &vars);
    }

    #[test]
    #[should_panic(expected = "empty template")]
    fn test_whitespace_only_with_variable() {
        let mut vars = HashMap::new();
        vars.insert("v".to_string(), "\t\n".to_string());
        render(" {{v}} ", &vars);
    }
}

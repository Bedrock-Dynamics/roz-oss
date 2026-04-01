//! Simple template renderer for daemon body templates.
//!
//! Substitutes `{{key}}` placeholders with values from a map.
//! No conditionals, no loops — just string replacement.
//! Keys are sorted by length descending to prevent shorter keys
//! from corrupting longer ones (e.g. `{{head}}` vs `{{head_position_x}}`).

use std::collections::HashMap;
use std::hash::BuildHasher;

/// Render a body template by replacing `{{key}}` placeholders with values.
///
/// Keys are sorted by length descending before substitution to prevent
/// `{{head}}` from corrupting `{{head_position_x}}`.
///
/// Unresolved placeholders are left as-is (the daemon will reject them,
/// giving a clear error).
pub fn render_template<S: BuildHasher>(template: &str, values: &HashMap<String, String, S>) -> String {
    let mut result = template.to_string();
    let mut keys: Vec<&String> = values.keys().collect();
    keys.sort_by_key(|b| std::cmp::Reverse(b.len()));
    for key in keys {
        result = result.replace(&format!("{{{{{key}}}}}"), &values[key]);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_simple_placeholders() {
        let mut values = HashMap::new();
        values.insert("name".into(), "world".into());
        assert_eq!(render_template("hello {{name}}", &values), "hello world");
    }

    #[test]
    fn renders_channel_names_with_underscores() {
        let mut values = HashMap::new();
        values.insert("head_pitch".into(), "0.35".into());
        values.insert("duration".into(), "1.5".into());
        let template = r#"{"pitch": {{head_pitch}}, "duration": {{duration}}}"#;
        let result = render_template(template, &values);
        assert_eq!(result, r#"{"pitch": 0.35, "duration": 1.5}"#);
    }

    #[test]
    fn leaves_unresolved_placeholders() {
        let values = HashMap::new();
        assert_eq!(render_template("{{missing}}", &values), "{{missing}}");
    }

    #[test]
    fn renders_multiple_occurrences() {
        let mut values = HashMap::new();
        values.insert("x".into(), "1.0".into());
        assert_eq!(render_template("{{x}} and {{x}}", &values), "1.0 and 1.0");
    }

    #[test]
    fn long_keys_sorted_before_short_keys() {
        let mut values = HashMap::new();
        values.insert("head".into(), "WRONG".into());
        values.insert("head_position_x".into(), "0.01".into());
        let template = "{{head_position_x}} and {{head}}";
        let result = render_template(template, &values);
        // head_position_x should be replaced first (longer), then head
        assert_eq!(result, "0.01 and WRONG");
    }

    #[test]
    fn empty_template() {
        let values = HashMap::new();
        assert_eq!(render_template("", &values), "");
    }

    #[test]
    fn no_placeholders() {
        let mut values = HashMap::new();
        values.insert("unused".into(), "value".into());
        assert_eq!(render_template("no placeholders here", &values), "no placeholders here");
    }
}

use serde_json::Value;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Blackboard
// ---------------------------------------------------------------------------

/// Typed key-value store for behavior tree data flow.
///
/// Supports parent scoping for subtree isolation: a child blackboard
/// can read from its parent but writes go only to the local scope.
#[derive(Debug, Clone)]
pub struct Blackboard {
    entries: HashMap<String, Value>,
    parent: Option<Box<Self>>,
}

impl Blackboard {
    /// Create a new empty blackboard with no parent scope.
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            parent: None,
        }
    }

    /// Create a child blackboard that delegates reads to `parent` on miss.
    pub fn child(parent: Self) -> Self {
        Self {
            entries: HashMap::new(),
            parent: Some(Box::new(parent)),
        }
    }

    /// Set a key-value pair in the local scope.
    pub fn set(&mut self, key: &str, value: Value) {
        self.entries.insert(key.to_string(), value);
    }

    /// Get a value by key, checking local scope first then parent.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries
            .get(key)
            .or_else(|| self.parent.as_ref().and_then(|p| p.get(key)))
    }

    /// Resolve a blackboard reference string.
    ///
    /// Supported formats:
    /// - `{key}` — simple lookup
    /// - `{nested.key}` — dot-path navigation into JSON objects
    /// - `{array[0].field}` — array index + field navigation
    pub fn resolve_reference(&self, reference: &str) -> Option<Value> {
        let trimmed = reference.trim();

        // Strip surrounding braces if present
        let path = if trimmed.starts_with('{') && trimmed.ends_with('}') {
            &trimmed[1..trimmed.len() - 1]
        } else {
            return None;
        };

        if path.is_empty() {
            return None;
        }

        // Split into segments on '.' while handling array indices
        let segments = parse_path_segments(path)?;
        if segments.is_empty() {
            return None;
        }

        // Resolve the first segment: look up root key, then apply array index if present
        let root_value = self.get(segments[0].name())?;
        let mut current = match &segments[0] {
            PathSegment::Field { .. } => root_value.clone(),
            PathSegment::Indexed { index, .. } => root_value.as_array()?.get(*index)?.clone(),
        };

        // Navigate remaining segments
        for segment in &segments[1..] {
            current = navigate_segment(&current, segment)?;
        }

        Some(current)
    }

    /// Remap blackboard keys according to the given mappings.
    ///
    /// For each `(old_key, new_key)` pair, the value at `old_key` is moved
    /// to `new_key`. This is used at subtree boundaries for port remapping.
    pub fn remap_ports(&mut self, mappings: &HashMap<String, String>) {
        let mut remapped = HashMap::new();
        for (old_key, new_key) in mappings {
            if let Some(value) = self.entries.remove(old_key) {
                remapped.insert(new_key.clone(), value);
            }
        }
        self.entries.extend(remapped);
    }
}

impl Default for Blackboard {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Path parsing helpers
// ---------------------------------------------------------------------------

/// A single segment in a dot-separated path.
#[derive(Debug)]
enum PathSegment {
    /// A plain field name, e.g. `key`.
    Field { name: String },
    /// A field with an array index, e.g. `array[0]`.
    Indexed { name: String, index: usize },
}

impl PathSegment {
    fn name(&self) -> &str {
        match self {
            Self::Field { name } | Self::Indexed { name, .. } => name,
        }
    }
}

/// Parse a dot-path like `sensor.readings[0].value` into segments.
fn parse_path_segments(path: &str) -> Option<Vec<PathSegment>> {
    let mut segments = Vec::new();
    for part in path.split('.') {
        let seg = if let Some(bracket_pos) = part.find('[') {
            let name = part[..bracket_pos].to_string();
            if !part.ends_with(']') {
                return None;
            }
            let index_str = &part[bracket_pos + 1..part.len() - 1];
            let index = index_str.parse::<usize>().ok()?;
            PathSegment::Indexed { name, index }
        } else {
            PathSegment::Field { name: part.to_string() }
        };
        segments.push(seg);
    }
    Some(segments)
}

/// Navigate a single path segment within a JSON value.
fn navigate_segment(value: &Value, segment: &PathSegment) -> Option<Value> {
    match segment {
        PathSegment::Field { name } => value.as_object()?.get(name).cloned(),
        PathSegment::Indexed { name, index } => {
            let obj = value.as_object()?.get(name)?;
            obj.as_array()?.get(*index).cloned()
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- Basic set/get --

    #[test]
    fn set_and_get_basic() {
        let mut bb = Blackboard::new();
        bb.set("velocity", json!(1.5));
        assert_eq!(bb.get("velocity"), Some(&json!(1.5)));
    }

    #[test]
    fn get_missing_key_returns_none() {
        let bb = Blackboard::new();
        assert_eq!(bb.get("nonexistent"), None);
    }

    // -- Parent fallback --

    #[test]
    fn parent_fallback() {
        let mut parent = Blackboard::new();
        parent.set("global_param", json!("from_parent"));

        let child = Blackboard::child(parent);
        assert_eq!(child.get("global_param"), Some(&json!("from_parent")));
    }

    #[test]
    fn local_overrides_parent() {
        let mut parent = Blackboard::new();
        parent.set("shared", json!("parent_value"));

        let mut child = Blackboard::child(parent);
        child.set("shared", json!("child_value"));

        assert_eq!(child.get("shared"), Some(&json!("child_value")));
    }

    // -- Resolve references --

    #[test]
    fn resolve_simple_reference() {
        let mut bb = Blackboard::new();
        bb.set("velocity", json!(3.14));
        let result = bb.resolve_reference("{velocity}");
        assert_eq!(result, Some(json!(3.14)));
    }

    #[test]
    fn resolve_nested_reference() {
        let mut bb = Blackboard::new();
        bb.set("sensor", json!({"temperature": 42.0, "humidity": 80}));
        let result = bb.resolve_reference("{sensor.temperature}");
        assert_eq!(result, Some(json!(42.0)));
    }

    #[test]
    fn resolve_array_index_reference() {
        let mut bb = Blackboard::new();
        bb.set("readings", json!([{"value": 10}, {"value": 20}, {"value": 30}]));
        let result = bb.resolve_reference("{readings[1].value}");
        assert_eq!(result, Some(json!(20)));
    }

    #[test]
    fn resolve_missing_key_returns_none() {
        let bb = Blackboard::new();
        let result = bb.resolve_reference("{missing}");
        assert_eq!(result, None);
    }

    #[test]
    fn resolve_invalid_reference_no_braces() {
        let mut bb = Blackboard::new();
        bb.set("key", json!(1));
        let result = bb.resolve_reference("key");
        assert_eq!(result, None);
    }

    // -- Remap ports --

    #[test]
    fn remap_ports_renames_keys() {
        let mut bb = Blackboard::new();
        bb.set("input_speed", json!(5.0));
        bb.set("input_direction", json!("north"));

        let mut mappings = HashMap::new();
        mappings.insert("input_speed".to_string(), "speed".to_string());
        mappings.insert("input_direction".to_string(), "direction".to_string());
        bb.remap_ports(&mappings);

        assert_eq!(bb.get("speed"), Some(&json!(5.0)));
        assert_eq!(bb.get("direction"), Some(&json!("north")));
        assert_eq!(bb.get("input_speed"), None);
        assert_eq!(bb.get("input_direction"), None);
    }

    // -- Empty blackboard --

    #[test]
    fn empty_blackboard_returns_none_for_all() {
        let bb = Blackboard::new();
        assert_eq!(bb.get("anything"), None);
        assert_eq!(bb.resolve_reference("{anything}"), None);
    }

    #[test]
    fn resolve_invalid_array_index_returns_none() {
        let mut bb = Blackboard::new();
        bb.set("arr", json!([1, 2, 3]));
        assert_eq!(bb.resolve_reference("{arr[abc]}"), None);
    }

    #[test]
    fn resolve_missing_bracket_close_returns_none() {
        let mut bb = Blackboard::new();
        bb.set("arr", json!([1, 2, 3]));
        assert_eq!(bb.resolve_reference("{arr[0}"), None);
    }

    // -- Child scope isolation --

    #[test]
    fn child_scope_isolation() {
        let mut parent = Blackboard::new();
        parent.set("parent_only", json!("visible"));

        let mut child = Blackboard::child(parent);
        child.set("child_only", json!("isolated"));

        // Child can see parent keys
        assert_eq!(child.get("parent_only"), Some(&json!("visible")));
        // Child has its own keys
        assert_eq!(child.get("child_only"), Some(&json!("isolated")));
    }
}

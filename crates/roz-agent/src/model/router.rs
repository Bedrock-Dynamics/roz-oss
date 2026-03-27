use super::types::{Model, ModelCapability};

/// Routes model requests to the appropriate provider based on required capability.
pub struct ModelRouter {
    providers: Vec<Box<dyn Model>>,
}

impl ModelRouter {
    pub fn new(providers: Vec<Box<dyn Model>>) -> Self {
        Self { providers }
    }

    /// Select the first provider that advertises the required capability.
    pub fn select(&self, required: ModelCapability) -> Option<&dyn Model> {
        self.providers
            .iter()
            .find(|p| p.capabilities().contains(&required))
            .map(AsRef::as_ref)
    }
}

/// Lightweight capability-to-model-name mapping for routing decisions.
///
/// Unlike `ModelRouter` (which holds live `Box<dyn Model>` instances), this
/// stores only model names and their declared capabilities. Used to decide
/// *which* model to instantiate (via `create_model`) without holding open
/// connections to all providers.
pub struct NamedModelRouter {
    entries: Vec<(String, Vec<ModelCapability>)>,
}

impl NamedModelRouter {
    #[allow(clippy::missing_const_for_fn)] // Vec::new() prevents const
    pub fn new(entries: Vec<(String, Vec<ModelCapability>)>) -> Self {
        Self { entries }
    }

    /// Return the name of the first model that advertises the required capability.
    pub fn select_for_capability(&self, required: ModelCapability) -> Option<&str> {
        self.entries
            .iter()
            .find(|(_, caps)| caps.contains(&required))
            .map(|(name, _)| name.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::types::*;

    fn text_model() -> MockModel {
        MockModel::new(vec![ModelCapability::TextReasoning], vec![])
    }

    fn spatial_model() -> MockModel {
        MockModel::new(
            vec![ModelCapability::SpatialReasoning, ModelCapability::VisionAnalysis],
            vec![],
        )
    }

    #[test]
    fn select_routes_text_reasoning_to_text_model() {
        let router = ModelRouter::new(vec![Box::new(text_model()), Box::new(spatial_model())]);
        let model = router.select(ModelCapability::TextReasoning);
        assert!(model.is_some());
        assert!(model.unwrap().capabilities().contains(&ModelCapability::TextReasoning));
    }

    #[test]
    fn select_routes_spatial_reasoning_to_spatial_model() {
        let router = ModelRouter::new(vec![Box::new(text_model()), Box::new(spatial_model())]);
        let model = router.select(ModelCapability::SpatialReasoning);
        assert!(model.is_some());
        assert!(
            model
                .unwrap()
                .capabilities()
                .contains(&ModelCapability::SpatialReasoning)
        );
    }

    #[test]
    fn select_returns_none_for_missing_capability() {
        let router = ModelRouter::new(vec![Box::new(text_model())]);
        let model = router.select(ModelCapability::EdgeInference);
        assert!(model.is_none());
    }

    #[test]
    fn select_returns_first_match_when_multiple_providers_qualify() {
        let a = MockModel::new(vec![ModelCapability::TextReasoning], vec![]);
        let b = MockModel::new(vec![ModelCapability::TextReasoning], vec![]);
        let router = ModelRouter::new(vec![Box::new(a), Box::new(b)]);
        let model = router.select(ModelCapability::TextReasoning);
        assert!(model.is_some());
    }

    #[test]
    fn empty_router_returns_none() {
        let router = ModelRouter::new(vec![]);
        assert!(router.select(ModelCapability::TextReasoning).is_none());
    }

    // --- NamedModelRouter tests ---

    #[test]
    fn named_router_selects_spatial_model() {
        let router = NamedModelRouter::new(vec![
            ("claude-haiku-4-5".to_string(), vec![ModelCapability::TextReasoning]),
            (
                "gemini-2.5-flash".to_string(),
                vec![ModelCapability::SpatialReasoning, ModelCapability::VisionAnalysis],
            ),
        ]);
        assert_eq!(
            router.select_for_capability(ModelCapability::SpatialReasoning),
            Some("gemini-2.5-flash")
        );
    }

    #[test]
    fn named_router_selects_text_model() {
        let router = NamedModelRouter::new(vec![
            ("claude-haiku-4-5".to_string(), vec![ModelCapability::TextReasoning]),
            ("gemini-2.5-flash".to_string(), vec![ModelCapability::SpatialReasoning]),
        ]);
        assert_eq!(
            router.select_for_capability(ModelCapability::TextReasoning),
            Some("claude-haiku-4-5")
        );
    }

    #[test]
    fn named_router_returns_none_for_missing_capability() {
        let router = NamedModelRouter::new(vec![(
            "claude-haiku-4-5".to_string(),
            vec![ModelCapability::TextReasoning],
        )]);
        assert!(router.select_for_capability(ModelCapability::EdgeInference).is_none());
    }

    #[test]
    fn named_router_returns_first_match() {
        let router = NamedModelRouter::new(vec![
            ("model-a".to_string(), vec![ModelCapability::SpatialReasoning]),
            ("model-b".to_string(), vec![ModelCapability::SpatialReasoning]),
        ]);
        assert_eq!(
            router.select_for_capability(ModelCapability::SpatialReasoning),
            Some("model-a")
        );
    }

    #[test]
    fn named_router_empty_returns_none() {
        let router = NamedModelRouter::new(vec![]);
        assert!(router.select_for_capability(ModelCapability::TextReasoning).is_none());
    }
}

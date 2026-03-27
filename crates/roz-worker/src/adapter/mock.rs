use async_trait::async_trait;
use roz_core::messages::CommandMsg;
use serde_json::{Value, json};

use super::HardwareAdapter;

/// A mock hardware adapter for testing.
///
/// Tracks lifecycle transitions and returns canned responses for commands.
pub struct MockAdapter {
    configured: bool,
    active: bool,
}

impl MockAdapter {
    pub const fn new() -> Self {
        Self {
            configured: false,
            active: false,
        }
    }
}

impl Default for MockAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HardwareAdapter for MockAdapter {
    async fn on_configure(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.configured = true;
        Ok(())
    }

    async fn on_activate(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.active = true;
        Ok(())
    }

    async fn on_deactivate(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.active = false;
        Ok(())
    }

    async fn on_cleanup(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.configured = false;
        Ok(())
    }

    async fn on_shutdown(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.configured = false;
        self.active = false;
        Ok(())
    }

    async fn on_error(&mut self, _error: &str) {
        self.active = false;
    }

    fn capabilities(&self) -> Vec<String> {
        vec!["mock_capability".to_string()]
    }

    async fn execute(&self, command: CommandMsg) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Ok(json!({
            "status": "ok",
            "command": command.command,
            "id": command.id,
        }))
    }

    async fn emergency_stop(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.active = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mock_adapter_default() {
        let adapter = MockAdapter::default();
        assert!(!adapter.configured);
        assert!(!adapter.active);
    }

    #[test]
    fn mock_adapter_capabilities() {
        let adapter = MockAdapter::new();
        let caps = adapter.capabilities();
        assert_eq!(caps, vec!["mock_capability".to_string()]);
    }

    #[tokio::test]
    async fn mock_adapter_configure_sets_configured() {
        let mut adapter = MockAdapter::new();
        assert!(!adapter.configured);
        adapter.on_configure().await.unwrap();
        assert!(adapter.configured);
    }

    #[tokio::test]
    async fn mock_adapter_execute_returns_canned() {
        let adapter = MockAdapter::new();
        let cmd = CommandMsg {
            id: "cmd-1".to_string(),
            command: "test".to_string(),
            params: json!({}),
            task_id: None,
        };
        let result = adapter.execute(cmd).await.unwrap();
        assert_eq!(result["status"], "ok");
        assert_eq!(result["command"], "test");
    }
}

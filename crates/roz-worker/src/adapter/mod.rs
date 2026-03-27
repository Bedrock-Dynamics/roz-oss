pub mod mock;

use async_trait::async_trait;
use roz_core::adapter::{AdapterEvent, AdapterState};
use roz_core::messages::CommandMsg;

/// Trait for hardware adapters following the ROS 2 lifecycle pattern.
///
/// Each method corresponds to a lifecycle transition callback. The
/// `AdapterManager` validates transitions using the state machine
/// before calling these methods.
#[async_trait]
pub trait HardwareAdapter: Send + Sync {
    /// Called on Configure transition (Unconfigured -> Inactive).
    async fn on_configure(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Called on Activate transition (Inactive -> Active).
    async fn on_activate(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Called on Deactivate transition (Active/SafeStop -> Inactive).
    async fn on_deactivate(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Called on Cleanup transition (Inactive -> Unconfigured).
    async fn on_cleanup(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Called on Shutdown transition (any -> Finalized).
    async fn on_shutdown(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Called when an error occurs.
    async fn on_error(&mut self, error: &str);

    /// Return a list of capabilities this adapter provides.
    fn capabilities(&self) -> Vec<String>;

    /// Execute a command while in the Active state.
    async fn execute(&self, command: CommandMsg)
    -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>>;

    /// Emergency stop — attempt to reach a safe state immediately.
    async fn emergency_stop(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

/// Wraps a `HardwareAdapter` with lifecycle state machine validation.
///
/// Validates that transitions are legal according to `AdapterState::transition()`
/// before calling the adapter's lifecycle callbacks.
pub struct AdapterManager {
    adapter: Box<dyn HardwareAdapter>,
    state: AdapterState,
}

impl AdapterManager {
    /// Create a new manager wrapping the given adapter, starting in Unconfigured.
    pub fn new(adapter: Box<dyn HardwareAdapter>) -> Self {
        Self {
            adapter,
            state: AdapterState::Unconfigured,
        }
    }

    /// Get the current adapter state.
    pub const fn state(&self) -> &AdapterState {
        &self.state
    }

    /// Attempt to configure the adapter.
    pub async fn configure(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let new_state = self
            .state
            .transition(&AdapterEvent::Configure)
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;
        if let Err(e) = self.adapter.on_configure().await {
            let msg = e.to_string();
            self.adapter.on_error(&msg).await;
            return Err(e);
        }
        self.state = new_state;
        Ok(())
    }

    /// Attempt to activate the adapter.
    pub async fn activate(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let new_state = self
            .state
            .transition(&AdapterEvent::Activate)
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;
        if let Err(e) = self.adapter.on_activate().await {
            let msg = e.to_string();
            self.adapter.on_error(&msg).await;
            return Err(e);
        }
        self.state = new_state;
        Ok(())
    }

    /// Attempt to deactivate the adapter.
    pub async fn deactivate(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let new_state = self
            .state
            .transition(&AdapterEvent::Deactivate)
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;
        if let Err(e) = self.adapter.on_deactivate().await {
            let msg = e.to_string();
            self.adapter.on_error(&msg).await;
            return Err(e);
        }
        self.state = new_state;
        Ok(())
    }

    /// Attempt to clean up the adapter.
    pub async fn cleanup(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let new_state = self
            .state
            .transition(&AdapterEvent::Cleanup)
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;
        self.adapter.on_cleanup().await?;
        self.state = new_state;
        Ok(())
    }

    /// Attempt to shut down the adapter.
    pub async fn shutdown(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let new_state = self
            .state
            .transition(&AdapterEvent::Shutdown)
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;
        self.adapter.on_shutdown().await?;
        self.state = new_state;
        Ok(())
    }

    /// Trigger an emergency stop.
    pub async fn emergency_stop(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let new_state = self
            .state
            .transition(&AdapterEvent::EmergencyStop)
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;
        if let Err(e) = self.adapter.emergency_stop().await {
            let msg = e.to_string();
            self.adapter.on_error(&msg).await;
            return Err(e);
        }
        self.state = new_state;
        Ok(())
    }

    /// Execute a command (only valid in Active state).
    pub async fn execute(
        &self,
        command: CommandMsg,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        if self.state != AdapterState::Active {
            return Err(format!(
                "cannot execute command: adapter is in {:?}, expected Active",
                self.state
            )
            .into());
        }
        self.adapter.execute(command).await
    }
}

#[cfg(test)]
mod tests {
    use super::mock::MockAdapter;
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn full_lifecycle() {
        let adapter = MockAdapter::new();
        let mut mgr = AdapterManager::new(Box::new(adapter));

        assert_eq!(mgr.state(), &AdapterState::Unconfigured);

        mgr.configure().await.unwrap();
        assert_eq!(mgr.state(), &AdapterState::Inactive);

        mgr.activate().await.unwrap();
        assert_eq!(mgr.state(), &AdapterState::Active);

        // Execute a command while active
        let cmd = CommandMsg {
            id: "cmd-1".to_string(),
            command: "test_cmd".to_string(),
            params: json!({"arg": 1}),
            task_id: None,
        };
        let result = mgr.execute(cmd).await.unwrap();
        assert_eq!(result["status"], "ok");

        mgr.deactivate().await.unwrap();
        assert_eq!(mgr.state(), &AdapterState::Inactive);

        mgr.cleanup().await.unwrap();
        assert_eq!(mgr.state(), &AdapterState::Unconfigured);

        mgr.shutdown().await.unwrap();
        assert_eq!(mgr.state(), &AdapterState::Finalized);
    }

    #[tokio::test]
    async fn emergency_stop_from_active() {
        let adapter = MockAdapter::new();
        let mut mgr = AdapterManager::new(Box::new(adapter));

        mgr.configure().await.unwrap();
        mgr.activate().await.unwrap();
        assert_eq!(mgr.state(), &AdapterState::Active);

        mgr.emergency_stop().await.unwrap();
        assert_eq!(mgr.state(), &AdapterState::SafeStop);
    }

    #[tokio::test]
    async fn invalid_transition_activate_from_unconfigured() {
        let adapter = MockAdapter::new();
        let mut mgr = AdapterManager::new(Box::new(adapter));

        assert_eq!(mgr.state(), &AdapterState::Unconfigured);

        let result = mgr.activate().await;
        assert!(result.is_err());
        // State should not have changed
        assert_eq!(mgr.state(), &AdapterState::Unconfigured);
    }

    #[tokio::test]
    async fn execute_not_in_active_state_fails() {
        let adapter = MockAdapter::new();
        let mgr = AdapterManager::new(Box::new(adapter));

        let cmd = CommandMsg {
            id: "cmd-1".to_string(),
            command: "test_cmd".to_string(),
            params: json!({}),
            task_id: None,
        };

        let result = mgr.execute(cmd).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("Active"),
            "error should mention Active state, got: {err}",
        );
    }

    #[tokio::test]
    async fn execute_returns_canned_response() {
        let adapter = MockAdapter::new();
        let mut mgr = AdapterManager::new(Box::new(adapter));

        mgr.configure().await.unwrap();
        mgr.activate().await.unwrap();

        let cmd = CommandMsg {
            id: "cmd-42".to_string(),
            command: "move_arm".to_string(),
            params: json!({"x": 1.0}),
            task_id: Some("task-1".to_string()),
        };
        let result = mgr.execute(cmd).await.unwrap();
        assert_eq!(result["status"], "ok");
        assert_eq!(result["command"], "move_arm");
    }

    #[tokio::test]
    async fn emergency_stop_from_unconfigured_fails() {
        let adapter = MockAdapter::new();
        let mut mgr = AdapterManager::new(Box::new(adapter));

        let result = mgr.emergency_stop().await;
        assert!(result.is_err());
        assert_eq!(mgr.state(), &AdapterState::Unconfigured);
    }
}

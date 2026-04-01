//! Cross-layer interface boundaries. Traits only, no implementations.

use crate::edge_health::EdgeTransportHealth;
use crate::embodiment::prediction::PredictedState;
use crate::memory::MemoryEntry;
use crate::spatial::SpatialContext;

/// Adapter for edge transport (Zenoh, NATS, etc.).
pub trait EdgeAdapter: Send + Sync {
    fn publish(&self, topic: &str, payload: &[u8]) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
    fn health(&self) -> EdgeTransportHealth;
}

/// Adapter for sensor data.
pub trait SensorAdapter: Send + Sync {
    fn read_joint_positions(&self) -> Result<Vec<f64>, Box<dyn std::error::Error + Send + Sync>>;
    fn read_joint_velocities(&self) -> Result<Vec<f64>, Box<dyn std::error::Error + Send + Sync>>;
    fn sensor_ids(&self) -> Vec<String>;
}

/// Adapter for actuator output.
pub trait ActuatorAdapter: Send + Sync {
    fn send_commands(&self, commands: &[f64]) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
    fn estop(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

/// Memory retrieval for prompt assembly.
pub trait MemoryRetriever: Send + Sync {
    fn retrieve(
        &self,
        scope: &str,
        budget_tokens: u32,
    ) -> Result<Vec<MemoryEntry>, Box<dyn std::error::Error + Send + Sync>>;
}

/// Shared blackboard for multi-agent coordination.
pub trait SharedBlackboard: Send + Sync {
    fn read_key(&self, key: &str) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>>;
    fn write_key(&self, key: &str, value: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

/// Predictive world model for verification.
pub trait WorldModelPredictor: Send + Sync {
    fn predict(
        &self,
        history: &[SpatialContext],
        actions: &[Vec<f64>],
        horizon_ticks: u32,
    ) -> Result<Vec<PredictedState>, Box<dyn std::error::Error + Send + Sync>>;
}

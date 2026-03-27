use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentKind {
    Simulation,
    Hardware,
    Hybrid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Environment {
    pub id: Uuid,
    pub tenant_id: String,
    pub name: String,
    pub kind: EnvironmentKind,
    pub framework: Option<String>,
    pub config: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Host
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostType {
    Cloud,
    Edge,
    Hybrid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostStatus {
    Online,
    Offline,
    Degraded,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Host {
    pub id: Uuid,
    pub tenant_id: String,
    pub name: String,
    pub host_type: HostType,
    pub status: HostStatus,
    pub capabilities: Vec<String>,
    pub labels: HashMap<String, String>,
    pub worker_version: Option<String>,
    pub clock_offset_ms: Option<f64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Task
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Queued,
    Provisioning,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    SafetyStop,
    Retrying,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: Uuid,
    pub tenant_id: String,
    pub prompt: String,
    pub environment_id: Uuid,
    pub skill_id: Option<Uuid>,
    pub host_id: Option<Uuid>,
    pub status: TaskStatus,
    pub timeout_secs: Option<u32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Trigger
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerType {
    Schedule,
    Webhook,
    Mqtt,
    Threshold,
    Manual,
    Integration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trigger {
    pub id: Uuid,
    pub tenant_id: String,
    pub name: String,
    pub trigger_type: TriggerType,
    pub config: serde_json::Value,
    pub task_prompt: String,
    pub environment_id: Uuid,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Skill
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub id: Uuid,
    pub tenant_id: String,
    pub name: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillVersion {
    pub id: Uuid,
    pub skill_id: Uuid,
    pub version: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Stream
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamCategory {
    Telemetry,
    Sensors,
    Video,
    Logs,
    Events,
    Commands,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stream {
    pub id: Uuid,
    pub tenant_id: String,
    pub name: String,
    pub category: StreamCategory,
    pub host_id: Option<Uuid>,
    pub rate_hz: Option<f64>,
    pub config: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    // -----------------------------------------------------------------------
    // Environment
    // -----------------------------------------------------------------------

    #[test]
    fn environment_kind_serialization() {
        assert_eq!(
            serde_json::to_string(&EnvironmentKind::Simulation).unwrap(),
            "\"simulation\""
        );
        assert_eq!(
            serde_json::to_string(&EnvironmentKind::Hardware).unwrap(),
            "\"hardware\""
        );
        assert_eq!(serde_json::to_string(&EnvironmentKind::Hybrid).unwrap(), "\"hybrid\"");
    }

    #[test]
    fn environment_serde_roundtrip() {
        let env = Environment {
            id: Uuid::new_v4(),
            tenant_id: "tenant-1".into(),
            name: "gazebo-sim".into(),
            kind: EnvironmentKind::Simulation,
            framework: Some("gazebo".into()),
            config: json!({"world": "empty.sdf"}),
            created_at: now(),
            updated_at: now(),
        };

        let json = serde_json::to_string(&env).unwrap();
        let back: Environment = serde_json::from_str(&json).unwrap();

        assert_eq!(back.id, env.id);
        assert_eq!(back.tenant_id, env.tenant_id);
        assert_eq!(back.name, env.name);
        assert_eq!(back.kind, env.kind);
        assert_eq!(back.framework, env.framework);
        assert_eq!(back.config, env.config);
    }

    // -----------------------------------------------------------------------
    // Host
    // -----------------------------------------------------------------------

    #[test]
    fn host_type_serialization() {
        assert_eq!(serde_json::to_string(&HostType::Cloud).unwrap(), "\"cloud\"");
        assert_eq!(serde_json::to_string(&HostType::Edge).unwrap(), "\"edge\"");
        assert_eq!(serde_json::to_string(&HostType::Hybrid).unwrap(), "\"hybrid\"");
    }

    #[test]
    fn host_status_serialization() {
        assert_eq!(serde_json::to_string(&HostStatus::Online).unwrap(), "\"online\"");
        assert_eq!(serde_json::to_string(&HostStatus::Offline).unwrap(), "\"offline\"");
        assert_eq!(serde_json::to_string(&HostStatus::Degraded).unwrap(), "\"degraded\"");
    }

    #[test]
    fn host_serde_roundtrip() {
        let mut labels = HashMap::new();
        labels.insert("region".into(), "us-east-1".into());
        labels.insert("gpu".into(), "true".into());

        let host = Host {
            id: Uuid::new_v4(),
            tenant_id: "tenant-1".into(),
            name: "edge-node-42".into(),
            host_type: HostType::Edge,
            status: HostStatus::Online,
            capabilities: vec!["ros2".into(), "gpu".into()],
            labels,
            worker_version: Some("0.1.0".into()),
            clock_offset_ms: Some(-2.5),
            created_at: now(),
            updated_at: now(),
        };

        let json = serde_json::to_string(&host).unwrap();
        let back: Host = serde_json::from_str(&json).unwrap();

        assert_eq!(back.id, host.id);
        assert_eq!(back.tenant_id, host.tenant_id);
        assert_eq!(back.name, host.name);
        assert_eq!(back.host_type, host.host_type);
        assert_eq!(back.status, host.status);
        assert_eq!(back.capabilities, host.capabilities);
        assert_eq!(back.labels, host.labels);
        assert_eq!(back.worker_version, host.worker_version);
        assert_eq!(back.clock_offset_ms, host.clock_offset_ms);
    }

    // -----------------------------------------------------------------------
    // Task
    // -----------------------------------------------------------------------

    #[test]
    fn task_status_all_nine_variants() {
        let variants = [
            (TaskStatus::Pending, "pending"),
            (TaskStatus::Queued, "queued"),
            (TaskStatus::Provisioning, "provisioning"),
            (TaskStatus::Running, "running"),
            (TaskStatus::Succeeded, "succeeded"),
            (TaskStatus::Failed, "failed"),
            (TaskStatus::Cancelled, "cancelled"),
            (TaskStatus::SafetyStop, "safety_stop"),
            (TaskStatus::Retrying, "retrying"),
        ];

        assert_eq!(variants.len(), 9, "TaskStatus must have exactly 9 variants");

        for (variant, expected) in &variants {
            let serialized = serde_json::to_string(variant).unwrap();
            assert_eq!(serialized, format!("\"{expected}\""));

            let deserialized: TaskStatus = serde_json::from_str(&serialized).unwrap();
            assert_eq!(&deserialized, variant);
        }
    }

    #[test]
    fn task_serde_roundtrip() {
        let task = Task {
            id: Uuid::new_v4(),
            tenant_id: "tenant-1".into(),
            prompt: "Navigate to waypoint A".into(),
            environment_id: Uuid::new_v4(),
            skill_id: Some(Uuid::new_v4()),
            host_id: None,
            status: TaskStatus::Pending,
            timeout_secs: Some(300),
            created_at: now(),
            updated_at: now(),
        };

        let json = serde_json::to_string(&task).unwrap();
        let back: Task = serde_json::from_str(&json).unwrap();

        assert_eq!(back.id, task.id);
        assert_eq!(back.tenant_id, task.tenant_id);
        assert_eq!(back.prompt, task.prompt);
        assert_eq!(back.environment_id, task.environment_id);
        assert_eq!(back.skill_id, task.skill_id);
        assert_eq!(back.host_id, task.host_id);
        assert_eq!(back.status, task.status);
        assert_eq!(back.timeout_secs, task.timeout_secs);
    }

    // -----------------------------------------------------------------------
    // Trigger
    // -----------------------------------------------------------------------

    #[test]
    fn trigger_type_serialization() {
        assert_eq!(serde_json::to_string(&TriggerType::Schedule).unwrap(), "\"schedule\"");
        assert_eq!(serde_json::to_string(&TriggerType::Webhook).unwrap(), "\"webhook\"");
        assert_eq!(serde_json::to_string(&TriggerType::Mqtt).unwrap(), "\"mqtt\"");
        assert_eq!(serde_json::to_string(&TriggerType::Threshold).unwrap(), "\"threshold\"");
        assert_eq!(serde_json::to_string(&TriggerType::Manual).unwrap(), "\"manual\"");
        assert_eq!(
            serde_json::to_string(&TriggerType::Integration).unwrap(),
            "\"integration\""
        );
    }

    #[test]
    fn trigger_serde_roundtrip() {
        let trigger = Trigger {
            id: Uuid::new_v4(),
            tenant_id: "tenant-1".into(),
            name: "nightly-patrol".into(),
            trigger_type: TriggerType::Schedule,
            config: json!({"cron": "0 0 * * *"}),
            task_prompt: "Run nightly patrol route".into(),
            environment_id: Uuid::new_v4(),
            enabled: true,
            created_at: now(),
            updated_at: now(),
        };

        let json = serde_json::to_string(&trigger).unwrap();
        let back: Trigger = serde_json::from_str(&json).unwrap();

        assert_eq!(back.id, trigger.id);
        assert_eq!(back.tenant_id, trigger.tenant_id);
        assert_eq!(back.name, trigger.name);
        assert_eq!(back.trigger_type, trigger.trigger_type);
        assert_eq!(back.config, trigger.config);
        assert_eq!(back.task_prompt, trigger.task_prompt);
        assert_eq!(back.environment_id, trigger.environment_id);
        assert_eq!(back.enabled, trigger.enabled);
    }

    // -----------------------------------------------------------------------
    // Skill
    // -----------------------------------------------------------------------

    #[test]
    fn skill_serde_roundtrip() {
        let skill = Skill {
            id: Uuid::new_v4(),
            tenant_id: "tenant-1".into(),
            name: "navigate".into(),
            description: "Navigate a robot to a waypoint".into(),
            created_at: now(),
            updated_at: now(),
        };

        let json = serde_json::to_string(&skill).unwrap();
        let back: Skill = serde_json::from_str(&json).unwrap();

        assert_eq!(back.id, skill.id);
        assert_eq!(back.tenant_id, skill.tenant_id);
        assert_eq!(back.name, skill.name);
        assert_eq!(back.description, skill.description);
    }

    #[test]
    fn skill_version_serde_roundtrip() {
        let sv = SkillVersion {
            id: Uuid::new_v4(),
            skill_id: Uuid::new_v4(),
            version: "1.2.0".into(),
            content: "steps:\n  - move_to: [1.0, 2.0]".into(),
            created_at: now(),
        };

        let json = serde_json::to_string(&sv).unwrap();
        let back: SkillVersion = serde_json::from_str(&json).unwrap();

        assert_eq!(back.id, sv.id);
        assert_eq!(back.skill_id, sv.skill_id);
        assert_eq!(back.version, sv.version);
        assert_eq!(back.content, sv.content);
    }

    // -----------------------------------------------------------------------
    // Stream
    // -----------------------------------------------------------------------

    #[test]
    fn stream_category_serialization() {
        assert_eq!(
            serde_json::to_string(&StreamCategory::Telemetry).unwrap(),
            "\"telemetry\""
        );
        assert_eq!(serde_json::to_string(&StreamCategory::Sensors).unwrap(), "\"sensors\"");
        assert_eq!(serde_json::to_string(&StreamCategory::Video).unwrap(), "\"video\"");
        assert_eq!(serde_json::to_string(&StreamCategory::Logs).unwrap(), "\"logs\"");
        assert_eq!(serde_json::to_string(&StreamCategory::Events).unwrap(), "\"events\"");
        assert_eq!(
            serde_json::to_string(&StreamCategory::Commands).unwrap(),
            "\"commands\""
        );
    }

    #[test]
    fn stream_serde_roundtrip() {
        let stream = Stream {
            id: Uuid::new_v4(),
            tenant_id: "tenant-1".into(),
            name: "lidar-front".into(),
            category: StreamCategory::Sensors,
            host_id: Some(Uuid::new_v4()),
            rate_hz: Some(10.0),
            config: json!({"topic": "/scan"}),
            created_at: now(),
            updated_at: now(),
        };

        let json = serde_json::to_string(&stream).unwrap();
        let back: Stream = serde_json::from_str(&json).unwrap();

        assert_eq!(back.id, stream.id);
        assert_eq!(back.tenant_id, stream.tenant_id);
        assert_eq!(back.name, stream.name);
        assert_eq!(back.category, stream.category);
        assert_eq!(back.host_id, stream.host_id);
        assert_eq!(back.rate_hz, stream.rate_hz);
        assert_eq!(back.config, stream.config);
    }
}

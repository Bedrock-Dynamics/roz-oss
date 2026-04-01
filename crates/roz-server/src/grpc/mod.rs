pub mod agent;
pub mod convert;
pub mod event_mapper;
pub mod tasks;

/// Generated protobuf types and gRPC service stubs for roz API v1.
#[allow(
    clippy::default_trait_access,
    clippy::derive_partial_eq_without_eq,
    clippy::doc_markdown,
    clippy::enum_variant_names,
    clippy::missing_const_for_fn,
    clippy::too_many_lines,
    clippy::wildcard_imports
)]
pub mod roz_v1 {
    tonic::include_proto!("roz.v1");

    /// File descriptor set for gRPC reflection.
    pub const FILE_DESCRIPTOR_SET: &[u8] = tonic::include_file_descriptor_set!("roz_v1_descriptor");
}

#[cfg(test)]
mod tests {
    use super::roz_v1::{CreateTaskRequest, FILE_DESCRIPTOR_SET, RegisterHostRequest, SafetyStatus, TaskStatusUpdate};

    #[test]
    fn create_task_request_has_correct_fields() {
        let req = CreateTaskRequest {
            prompt: "do something".into(),
            environment_id: "env-1".into(),
            host_id: Some("host-1".into()),
            timeout_secs: Some(300),
        };
        assert_eq!(req.prompt, "do something");
        assert_eq!(req.environment_id, "env-1");
        assert_eq!(req.host_id, Some("host-1".into()));
        assert_eq!(req.timeout_secs, Some(300));
    }

    #[test]
    fn task_status_update_has_optional_detail() {
        let update = TaskStatusUpdate {
            task_id: "task-1".into(),
            status: "running".into(),
            detail: None,
            timestamp: None,
        };
        assert_eq!(update.task_id, "task-1");
        assert!(update.detail.is_none());
    }

    #[test]
    fn register_host_request_has_capabilities_map() {
        let mut caps = std::collections::HashMap::new();
        caps.insert("arm".to_string(), "6dof".to_string());
        let req = RegisterHostRequest {
            name: "robot-1".into(),
            environment_id: "env-1".into(),
            capabilities: caps,
        };
        assert_eq!(req.name, "robot-1");
        assert_eq!(req.capabilities.get("arm").unwrap(), "6dof");
    }

    #[test]
    fn safety_status_has_active_guards() {
        let status = SafetyStatus {
            environment_id: "env-1".into(),
            estop_active: true,
            level: "critical".into(),
            active_guards: vec!["zone_guard".into(), "speed_guard".into()],
        };
        assert!(status.estop_active);
        assert_eq!(status.active_guards.len(), 2);
    }

    #[test]
    fn file_descriptor_set_is_not_empty() {
        assert!(!FILE_DESCRIPTOR_SET.is_empty());
    }
}

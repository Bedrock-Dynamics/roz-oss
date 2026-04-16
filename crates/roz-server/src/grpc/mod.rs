pub mod agent;
pub mod auth_ext;
pub mod convert;
pub mod embodiment;
pub mod embodiment_convert;
pub mod event_mapper;
pub mod mcp;
pub mod media;
pub mod media_fetch;
pub mod session_bus;
pub mod skills;
pub mod tasks;

/// Generated protobuf types and gRPC service stubs for roz API v1.
#[allow(
    clippy::default_trait_access,
    clippy::derive_partial_eq_without_eq,
    clippy::doc_markdown,
    clippy::enum_variant_names,
    clippy::missing_const_for_fn,
    clippy::too_long_first_doc_paragraph,
    clippy::too_many_lines,
    clippy::wildcard_imports
)]
pub mod roz_v1 {
    tonic::include_proto!("roz.v1");

    /// File descriptor set for gRPC reflection.
    pub const FILE_DESCRIPTOR_SET: &[u8] = tonic::include_file_descriptor_set!("roz_v1_descriptor");
}

#[cfg(test)]
#[allow(unused_imports)]
mod tests {
    use super::roz_v1::{
        // Enums
        BindingType,
        // Embodiment imports
        CalibrationOverlay,
        // Misc
        CameraFrustum,
        CameraResolution,
        ChannelBinding,
        CollisionBody,
        CollisionPair,
        CommandInterfaceType,
        ContactForceEnvelope,
        ControlChannelDef,
        ControlInterfaceManifest,
        // Existing imports
        CreateTaskRequest,
        EmbodimentFamily,
        EmbodimentModel,
        EmbodimentRuntime,
        FILE_DESCRIPTOR_SET,
        ForceSafetyLimits,
        FrameNode,
        FrameSource,
        FrameTree,
        Geometry,
        // Embodiment request/response types
        GetModelRequest,
        GetRuntimeRequest,
        Joint,
        JointSafetyLimits,
        JointType,
        Link,
        ListBindingsRequest,
        ListBindingsResponse,
        Quaternion,
        RegisterHostRequest,
        SafetyOverlay,
        SafetyStatus,
        SemanticRole,
        SensorCalibration,
        SensorMount,
        SensorType,
        TaskStatusUpdate,
        TcpType,
        ToolCenterPoint,
        Transform3D,
        UnboundChannel,
        ValidateBindingsRequest,
        ValidateBindingsResponse,
        Vec3,
        WorkspaceShape,
        WorkspaceZone,
        ZoneType,
    };

    #[test]
    fn create_task_request_has_correct_fields() {
        let req = CreateTaskRequest {
            prompt: "do something".into(),
            environment_id: "env-1".into(),
            host_id: "host-1".into(),
            timeout_secs: Some(300),
            control_interface_manifest: None,
            delegation_scope: None,
            phases: vec![],
            parent_task_id: None,
        };
        assert_eq!(req.prompt, "do something");
        assert_eq!(req.environment_id, "env-1");
        assert_eq!(req.host_id, "host-1");
        assert_eq!(req.timeout_secs, Some(300));
        assert!(req.phases.is_empty());
        assert!(req.parent_task_id.is_none());
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
        let mut caps = std::collections::BTreeMap::new();
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

    // -----------------------------------------------------------------------
    // Embodiment proto generated-type verification tests
    // -----------------------------------------------------------------------

    #[test]
    fn embodiment_model_has_all_fields() {
        let model = EmbodimentModel {
            model_id: "test".into(),
            model_digest: "abc".into(),
            embodiment_family: Some(EmbodimentFamily {
                family_id: "arm".into(),
                description: "test arm".into(),
            }),
            links: vec![],
            joints: vec![],
            frame_tree: Some(FrameTree {
                frames: std::collections::BTreeMap::new(),
                root: Some("world".into()),
            }),
            collision_bodies: vec![],
            allowed_collision_pairs: vec![CollisionPair {
                link_a: "a".into(),
                link_b: "b".into(),
            }],
            tcps: vec![],
            sensor_mounts: vec![],
            workspace_zones: vec![],
            watched_frames: vec!["world".into()],
            channel_bindings: vec![],
        };
        assert_eq!(model.model_id, "test");
        assert!(model.embodiment_family.is_some());
        assert_eq!(model.allowed_collision_pairs.len(), 1);
        assert_eq!(model.watched_frames.len(), 1);
    }

    #[test]
    fn embodiment_runtime_has_digest_fields() {
        let runtime = EmbodimentRuntime {
            model: Some(EmbodimentModel {
                model_id: "r".into(),
                model_digest: String::new(),
                embodiment_family: None,
                links: vec![],
                joints: vec![],
                frame_tree: None,
                collision_bodies: vec![],
                allowed_collision_pairs: vec![],
                tcps: vec![],
                sensor_mounts: vec![],
                workspace_zones: vec![],
                watched_frames: vec![],
                channel_bindings: vec![],
            }),
            calibration: None,
            safety_overlay: None,
            model_digest: "md".into(),
            calibration_digest: "cd".into(),
            safety_digest: "sd".into(),
            combined_digest: "combined".into(),
            frame_graph: None,
            active_calibration_id: Some("cal-1".into()),
            joint_count: 6,
            tcp_count: 1,
            watched_frames: vec![],
            validation_issues: vec![],
        };
        assert_eq!(runtime.combined_digest, "combined");
        assert_eq!(runtime.joint_count, 6);
        assert!(runtime.active_calibration_id.is_some());
    }

    #[test]
    fn quaternion_has_xyzw_fields() {
        let q = Quaternion {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            w: 1.0,
        };
        assert!((q.w - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn joint_safety_limits_has_optional_torque() {
        let limits = JointSafetyLimits {
            joint_name: "shoulder".into(),
            max_velocity: 2.0,
            max_acceleration: 5.0,
            max_jerk: 50.0,
            position_min: -3.14,
            position_max: 3.14,
            max_torque: Some(40.0),
        };
        assert_eq!(limits.max_torque, Some(40.0));

        let no_torque = JointSafetyLimits {
            max_torque: None,
            ..limits
        };
        assert!(no_torque.max_torque.is_none());
    }

    #[test]
    fn frame_tree_uses_btree_map() {
        let mut frames = std::collections::BTreeMap::new();
        frames.insert(
            "world".to_string(),
            FrameNode {
                frame_id: "world".into(),
                parent_id: None,
                static_transform: Some(Transform3D {
                    translation: Some(Vec3 { x: 0.0, y: 0.0, z: 0.0 }),
                    rotation: Some(Quaternion {
                        x: 0.0,
                        y: 0.0,
                        z: 0.0,
                        w: 1.0,
                    }),
                    timestamp_ns: 0,
                }),
                source: FrameSource::Static.into(),
            },
        );
        let tree = FrameTree {
            frames,
            root: Some("world".into()),
        };
        assert_eq!(tree.frames.len(), 1);
        // BTreeMap is confirmed by the fact that this compiles with BTreeMap::new()
    }

    #[test]
    fn calibration_overlay_has_map_fields() {
        let cal = CalibrationOverlay {
            calibration_id: "cal-1".into(),
            calibration_digest: "dig".into(),
            calibrated_at: None,
            stale_after: None,
            joint_offsets: std::collections::BTreeMap::from([("j1".into(), 0.01)]),
            frame_corrections: std::collections::BTreeMap::new(),
            sensor_calibrations: std::collections::BTreeMap::new(),
            temperature_min: Some(15.0),
            temperature_max: Some(35.0),
            valid_for_model_digest: "model_abc".into(),
        };
        assert_eq!(cal.joint_offsets.len(), 1);
        assert_eq!(cal.temperature_min, Some(15.0));
    }

    #[test]
    fn safety_overlay_has_optional_payload() {
        let overlay = SafetyOverlay {
            overlay_digest: "d".into(),
            workspace_restrictions: vec![],
            joint_limit_overrides: std::collections::BTreeMap::new(),
            max_payload_kg: Some(5.0),
            human_presence_zones: vec![],
            force_limits: None,
            contact_force_envelopes: vec![],
            contact_allowed_zones: vec![],
            force_rate_limits: std::collections::BTreeMap::new(),
        };
        assert_eq!(overlay.max_payload_kg, Some(5.0));
    }

    #[test]
    fn embodiment_service_trait_is_generated() {
        // This test proves the EmbodimentService trait exists and has the expected RPCs.
        // We don't implement it here (Phase 4), just confirm the generated code resolves.
        fn _assert_trait_exists<T: super::roz_v1::embodiment_service_server::EmbodimentService>() {}
    }
}

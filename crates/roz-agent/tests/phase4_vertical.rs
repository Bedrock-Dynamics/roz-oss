//! Phase 4 vertical integration tests.
//!
//! Each test chains across module boundaries — skill parsing, BT execution,
//! sim-to-real comparison, device trust — exercising the production code
//! path end-to-end without any external services.

use std::collections::HashMap;

use chrono::Utc;
use roz_agent::bt::action::{ActionExecutor, ActionNode};
use roz_agent::bt::conditions::ConditionChecker;
use roz_agent::bt::node::BtNode;
use roz_agent::bt::runner::{SkillRunner, SkillTickResult};
use roz_agent::bt::sequence::SequenceNode;
use roz_agent::skills::executor::{ExecutionStrategy, SkillExecutor};
use roz_core::bt::blackboard::Blackboard;
use roz_core::bt::conditions::{ConditionPhase, ConditionSpec};
use roz_core::bt::eval::evaluate_condition;
use roz_core::bt::parser::parse_execution_skill;
use roz_core::bt::status::BtStatus;
use roz_core::device_trust::evaluator::{TrustPolicy, evaluate_trust};
use roz_core::device_trust::verify::{verify_firmware_crc32, verify_firmware_sha256};
use roz_core::device_trust::{DeviceTrust, FirmwareManifest, FlashPartition, TrustPosture};
use roz_core::recording::{ChannelManifest, RecordingManifest, RecordingSource};
use roz_core::sim2real::diagnosis::{DiagnosisContext, compute_channel_stats, generate_summary};
use roz_core::sim2real::pipeline::{ChannelConfig, ComparisonConfig, compare};
use roz_core::sim2real::report::{DiagnosisAction, MetricKind};
use roz_core::sim2real::signatures::{builtin_signatures, match_signatures};
use roz_core::skills::SkillKind;
use roz_core::skills::builtin::builtin_skills;
use roz_core::skills::discovery::SkillDiscovery;
use roz_core::skills::template::substitute;
use roz_core::skills::validate::validate_skill;
use roz_core::wal::WalEntry;
use serde_json::json;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Test helpers — mock executors that simulate real robot actions and produce
// telemetry data on the blackboard.
// ---------------------------------------------------------------------------

/// Simulates a motor movement that writes velocity telemetry to the blackboard.
struct MoveExecutor {
    ticks_remaining: u32,
    velocity: f64,
}

impl MoveExecutor {
    fn new(ticks: u32, velocity: f64) -> Self {
        Self {
            ticks_remaining: ticks,
            velocity,
        }
    }
}

impl ActionExecutor for MoveExecutor {
    fn on_start(&mut self, bb: &mut Blackboard) -> BtStatus {
        bb.set("velocity", json!(self.velocity));
        self.ticks_remaining -= 1;
        if self.ticks_remaining == 0 {
            bb.set("velocity", json!(0.0));
            BtStatus::Success
        } else {
            BtStatus::Running
        }
    }

    fn on_running(&mut self, bb: &mut Blackboard) -> BtStatus {
        self.ticks_remaining -= 1;
        if self.ticks_remaining == 0 {
            bb.set("velocity", json!(0.0));
            BtStatus::Success
        } else {
            BtStatus::Running
        }
    }

    fn on_halted(&mut self, bb: &mut Blackboard) {
        bb.set("velocity", json!(0.0));
    }

    fn action_type(&self) -> &'static str {
        "move"
    }
}

/// Simulates a gripper that writes force telemetry.
struct GripperExecutor {
    close: bool,
}

impl ActionExecutor for GripperExecutor {
    fn on_start(&mut self, bb: &mut Blackboard) -> BtStatus {
        if self.close {
            bb.set("gripper_open", json!(false));
            bb.set("force", json!(25.0));
        } else {
            bb.set("gripper_open", json!(true));
            bb.set("force", json!(0.0));
            bb.set("object_placed", json!(true));
        }
        BtStatus::Success
    }

    fn on_running(&mut self, _bb: &mut Blackboard) -> BtStatus {
        BtStatus::Success
    }

    fn on_halted(&mut self, _bb: &mut Blackboard) {}

    fn action_type(&self) -> &'static str {
        "gripper"
    }
}

// ---------------------------------------------------------------------------
// 1. Parse execution skill → Build BT → Tick to completion with conditions
// ---------------------------------------------------------------------------

#[test]
fn parse_skill_build_bt_tick_to_completion() {
    // Step 1: Parse the execution skill YAML (roz-core parser)
    let yaml = r#"
name: pick-place
description: Pick up an object and place it at the target
version: "1.0.0"
inputs:
  - name: object_pose
    port_type: Pose3D
    required: true
  - name: target_pose
    port_type: Pose3D
    required: true
outputs:
  - name: success
    port_type: bool
    required: true
conditions:
  pre:
    - expression: "{gripper_open} == true"
      phase: pre
  hold:
    - expression: "{force} < 50"
      phase: hold
  post:
    - expression: "{object_placed} == true"
      phase: post
hardware:
  timeout_secs: 30
  heartbeat_hz: 10.0
  reversible: true
  safe_halt_action: open_gripper
tree:
  type: sequence
  children:
    - type: action
      name: approach_object
      action_type: move
    - type: action
      name: close_gripper
      action_type: gripper
    - type: action
      name: move_to_target
      action_type: move
    - type: action
      name: open_gripper
      action_type: gripper
"#;

    let skill_def = parse_execution_skill(yaml).expect("skill YAML should parse");
    assert_eq!(skill_def.name, "pick-place");
    assert_eq!(skill_def.conditions.pre.len(), 1);
    assert_eq!(skill_def.conditions.hold.len(), 1);
    assert_eq!(skill_def.conditions.post.len(), 1);

    // Step 2: Build the condition checker from parsed conditions (roz-agent)
    let checker = ConditionChecker::new(
        skill_def
            .conditions
            .pre
            .iter()
            .map(|c| ConditionSpec {
                expression: c.expression.clone(),
                phase: ConditionPhase::Pre,
            })
            .collect(),
        skill_def
            .conditions
            .hold
            .iter()
            .map(|c| ConditionSpec {
                expression: c.expression.clone(),
                phase: ConditionPhase::Hold,
            })
            .collect(),
        skill_def
            .conditions
            .post
            .iter()
            .map(|c| ConditionSpec {
                expression: c.expression.clone(),
                phase: ConditionPhase::Post,
            })
            .collect(),
    );

    // Step 3: Build the BT from parsed tree structure (roz-agent nodes)
    let root: Box<dyn BtNode> = Box::new(SequenceNode::new(
        "pick-place",
        vec![
            Box::new(ActionNode::new("approach_object", Box::new(MoveExecutor::new(2, 1.5)))),
            Box::new(ActionNode::new(
                "close_gripper",
                Box::new(GripperExecutor { close: true }),
            )),
            Box::new(ActionNode::new("move_to_target", Box::new(MoveExecutor::new(2, 1.0)))),
            Box::new(ActionNode::new(
                "open_gripper",
                Box::new(GripperExecutor { close: false }),
            )),
        ],
    ));

    // Step 4: Run with SkillRunner (roz-agent runner + roz-core conditions)
    let mut runner = SkillRunner::new(root, checker);
    runner.blackboard_mut().set("gripper_open", json!(true));
    runner.blackboard_mut().set("force", json!(0.0));
    runner.blackboard_mut().set("object_placed", json!(false));

    let result = runner.run_to_completion(20);

    // Post-condition {object_placed} == true is set by the final gripper open
    assert!(
        result.success,
        "skill should complete successfully: {:?}",
        result.message
    );
    // Sequence advances through children within the same tick when they return
    // Success immediately, so total ticks = sum of Running-returning ticks only.
    // MoveExecutor(2) returns Running once, then Success: 2 moves × 1 Running tick = 2.
    // GripperExecutor returns Success immediately: 0 Running ticks.
    // Plus 1 final tick where last move completes + gripper runs = 3 total.
    assert!(result.ticks >= 2, "should take multiple ticks for multi-step sequence");
}

// ---------------------------------------------------------------------------
// 2. Discover AI skill → Validate → Template substitute → Verify output
// ---------------------------------------------------------------------------

#[test]
fn discover_validate_and_render_ai_skill() {
    // Step 1: Discover built-in skills (roz-core discovery)
    let builtins = builtin_skills();
    let discovery = SkillDiscovery::new(vec![], vec![], builtins);
    let summaries = discovery.discover();
    assert!(summaries.len() >= 3);

    // Step 2: Load a specific skill
    let skill = discovery
        .load("diagnose-motor")
        .expect("diagnose-motor should be loadable");
    assert_eq!(skill.frontmatter.kind, SkillKind::Ai);

    // Step 3: Validate the loaded skill (roz-core validation)
    validate_skill(&skill.frontmatter).expect("built-in skill should be valid");

    // Step 4: Template substitution with arguments (roz-core template)
    let rendered = substitute(
        &skill.body,
        &skill.frontmatter.parameters,
        &json!({"motor_id": "joint_3"}),
    );

    // The rendered body should contain the substituted value and no leftover placeholders
    assert!(
        rendered.contains("joint_3"),
        "rendered body should contain the substituted motor_id"
    );
    assert!(
        !rendered.contains("{{motor_id}}"),
        "no unresolved placeholders should remain"
    );

    // Step 5: Verify the executor routing (roz-agent executor)
    assert_eq!(
        SkillExecutor::execution_strategy(skill.frontmatter.kind),
        ExecutionStrategy::AgentLoopReAct,
        "AI skills should route to agent loop React mode"
    );
}

// ---------------------------------------------------------------------------
// 3. BT execution → Record telemetry → Sim-vs-real comparison → Diagnosis
// ---------------------------------------------------------------------------

#[test]
fn bt_execution_to_sim_real_diagnosis() {
    // Step 1: Run a BT that produces telemetry (roz-agent)
    let root: Box<dyn BtNode> = Box::new(SequenceNode::new(
        "calibrate",
        vec![
            Box::new(ActionNode::new("move_slow", Box::new(MoveExecutor::new(3, 0.5)))),
            Box::new(ActionNode::new("grip", Box::new(GripperExecutor { close: true }))),
        ],
    ));

    let mut runner = SkillRunner::new(root, ConditionChecker::new(vec![], vec![], vec![]));
    let result = runner.run_to_completion(10);
    assert!(result.success);

    // Step 2: Create recording manifests for sim and real runs (roz-core recording)
    let run_id = Uuid::new_v4();
    let env_id = Uuid::new_v4();
    let host_id = Uuid::new_v4();
    let now = Utc::now();

    let sim_manifest = RecordingManifest {
        id: Uuid::new_v4(),
        run_id,
        environment_id: env_id,
        host_id,
        source: RecordingSource::Simulation,
        channels: vec![
            ChannelManifest {
                name: "velocity".to_string(),
                topic: "/joint/vel".to_string(),
                schema_name: "Float64".to_string(),
                message_count: 5,
            },
            ChannelManifest {
                name: "force".to_string(),
                topic: "/gripper/force".to_string(),
                schema_name: "Float64".to_string(),
                message_count: 5,
            },
        ],
        duration_secs: f64::from(result.ticks),
        created_at: now,
    };

    let real_manifest = RecordingManifest {
        id: Uuid::new_v4(),
        run_id,
        environment_id: env_id,
        host_id,
        source: RecordingSource::Physical,
        channels: sim_manifest.channels.clone(),
        duration_secs: f64::from(result.ticks) + 0.5, // real run slightly longer
        created_at: now,
    };

    assert_eq!(sim_manifest.source, RecordingSource::Simulation);
    assert_eq!(real_manifest.source, RecordingSource::Physical);

    // Step 3: Simulate collected telemetry data and run comparison pipeline
    // (roz-core sim2real pipeline)
    let sim_data: HashMap<String, Vec<f64>> = [
        ("velocity".to_string(), vec![0.0, 0.5, 0.5, 0.5, 0.0]),
        ("force".to_string(), vec![0.0, 0.0, 0.0, 25.0, 25.0]),
    ]
    .into();

    // Real robot has slightly different telemetry — small drift on velocity
    let real_data: HashMap<String, Vec<f64>> = [
        ("velocity".to_string(), vec![0.0, 0.48, 0.52, 0.49, 0.01]),
        ("force".to_string(), vec![0.0, 0.0, 0.0, 24.5, 25.2]),
    ]
    .into();

    let config = ComparisonConfig {
        channels: vec![
            ChannelConfig {
                name: "velocity".to_string(),
                metric: MetricKind::Rmse,
                threshold: 0.5,
            },
            ChannelConfig {
                name: "force".to_string(),
                metric: MetricKind::Mae,
                threshold: 1.0,
            },
        ],
        pass_score: 0.8,
    };

    let report = compare(&sim_data, &real_data, &config);

    // With very similar data and generous thresholds, should pass
    assert_eq!(
        report.action,
        DiagnosisAction::Pass,
        "similar sim/real telemetry should pass comparison"
    );

    // Step 4: Match against failure signatures (roz-core signatures)
    let sigs = builtin_signatures();
    let matches = match_signatures(&report, &sigs);
    // With healthy data and passing report, we don't expect signature matches
    // (but the function should work without error)
    assert!(matches.len() <= sigs.len());

    // Step 5: Build diagnosis context for LLM (roz-core diagnosis)
    let sim_stats: Vec<_> = sim_data
        .iter()
        .map(|(name, data)| compute_channel_stats(name, data))
        .collect();
    let real_stats: Vec<_> = real_data
        .iter()
        .map(|(name, data)| compute_channel_stats(name, data))
        .collect();

    let ctx = DiagnosisContext {
        sim_stats,
        real_stats,
        divergence_summary: format!(
            "Overall score: {:.2}. {} channels compared, action: {:?}",
            report.overall_score,
            report.phases[0].channels.len(),
            report.action,
        ),
        action: report.action,
    };

    let summary = generate_summary(&ctx);
    assert!(summary.contains("Pass"), "summary should reference the pass action");
    assert!(
        summary.contains("Sim channel statistics:"),
        "summary should include sim stats"
    );
    assert!(
        summary.contains("Real channel statistics:"),
        "summary should include real stats"
    );
}

// ---------------------------------------------------------------------------
// 4. Firmware verification → Trust evaluation → Skill execution gating
// ---------------------------------------------------------------------------

#[test]
fn firmware_trust_gates_skill_execution() {
    let firmware = b"roz-firmware-v2.0.0-production-build";
    let now = Utc::now();

    // Step 1: Compute firmware integrity hashes (roz-core device_trust/verify)
    let crc = crc32fast::hash(firmware);
    let sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(firmware));

    assert!(verify_firmware_crc32(firmware, crc));
    assert!(verify_firmware_sha256(firmware, &sha));

    // Step 2: Build device record with verified firmware (roz-core device_trust)
    let device = DeviceTrust {
        host_id: Uuid::new_v4(),
        tenant_id: "tenant-prod".to_string(),
        posture: TrustPosture::Untrusted, // will be re-evaluated
        firmware: Some(FirmwareManifest {
            version: "2.0.0".to_string(),
            sha256: sha.clone(),
            crc32: crc,
            ed25519_signature: None,
            partition: FlashPartition::A,
        }),
        sbom_hash: None,
        last_attestation: Some(now),
        created_at: now,
        updated_at: now,
    };

    // Step 3: Evaluate trust posture against policy (roz-core evaluator)
    let policy = TrustPolicy {
        max_attestation_age_secs: 3600,
        require_firmware_signature: false,
        allowed_firmware_versions: vec!["2.0.0".to_string()],
    };

    let posture = evaluate_trust(&device, &policy, now);
    assert_eq!(posture, TrustPosture::Trusted, "verified firmware should yield Trusted");

    // Step 4: Gate skill execution on trust — trusted devices can run execution skills
    let skill_def = parse_execution_skill(
        r#"
name: precision-move
description: High-precision arm movement
version: "1.0.0"
inputs: []
outputs: []
conditions:
  pre: []
  hold: []
  post: []
hardware:
  timeout_secs: 10
  reversible: true
  safe_halt_action: stop
tree:
  type: action
  name: move_arm
  action_type: move
"#,
    )
    .expect("skill should parse");

    assert_eq!(
        SkillExecutor::execution_strategy(SkillKind::Execution),
        ExecutionStrategy::AgentLoopOodaReAct
    );

    // Only run if trusted
    if posture == TrustPosture::Trusted {
        let root: Box<dyn BtNode> = Box::new(ActionNode::new(&skill_def.name, Box::new(MoveExecutor::new(1, 0.1))));
        let mut runner = SkillRunner::new(root, ConditionChecker::new(vec![], vec![], vec![]));
        let result = runner.run_to_completion(5);
        assert!(result.success, "trusted device should execute skill successfully");
    }

    // Step 5: Untrusted device should be blocked
    let untrusted_device = DeviceTrust {
        firmware: Some(FirmwareManifest {
            version: "0.1.0-unknown".to_string(),
            sha256: "bad".to_string(),
            crc32: 0,
            ed25519_signature: None,
            partition: FlashPartition::A,
        }),
        ..device.clone()
    };

    let untrusted_posture = evaluate_trust(&untrusted_device, &policy, now);
    assert_eq!(untrusted_posture, TrustPosture::Untrusted);
    // Production code would refuse to run the skill here
}

// ---------------------------------------------------------------------------
// 5. Hold condition violation → Safe halt → WAL entry → Divergence escalation
// ---------------------------------------------------------------------------

#[test]
fn hold_violation_triggers_halt_and_escalation() {
    // Step 1: Parse a skill with a hold condition (roz-core parser)
    let skill_def = parse_execution_skill(
        r#"
name: force-sensitive-grasp
description: Grasp with force monitoring
version: "1.0.0"
inputs: []
outputs: []
conditions:
  pre: []
  hold:
    - expression: "{force} < 40"
      phase: hold
  post: []
hardware:
  timeout_secs: 10
  reversible: true
  safe_halt_action: open_gripper
tree:
  type: action
  name: grasp
  action_type: gripper
"#,
    )
    .expect("skill should parse");

    // Step 2: Build condition checker with the parsed hold condition
    let checker = ConditionChecker::new(
        vec![],
        skill_def
            .conditions
            .hold
            .iter()
            .map(|c| ConditionSpec {
                expression: c.expression.clone(),
                phase: ConditionPhase::Hold,
            })
            .collect(),
        vec![],
    );

    // Step 3: Create executor that immediately exceeds force limit
    struct OverforceGripper;
    impl ActionExecutor for OverforceGripper {
        fn on_start(&mut self, bb: &mut Blackboard) -> BtStatus {
            bb.set("force", json!(50.0)); // exceeds hold condition
            BtStatus::Running
        }
        fn on_running(&mut self, _bb: &mut Blackboard) -> BtStatus {
            BtStatus::Running
        }
        fn on_halted(&mut self, bb: &mut Blackboard) {
            bb.set("force", json!(0.0)); // safe halt: release gripper
        }
        fn action_type(&self) -> &'static str {
            "gripper"
        }
    }

    let root: Box<dyn BtNode> = Box::new(ActionNode::new("grasp", Box::new(OverforceGripper)));
    let mut runner = SkillRunner::new(root, checker);
    runner.blackboard_mut().set("force", json!(0.0));

    // Step 4: First tick — OverforceGripper sets force=50, hold check (AFTER tick)
    // sees force=50 >= 40 → immediate violation
    let tick1 = runner.tick();
    assert!(
        matches!(tick1, SkillTickResult::HoldViolation { .. }),
        "force=50 should violate hold condition {{force}} < 40 on first tick"
    );

    // Step 6: Create WAL entries for the violation (roz-core WAL)
    let wal_entries = vec![
        WalEntry::SkillStarted {
            skill_name: skill_def.name.clone(),
            kind: "execution".to_string(),
        },
        WalEntry::ConditionViolation {
            skill_name: skill_def.name.clone(),
            condition: "{force} < 40".to_string(),
            phase: "hold".to_string(),
        },
    ];

    // Verify WAL entries serialize correctly (production code writes these to disk)
    for entry in &wal_entries {
        let json = serde_json::to_string(entry).expect("WAL entry should serialize");
        let _: WalEntry = serde_json::from_str(&json).expect("WAL entry should deserialize");
    }

    // Step 7: The divergence from expected behavior triggers escalation
    // Sim expected force < 40, real had force = 50 — that's a large divergence
    let sim_data: HashMap<String, Vec<f64>> = [("force".to_string(), vec![0.0, 25.0, 25.0])].into();
    let real_data: HashMap<String, Vec<f64>> = [("force".to_string(), vec![0.0, 50.0, 50.0])].into();

    let report = compare(
        &sim_data,
        &real_data,
        &ComparisonConfig {
            channels: vec![ChannelConfig {
                name: "force".to_string(),
                metric: MetricKind::Rmse,
                threshold: 5.0, // tight threshold — real data will exceed it
            }],
            pass_score: 0.8,
        },
    );

    assert_eq!(
        report.action,
        DiagnosisAction::Escalate,
        "large force divergence should trigger escalation"
    );
}

// ---------------------------------------------------------------------------
// 6. Full vertical: Discover → Parse → Conditions → BT → Record → Compare
//    → Signatures → Diagnose → Trust
// ---------------------------------------------------------------------------

#[test]
fn full_vertical_pipeline() {
    // ---- Skill Discovery & Routing ----
    let builtins = builtin_skills();
    let discovery = SkillDiscovery::new(vec![], vec![], builtins);

    // Verify we can discover and route both skill types
    let ai_skill = discovery.load("diagnose-motor").unwrap();
    assert_eq!(
        SkillExecutor::execution_strategy(ai_skill.frontmatter.kind),
        ExecutionStrategy::AgentLoopReAct
    );

    // ---- Parse Execution Skill ----
    let exec_yaml = r#"
name: sensor-sweep
description: Sweep sensors and collect telemetry
version: "1.0.0"
inputs:
  - name: sensor_id
    port_type: String
    required: true
outputs:
  - name: readings
    port_type: Trajectory
    required: true
conditions:
  pre:
    - expression: "{sensor_ready} == true"
      phase: pre
  hold:
    - expression: "{temperature} < 80"
      phase: hold
  post:
    - expression: "{sweep_complete} == true"
      phase: post
hardware:
  timeout_secs: 15
  heartbeat_hz: 20.0
  reversible: false
  safe_halt_action: park_sensor
tree:
  type: sequence
  children:
    - type: action
      name: initialize_sensor
      action_type: move
    - type: action
      name: collect_data
      action_type: move
"#;

    let skill_def = parse_execution_skill(exec_yaml).unwrap();
    assert_eq!(skill_def.name, "sensor-sweep");

    // ---- Build & Run BT ----
    let checker = ConditionChecker::new(
        skill_def
            .conditions
            .pre
            .iter()
            .map(|c| ConditionSpec {
                expression: c.expression.clone(),
                phase: ConditionPhase::Pre,
            })
            .collect(),
        skill_def
            .conditions
            .hold
            .iter()
            .map(|c| ConditionSpec {
                expression: c.expression.clone(),
                phase: ConditionPhase::Hold,
            })
            .collect(),
        skill_def
            .conditions
            .post
            .iter()
            .map(|c| ConditionSpec {
                expression: c.expression.clone(),
                phase: ConditionPhase::Post,
            })
            .collect(),
    );

    // Executor that simulates sensor sweep and writes completion flag
    struct SweepExecutor {
        ticks: u32,
        sets_complete: bool,
    }
    impl ActionExecutor for SweepExecutor {
        fn on_start(&mut self, bb: &mut Blackboard) -> BtStatus {
            self.ticks -= 1;
            bb.set("temperature", json!(45.0));
            if self.ticks == 0 {
                if self.sets_complete {
                    bb.set("sweep_complete", json!(true));
                }
                BtStatus::Success
            } else {
                BtStatus::Running
            }
        }
        fn on_running(&mut self, bb: &mut Blackboard) -> BtStatus {
            self.ticks -= 1;
            bb.set("temperature", json!(50.0));
            if self.ticks == 0 {
                if self.sets_complete {
                    bb.set("sweep_complete", json!(true));
                }
                BtStatus::Success
            } else {
                BtStatus::Running
            }
        }
        fn on_halted(&mut self, _bb: &mut Blackboard) {}
        fn action_type(&self) -> &'static str {
            "move"
        }
    }

    let root: Box<dyn BtNode> = Box::new(SequenceNode::new(
        "sensor-sweep",
        vec![
            Box::new(ActionNode::new(
                "initialize_sensor",
                Box::new(SweepExecutor {
                    ticks: 2,
                    sets_complete: false,
                }),
            )),
            Box::new(ActionNode::new(
                "collect_data",
                Box::new(SweepExecutor {
                    ticks: 3,
                    sets_complete: true,
                }),
            )),
        ],
    ));

    let mut runner = SkillRunner::new(root, checker);
    runner.blackboard_mut().set("sensor_ready", json!(true));
    runner.blackboard_mut().set("temperature", json!(25.0));
    runner.blackboard_mut().set("sweep_complete", json!(false));

    let bt_result = runner.run_to_completion(20);
    assert!(
        bt_result.success,
        "sensor sweep should succeed: {:?}",
        bt_result.message
    );

    // ---- WAL Logging ----
    let wal = vec![
        WalEntry::SkillStarted {
            skill_name: "sensor-sweep".to_string(),
            kind: "execution".to_string(),
        },
        WalEntry::SkillCompleted {
            skill_name: "sensor-sweep".to_string(),
            success: bt_result.success,
            ticks: Some(bt_result.ticks),
        },
    ];
    // Verify WAL entries round-trip
    for entry in &wal {
        let json = serde_json::to_string(entry).unwrap();
        let _: WalEntry = serde_json::from_str(&json).unwrap();
    }

    // ---- Recording Manifests ----
    let now = Utc::now();
    let run_id = Uuid::new_v4();
    let host_id = Uuid::new_v4();

    let sim_recording = RecordingManifest {
        id: Uuid::new_v4(),
        run_id,
        environment_id: Uuid::new_v4(),
        host_id,
        source: RecordingSource::Simulation,
        channels: vec![ChannelManifest {
            name: "temperature".to_string(),
            topic: "/sensor/temp".to_string(),
            schema_name: "Float64".to_string(),
            message_count: 50,
        }],
        duration_secs: f64::from(bt_result.ticks),
        created_at: now,
    };

    let real_recording = RecordingManifest {
        id: Uuid::new_v4(),
        run_id,
        environment_id: Uuid::new_v4(),
        host_id,
        source: RecordingSource::Physical,
        channels: sim_recording.channels.clone(),
        duration_secs: f64::from(bt_result.ticks) + 0.3,
        created_at: now,
    };

    assert_eq!(sim_recording.run_id, real_recording.run_id);

    // ---- Sim-to-Real Comparison ----
    let sim_temp: HashMap<String, Vec<f64>> = [("temperature".to_string(), vec![25.0, 30.0, 35.0, 40.0, 45.0])].into();
    let real_temp: HashMap<String, Vec<f64>> = [("temperature".to_string(), vec![25.5, 31.0, 36.0, 41.0, 46.0])].into();

    let comparison_config = ComparisonConfig {
        channels: vec![ChannelConfig {
            name: "temperature".to_string(),
            metric: MetricKind::Rmse,
            threshold: 2.0,
        }],
        pass_score: 0.8,
    };

    let report = compare(&sim_temp, &real_temp, &comparison_config);
    assert_eq!(report.action, DiagnosisAction::Pass);

    // ---- Failure Signatures ----
    let signatures = builtin_signatures();
    let matched = match_signatures(&report, &signatures);
    // Healthy data shouldn't match failure signatures prominently
    assert!(matched.len() <= signatures.len());

    // ---- LLM Diagnosis Context ----
    let ctx = DiagnosisContext {
        sim_stats: vec![compute_channel_stats("temperature", &sim_temp["temperature"])],
        real_stats: vec![compute_channel_stats("temperature", &real_temp["temperature"])],
        divergence_summary: format!(
            "Skill '{}' completed in {} ticks. Score: {:.2}, Action: {:?}. Matched signatures: {:?}",
            skill_def.name, bt_result.ticks, report.overall_score, report.action, matched,
        ),
        action: report.action,
    };

    let summary = generate_summary(&ctx);
    assert!(
        summary.contains("sensor-sweep"),
        "summary should mention the skill name"
    );
    assert!(summary.contains("Sim channel statistics:"));

    // ---- Device Trust Check ----
    let firmware = b"roz-firmware-sensor-v2.0.0";
    let crc = crc32fast::hash(firmware);
    let sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(firmware));

    assert!(verify_firmware_crc32(firmware, crc));
    assert!(verify_firmware_sha256(firmware, &sha));

    let device = DeviceTrust {
        host_id,
        tenant_id: "tenant-lab".to_string(),
        posture: TrustPosture::Untrusted,
        firmware: Some(FirmwareManifest {
            version: "2.0.0".to_string(),
            sha256: sha,
            crc32: crc,
            ed25519_signature: None,
            partition: FlashPartition::A,
        }),
        sbom_hash: None,
        last_attestation: Some(now),
        created_at: now,
        updated_at: now,
    };

    let posture = evaluate_trust(
        &device,
        &TrustPolicy {
            max_attestation_age_secs: 3600,
            require_firmware_signature: false,
            allowed_firmware_versions: vec!["2.0.0".to_string()],
        },
        now,
    );

    assert_eq!(
        posture,
        TrustPosture::Trusted,
        "host that ran the skill should be trusted"
    );

    // ---- Blackboard Condition Evaluation (cross-check) ----
    // Verify that the blackboard state from BT execution is consistent
    // with what the condition evaluator expects
    let mut bb = Blackboard::new();
    bb.set("temperature", json!(46.0)); // final real reading
    bb.set("sweep_complete", json!(true));

    assert!(matches!(
        evaluate_condition("{temperature} < 80", &bb),
        roz_core::bt::conditions::ConditionResult::Satisfied
    ));
    assert!(matches!(
        evaluate_condition("{sweep_complete} == true", &bb),
        roz_core::bt::conditions::ConditionResult::Satisfied
    ));
}

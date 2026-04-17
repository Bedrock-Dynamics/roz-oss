//! Structured 5-block system prompt assembly for multi-block prompt caching.
//!
//! ## Block layout (MEM-05)
//!
//! | # | Content | Stability |
//! |---|---------|-----------|
//! | 0 | Constitution text (verbatim) | Stable across turns |
//! | 1 | Memory context (frozen session-start snapshot from `MemoryStore`) | Stable per session |
//! | 2 | Tool catalog + embodiment manifest summary | Stable when tools don't change |
//! | 3 | Blueprint-injected project/domain context | Stable per session |
//! | 4 | Volatile per-turn context (snapshot, spatial, trust, edge) | Per-turn |
//!
//! Per MEM-05, the memory block sits between constitution and tool catalog so
//! the frozen memory snapshot participates in the stable cache prefix alongside
//! the constitution. Blocks 0–3 are designed to be stable across turns,
//! maximising prompt cache hits. Block 4 changes per turn and is placed last so
//! prefix caching applies to blocks 0–3.
//!
//! **Frozen snapshot rule (Hermes parity):** the memory entries in block 1 are
//! read exactly once at session start by `SessionRuntime` and referenced on
//! every turn. Mid-session writes are NOT visible in the prompt until the next
//! session — this is required for Anthropic/Gemini prefix cache stability.
//!
//! The assembler does **not** depend on any model provider — it only produces
//! `Vec<SystemBlock>`.

#![allow(clippy::similar_names)]

use roz_core::edge_health::EdgeTransportHealth;
use roz_core::memory::MemoryEntry;
use roz_core::session::snapshot::SessionSnapshot;
use roz_core::spatial::WorldState;
use roz_core::trust::TrustPosture;
use serde::{Deserialize, Serialize};

fn format_runtime_failure(failure: roz_core::session::activity::RuntimeFailureKind) -> String {
    serde_json::to_value(failure)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "model_error".to_string())
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A labeled system prompt block for multi-block prompt caching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemBlock {
    /// Human-readable label (used for diagnostics, not sent to the model).
    pub label: String,
    /// The prompt text content of this block.
    pub content: String,
}

/// Schema for a tool visible to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSchema {
    /// Tool name as registered with the dispatcher.
    pub name: String,
    /// One-line human-readable description.
    pub description: String,
    /// JSON Schema for the tool parameters (compact string form).
    pub parameters_json: String,
}

/// All context needed to assemble one set of system prompt blocks.
pub struct AssemblyContext<'a> {
    /// Execution mode for the current turn.
    pub mode: crate::agent_loop::AgentLoopMode,
    /// Most recent session snapshot, if one has been produced.
    pub snapshot: Option<&'a SessionSnapshot>,
    /// Spatial context from the OODA observe step, if available.
    pub spatial_context: Option<&'a WorldState>,
    /// Tool schemas to advertise in block 1.
    pub tool_schemas: &'a [ToolSchema],
    /// Current aggregate trust posture.
    pub trust_posture: &'a TrustPosture,
    /// Current edge transport health.
    pub edge_state: &'a EdgeTransportHealth,
    /// Retrieved memory entries for this turn.
    pub memory_entries: &'a [MemoryEntry],
    /// Phase 18 SKILL-05 / D-12: frozen session-start tier-0 skill snapshot.
    /// Loaded once by the bootstrap site via `roz_db::skills::list_recent`
    /// (PLAN-08 wires both production sites). Empty in tests / local-dev
    /// fail-open paths. Block-rendering is PLAN-07's responsibility — this plan
    /// only adds the field so PLAN-08's frozen-snapshot wiring compiles.
    pub skill_entries: &'a [roz_db::skills::SkillSummary],
    /// Blueprint / project context strings (joined into block 2).
    pub custom_blocks: Vec<String>,
    /// Volatile per-turn context strings appended into block 4.
    pub volatile_blocks: Vec<String>,
}

// ---------------------------------------------------------------------------
// PromptAssembler
// ---------------------------------------------------------------------------

/// Assembles multi-block system prompts with a cache-friendly 6-block layout.
///
/// Construct once per session (or per constitution change) and call
/// [`assemble`](Self::assemble) each turn.
#[derive(Debug, Clone)]
pub struct PromptAssembler {
    /// The full constitution text placed verbatim in block 0.
    constitution_text: String,
}

impl PromptAssembler {
    /// Create a new assembler with the given constitution text.
    #[must_use]
    pub const fn new(constitution_text: String) -> Self {
        Self { constitution_text }
    }

    /// Assemble the 6-block system prompt from the provided context.
    ///
    /// Block order (MEM-05 + Phase 18 SKILL-05): constitution, memory_context,
    /// **skills_context**, tool_catalog, blueprint_context, volatile. Memory and
    /// skills sit between constitution and tool catalog so the frozen
    /// memory/skill snapshots participate in the stable cache prefix alongside
    /// the constitution.
    ///
    /// Always returns exactly 6 blocks.
    #[must_use]
    pub fn assemble(&self, context: &AssemblyContext<'_>) -> Vec<SystemBlock> {
        vec![
            self.block_constitution(),
            Self::block_memory_context(context),
            Self::block_skills_context(context),
            Self::block_tool_catalog(context),
            Self::block_custom_context(context),
            Self::block_volatile_context(context),
        ]
    }

    // -----------------------------------------------------------------------
    // Block builders (private)
    // -----------------------------------------------------------------------

    /// Block 0 — constitution verbatim. Maximises cache hits across turns.
    fn block_constitution(&self) -> SystemBlock {
        SystemBlock {
            label: "constitution".into(),
            content: self.constitution_text.clone(),
        }
    }

    /// Block 3 — tool catalog and mode summary.
    fn block_tool_catalog(context: &AssemblyContext<'_>) -> SystemBlock {
        use crate::agent_loop::AgentLoopMode;

        let mode_label = match context.mode {
            AgentLoopMode::React => "React (pure reasoning + tools, no spatial observation)",
            AgentLoopMode::OodaReAct => "OodaReAct (spatial observation injected each cycle, safety stack active)",
        };

        let mut parts: Vec<String> = vec!["## Tool Catalog".into(), format!("Mode: {mode_label}"), String::new()];

        if context.tool_schemas.is_empty() {
            parts.push("(no tools registered for this turn)".into());
        } else {
            for schema in context.tool_schemas {
                parts.push(format!("- **{}**: {}", schema.name, schema.description));
            }
        }

        SystemBlock {
            label: "tool_catalog".into(),
            content: parts.join("\n"),
        }
    }

    /// Block 4 — blueprint / project context from `custom_blocks`.
    fn block_custom_context(ctx: &AssemblyContext<'_>) -> SystemBlock {
        let text = if ctx.custom_blocks.is_empty() {
            String::new()
        } else {
            ctx.custom_blocks.join("\n\n")
        };

        SystemBlock {
            label: "blueprint_context".into(),
            content: text,
        }
    }

    /// Block 1 — runtime-owned memory context retrieved before the turn begins.
    fn block_memory_context(context: &AssemblyContext<'_>) -> SystemBlock {
        let content = if context.memory_entries.is_empty() {
            String::new()
        } else {
            let mut parts = vec!["## Memory Context".to_string()];
            for entry in context.memory_entries {
                parts.push(format!(
                    "- [{} | verified={} | confidence={:?}] {}",
                    entry.memory_id, entry.verified, entry.confidence, entry.fact
                ));
            }
            parts.join("\n")
        };

        SystemBlock {
            label: "memory_context".into(),
            content,
        }
    }

    /// Block 2 — Phase 18 SKILL-05 / D-12: tier-0 skill listing.
    ///
    /// N=20 most-recent skills (caller pre-loaded at session start via
    /// `roz_db::skills::list_recent`), ≤3 KB rendered budget, format
    /// `- {name} v{version}: {desc≤140c}`. Frozen at session start
    /// (RESEARCH OQ #4) — the agent uses `skills_list` for live discovery and
    /// `skill_view` for live body/version loads when skills crystallize
    /// mid-session.
    ///
    /// Always returns a non-empty `SystemBlock` (placeholder text on no skills)
    /// so downstream code that asserts on `blocks[2].label == "skills_context"`
    /// works regardless of tenant skill inventory.
    fn block_skills_context(ctx: &AssemblyContext<'_>) -> SystemBlock {
        /// D-12: ≤3 KB rendered budget for the tier-0 skills block.
        const MAX_BLOCK_BYTES: usize = 3072;
        /// D-12: at most N=20 lines (most-recent-by-tenant ordering done by SQL).
        const MAX_SKILLS: usize = 20;
        /// Discretion (CONTEXT D-12): truncate descriptions to 140 chars
        /// (Twitter-era default, leaves room for 20 skills in the 3 KB budget).
        const MAX_DESC_CHARS: usize = 140;

        let mut content = String::with_capacity(1024);
        content.push_str("## Skills (tier-0 listing — call `skill_view` to load body)\n\n");

        if ctx.skill_entries.is_empty() {
            content.push_str("(no skills installed for this tenant)\n");
            return SystemBlock {
                label: "skills_context".into(),
                content,
            };
        }

        let mut rendered = 0usize;
        for s in ctx.skill_entries.iter().take(MAX_SKILLS) {
            let desc: String = if s.description.chars().count() > MAX_DESC_CHARS {
                let truncated: String = s.description.chars().take(MAX_DESC_CHARS).collect();
                format!("{truncated}...")
            } else {
                s.description.clone()
            };
            let line = format!("- {} v{}: {}\n", s.name, s.version, desc);
            if content.len() + line.len() > MAX_BLOCK_BYTES {
                break;
            }
            content.push_str(&line);
            rendered += 1;
        }
        tracing::debug!(rendered, available = ctx.skill_entries.len(), "skills tier-0 block");

        SystemBlock {
            label: "skills_context".into(),
            content,
        }
    }

    /// Block 5 — volatile per-turn context: snapshot, trust, edge, spatial.
    fn block_volatile_context(context: &AssemblyContext<'_>) -> SystemBlock {
        let mut parts: Vec<String> = Vec::new();

        // Snapshot summary
        if let Some(snap) = context.snapshot {
            parts.push("## Session State".into());
            parts.push(format!("Turn: {}", snap.turn_index));
            parts.push(format!("Can execute physical: {}", snap.can_execute_physical()));
            parts.push(format!("Control mode: {:?}", snap.control_mode));

            if let Some(goal) = &snap.current_goal {
                parts.push(format!("Current goal: {goal}"));
            }
            if let Some(phase) = &snap.current_phase {
                parts.push(format!("Current phase: {phase}"));
            }
            if let Some(step) = &snap.next_expected_step {
                parts.push(format!("Next expected step: {step}"));
            }
            if let Some(action) = &snap.last_approved_physical_action {
                parts.push(format!("Last approved physical action: {action}"));
            }
            if let Some(controller_id) = &snap.active_controller_id {
                parts.push(format!("Active controller: {controller_id}"));
            }
            if let Some(verdict) = &snap.last_controller_verdict {
                parts.push(format!("Last controller verdict: {verdict:?}"));
            }
            if let Some(verdict) = &snap.last_verifier_result {
                parts.push(format!("Last verifier result: {verdict:?}"));
            }
            if let Some(blocker) = &snap.pending_blocker {
                parts.push(format!("BLOCKER: {blocker}"));
            }
            if !snap.open_risks.is_empty() {
                parts.push(format!("Open risks: {}", snap.open_risks.join("; ")));
            }
            if let Some(failure) = &snap.last_failure {
                parts.push(format!("Last failure: {}", format_runtime_failure(*failure)));
            }
            parts.push(format!(
                "Freshness: telemetry={:?}, spatial={:?}",
                snap.telemetry_freshness, snap.spatial_freshness
            ));
        }

        // Trust posture summary
        parts.push(String::new());
        parts.push("## Trust Posture".into());
        parts.push(format!(
            "Physical execution: {:?} | Host: {:?} | Environment: {:?} | Tool: {:?}",
            context.trust_posture.physical_execution_trust,
            context.trust_posture.host_trust,
            context.trust_posture.environment_trust,
            context.trust_posture.tool_trust,
        ));
        parts.push(format!(
            "Controller artifact: {:?} | Edge transport: {:?}",
            context.trust_posture.controller_artifact_trust, context.trust_posture.edge_transport_trust,
        ));
        if !context.trust_posture.can_execute_physical() {
            parts.push("WARNING: Physical execution trust is below threshold.".into());
        }

        // Edge state
        parts.push(String::new());
        parts.push("## Edge Transport".into());
        match context.edge_state {
            EdgeTransportHealth::Healthy => parts.push("Status: Healthy".into()),
            EdgeTransportHealth::Degraded { affected } => {
                parts.push(format!("Status: Degraded (affected: {})", affected.join(", ")));
            }
            EdgeTransportHealth::Disconnected => {
                parts.push("Status: DISCONNECTED — physical actions will be blocked.".into());
            }
        }

        // Spatial context summary (entity counts, alert counts)
        if let Some(spatial) = context.spatial_context {
            parts.push(String::new());
            parts.push("## Spatial Context".into());
            parts.push(format!("Entities observed: {}", spatial.entities.len()));
            if !spatial.alerts.is_empty() {
                parts.push(format!("Active alerts: {}", spatial.alerts.len()));
                for alert in &spatial.alerts {
                    parts.push(format!("  - {alert:?}"));
                }
            }
            if !spatial.occluded_regions.is_empty() {
                parts.push(format!("Occluded regions: {}", spatial.occluded_regions.len()));
            }
        }

        if !context.volatile_blocks.is_empty() {
            parts.push(String::new());
            parts.push("## Turn Context".into());
            parts.extend(context.volatile_blocks.iter().cloned());
        }

        SystemBlock {
            label: "volatile_context".into(),
            content: parts.join("\n"),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_loop::AgentLoopMode;
    use roz_core::edge_health::EdgeTransportHealth;
    use roz_core::memory::{Confidence, MemoryClass, MemoryEntry, MemorySourceKind};
    use roz_core::session::activity::{RuntimeFailureKind, SafePauseState};
    use roz_core::session::control::ControlMode;
    use roz_core::session::snapshot::{FreshnessState, SessionSnapshot};
    use roz_core::trust::{TrustLevel, TrustPosture};
    use roz_db::skills::SkillSummary;

    fn default_assembler() -> PromptAssembler {
        PromptAssembler::new("Tier 1: Do no harm.".into())
    }

    fn default_trust() -> TrustPosture {
        TrustPosture::default()
    }

    fn default_edge() -> EdgeTransportHealth {
        EdgeTransportHealth::Healthy
    }

    fn minimal_context<'a>(trust: &'a TrustPosture, edge: &'a EdgeTransportHealth) -> AssemblyContext<'a> {
        AssemblyContext {
            mode: AgentLoopMode::React,
            snapshot: None,
            spatial_context: None,
            tool_schemas: &[],
            trust_posture: trust,
            edge_state: edge,
            memory_entries: &[],
            skill_entries: &[],
            custom_blocks: vec![],
            volatile_blocks: vec![],
        }
    }

    fn sample_skill(name: &str, version: &str, description: &str) -> SkillSummary {
        SkillSummary {
            name: name.into(),
            version: version.into(),
            description: description.into(),
            created_at: chrono::Utc::now(),
            created_by: "test".into(),
        }
    }

    fn sample_memory() -> MemoryEntry {
        MemoryEntry {
            memory_id: "mem-1".into(),
            class: MemoryClass::Safety,
            scope_key: "session:test".into(),
            fact: "Operator requested slower approach speed near the cup.".into(),
            source_kind: MemorySourceKind::OperatorStated,
            source_ref: None,
            confidence: Confidence::High,
            verified: true,
            stale_after: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn sample_snapshot() -> SessionSnapshot {
        use chrono::Utc;
        SessionSnapshot {
            session_id: "sess-001".into(),
            turn_index: 3,
            current_goal: Some("grasp the cup".into()),
            current_phase: Some("approach".into()),
            next_expected_step: Some("open gripper".into()),
            last_approved_physical_action: None,
            last_verifier_result: None,
            telemetry_freshness: FreshnessState::Fresh,
            spatial_freshness: FreshnessState::Fresh,
            pending_blocker: Some("waiting for arm calibration".into()),
            open_risks: vec!["cup near table edge".into()],
            control_mode: ControlMode::Autonomous,
            safe_pause_state: SafePauseState::Running,
            host_trust_posture: TrustPosture::default(),
            environment_trust_posture: TrustPosture::default(),
            edge_transport_state: EdgeTransportHealth::Healthy,
            active_controller_id: None,
            last_controller_verdict: None,
            last_failure: Some(RuntimeFailureKind::ToolError),
            updated_at: Utc::now(),
        }
    }

    // -----------------------------------------------------------------------

    #[test]
    fn assemble_produces_6_blocks() {
        // Phase 18 PLAN-01: pre-flipped for the 6-block layout that PLAN-07 will deliver
        // (constitution=0, memory=1, skills=2, tool_catalog=3, blueprint=4, volatile=5).
        // Fails until PLAN-07 inserts block_skills_context.
        let assembler = default_assembler();
        let trust = default_trust();
        let edge = default_edge();
        let ctx = minimal_context(&trust, &edge);
        let blocks = assembler.assemble(&ctx);
        assert_eq!(blocks.len(), 6, "assemble must always return exactly 6 blocks");
    }

    #[test]
    fn assemble_skills_in_block_2() {
        // Phase 18 PLAN-07: skills_context block now sits at position 2.
        let assembler = default_assembler();
        let trust = default_trust();
        let edge = default_edge();
        let ctx = minimal_context(&trust, &edge);
        let blocks = assembler.assemble(&ctx);
        assert_eq!(blocks[2].label, "skills_context", "skills block must be at position 2");
    }

    #[test]
    fn skills_block_empty_placeholder() {
        // D-12: empty skill_entries still produces a non-empty block so block-index
        // alignment is preserved (downstream callers may consult blocks[2].label).
        let assembler = default_assembler();
        let trust = default_trust();
        let edge = default_edge();
        let ctx = minimal_context(&trust, &edge);
        let blocks = assembler.assemble(&ctx);
        assert_eq!(blocks[2].label, "skills_context");
        assert!(
            blocks[2].content.contains("no skills installed"),
            "empty tenant must show placeholder; got {:?}",
            blocks[2].content
        );
        assert!(
            !blocks[2].content.is_empty(),
            "skills block content must never be empty"
        );
    }

    #[test]
    fn skills_block_renders_tier0_format() {
        // D-12: tier-0 format is `- {name} v{version}: {description}` per row.
        let assembler = default_assembler();
        let trust = default_trust();
        let edge = default_edge();
        let skills = vec![
            sample_skill("demo-one", "1.0.0", "first test skill"),
            sample_skill("demo-two", "2.3.4", "second test skill"),
        ];
        let ctx = AssemblyContext {
            mode: AgentLoopMode::React,
            snapshot: None,
            spatial_context: None,
            tool_schemas: &[],
            trust_posture: &trust,
            edge_state: &edge,
            memory_entries: &[],
            skill_entries: &skills,
            custom_blocks: vec![],
            volatile_blocks: vec![],
        };
        let blocks = assembler.assemble(&ctx);
        let content = &blocks[2].content;
        assert!(
            content.contains("- demo-one v1.0.0: first test skill"),
            "first skill row missing tier-0 format; got {content:?}"
        );
        assert!(
            content.contains("- demo-two v2.3.4: second test skill"),
            "second skill row missing tier-0 format; got {content:?}"
        );
    }

    #[test]
    fn skills_block_truncates_description_to_140c() {
        // D-12 discretion: descriptions longer than 140 chars are truncated with "..." suffix.
        let assembler = default_assembler();
        let trust = default_trust();
        let edge = default_edge();
        let long_desc = "x".repeat(200);
        let skills = vec![sample_skill("longdesc", "1.0.0", &long_desc)];
        let ctx = AssemblyContext {
            mode: AgentLoopMode::React,
            snapshot: None,
            spatial_context: None,
            tool_schemas: &[],
            trust_posture: &trust,
            edge_state: &edge,
            memory_entries: &[],
            skill_entries: &skills,
            custom_blocks: vec![],
            volatile_blocks: vec![],
        };
        let blocks = assembler.assemble(&ctx);
        let content = &blocks[2].content;
        // Find the rendered description portion after "1.0.0: " prefix.
        let prefix = "- longdesc v1.0.0: ";
        let after = content
            .find(prefix)
            .map(|idx| &content[idx + prefix.len()..])
            .expect("rendered skill row must contain the prefix");
        let line_end = after.find('\n').unwrap_or(after.len());
        let rendered_desc = &after[..line_end];
        assert!(
            rendered_desc.ends_with("..."),
            "long description must end with ellipsis; got {rendered_desc:?}"
        );
        // 140 chars + "..." (3 ASCII chars).
        assert_eq!(
            rendered_desc.chars().count(),
            143,
            "truncated description must be exactly 140 chars + 3-char ellipsis"
        );
    }

    #[test]
    fn skills_block_caps_at_3kb() {
        // D-12: total rendered block content must stay ≤3072 bytes regardless of
        // the upstream skill_entries volume. Iteration stops early when the next
        // line would breach the cap.
        let assembler = default_assembler();
        let trust = default_trust();
        let edge = default_edge();
        // 50 rows × ~80 chars each ≫ 3 KB ÷ ~80 ≈ 38 rows; iteration stops well
        // before 50 even if MAX_SKILLS allowed it.
        let big_desc = "x".repeat(60);
        let skills: Vec<SkillSummary> = (0..50)
            .map(|i| sample_skill(&format!("skill-{i:02}"), "1.0.0", &big_desc))
            .collect();
        let ctx = AssemblyContext {
            mode: AgentLoopMode::React,
            snapshot: None,
            spatial_context: None,
            tool_schemas: &[],
            trust_posture: &trust,
            edge_state: &edge,
            memory_entries: &[],
            skill_entries: &skills,
            custom_blocks: vec![],
            volatile_blocks: vec![],
        };
        let blocks = assembler.assemble(&ctx);
        assert!(
            blocks[2].content.len() <= 3072,
            "skills block must stay ≤3KB; got {} bytes",
            blocks[2].content.len()
        );
    }

    #[test]
    fn skills_block_caps_at_n20() {
        // D-12: at most N=20 lines, even when the input has more rows that would
        // fit in the byte budget. Use short descriptions so the byte cap doesn't
        // kick in first.
        let assembler = default_assembler();
        let trust = default_trust();
        let edge = default_edge();
        let skills: Vec<SkillSummary> = (0..30)
            .map(|i| sample_skill(&format!("s{i:02}"), "1.0.0", "x"))
            .collect();
        let ctx = AssemblyContext {
            mode: AgentLoopMode::React,
            snapshot: None,
            spatial_context: None,
            tool_schemas: &[],
            trust_posture: &trust,
            edge_state: &edge,
            memory_entries: &[],
            skill_entries: &skills,
            custom_blocks: vec![],
            volatile_blocks: vec![],
        };
        let blocks = assembler.assemble(&ctx);
        let line_count = blocks[2].content.lines().filter(|l| l.starts_with("- ")).count();
        assert_eq!(line_count, 20, "MAX_SKILLS=20 cap must drop the 21st row onward");
    }

    #[test]
    fn assemble_includes_constitution_as_block_0() {
        let constitution = "This is the roz constitution.";
        let assembler = PromptAssembler::new(constitution.into());
        let trust = default_trust();
        let edge = default_edge();
        let ctx = minimal_context(&trust, &edge);
        let blocks = assembler.assemble(&ctx);
        assert_eq!(blocks[0].label, "constitution");
        assert_eq!(blocks[0].content, constitution);
    }

    #[test]
    fn assemble_tool_catalog_in_block_3() {
        let assembler = default_assembler();
        let trust = default_trust();
        let edge = default_edge();
        let schemas = vec![
            ToolSchema {
                name: "move_joint".into(),
                description: "Move a robot joint to a target angle.".into(),
                parameters_json: r#"{"type":"object"}"#.into(),
            },
            ToolSchema {
                name: "read_sensor".into(),
                description: "Read a named sensor value.".into(),
                parameters_json: r#"{"type":"object"}"#.into(),
            },
        ];
        let ctx = AssemblyContext {
            mode: AgentLoopMode::OodaReAct,
            snapshot: None,
            spatial_context: None,
            tool_schemas: &schemas,
            trust_posture: &trust,
            edge_state: &edge,
            memory_entries: &[],
            skill_entries: &[],
            custom_blocks: vec![],
            volatile_blocks: vec![],
        };
        let blocks = assembler.assemble(&ctx);
        // Phase 18 PLAN-01: tool_catalog moves from block 2 to block 3 (skills now at 2).
        assert_eq!(blocks[3].label, "tool_catalog");
        assert!(
            blocks[3].content.contains("move_joint"),
            "block 3 must contain tool name 'move_joint'"
        );
        assert!(
            blocks[3].content.contains("read_sensor"),
            "block 3 must contain tool name 'read_sensor'"
        );
        assert!(
            blocks[3].content.contains("Move a robot joint"),
            "block 3 must contain tool description"
        );
    }

    #[test]
    fn assemble_custom_blocks_in_block_4() {
        let assembler = default_assembler();
        let trust = default_trust();
        let edge = default_edge();
        let ctx = AssemblyContext {
            mode: AgentLoopMode::React,
            snapshot: None,
            spatial_context: None,
            tool_schemas: &[],
            trust_posture: &trust,
            edge_state: &edge,
            memory_entries: &[],
            skill_entries: &[],
            custom_blocks: vec!["Project: RoboArm v2".into(), "Domain: industrial pick-and-place".into()],
            volatile_blocks: vec![],
        };
        let blocks = assembler.assemble(&ctx);
        // Phase 18 PLAN-01: blueprint_context moves from block 3 to block 4.
        assert_eq!(blocks[4].label, "blueprint_context");
        assert!(
            blocks[4].content.contains("RoboArm v2"),
            "block 4 must contain custom block content"
        );
        assert!(
            blocks[4].content.contains("industrial pick-and-place"),
            "block 4 must contain second custom block"
        );
    }

    #[test]
    fn assemble_volatile_context_in_block_5() {
        let assembler = default_assembler();
        let trust = default_trust();
        let edge = default_edge();
        let snap = sample_snapshot();
        let ctx = AssemblyContext {
            mode: AgentLoopMode::OodaReAct,
            snapshot: Some(&snap),
            spatial_context: None,
            tool_schemas: &[],
            trust_posture: &trust,
            edge_state: &edge,
            memory_entries: &[],
            skill_entries: &[],
            custom_blocks: vec![],
            volatile_blocks: vec![],
        };
        let blocks = assembler.assemble(&ctx);
        // Phase 18 PLAN-01: volatile_context moves from block 4 to block 5.
        assert_eq!(blocks[5].label, "volatile_context");
        let content = &blocks[5].content;
        assert!(content.contains("grasp the cup"), "goal must appear in block 5");
        assert!(
            content.contains("waiting for arm calibration"),
            "blocker must appear in block 5"
        );
        assert!(
            content.contains("Can execute physical: false"),
            "physical execution gate must appear in block 5"
        );
        assert!(content.contains("cup near table edge"), "risk must appear in block 5");
        assert!(content.contains("tool_error"), "last failure must appear in block 5");
    }

    #[test]
    fn block_0_stable_across_contexts() {
        let constitution = "Stable constitution text.";
        let assembler = PromptAssembler::new(constitution.into());

        let trust1 = default_trust();
        let edge1 = default_edge();
        let ctx1 = minimal_context(&trust1, &edge1);

        let trust2 = TrustPosture {
            workspace_trust: TrustLevel::High,
            ..TrustPosture::default()
        };
        let edge2 = EdgeTransportHealth::Disconnected;
        let snap = sample_snapshot();
        let ctx2 = AssemblyContext {
            mode: AgentLoopMode::OodaReAct,
            snapshot: Some(&snap),
            spatial_context: None,
            tool_schemas: &[],
            trust_posture: &trust2,
            edge_state: &edge2,
            memory_entries: &[],
            skill_entries: &[],
            custom_blocks: vec!["some project context".into()],
            volatile_blocks: vec![],
        };

        let blocks1 = assembler.assemble(&ctx1);
        let blocks2 = assembler.assemble(&ctx2);

        assert_eq!(
            blocks1[0].content, blocks2[0].content,
            "block 0 content must be identical regardless of context"
        );
    }

    #[test]
    fn empty_custom_blocks_gives_empty_block_4() {
        let assembler = default_assembler();
        let trust = default_trust();
        let edge = default_edge();
        let ctx = minimal_context(&trust, &edge);
        let blocks = assembler.assemble(&ctx);
        // Phase 18 PLAN-01: blueprint_context (custom blocks) moves to block 4.
        assert_eq!(blocks[4].content, "");
    }

    #[test]
    fn block_1_is_empty_when_no_memory_entries() {
        let assembler = default_assembler();
        let trust = default_trust();
        let edge = default_edge();
        let ctx = minimal_context(&trust, &edge);
        let blocks = assembler.assemble(&ctx);
        // MEM-05: memory_context moved from block 3 to block 1.
        assert_eq!(blocks[1].label, "memory_context");
        assert_eq!(blocks[1].content, "");
    }

    #[test]
    fn block_1_renders_memory_entries() {
        let assembler = default_assembler();
        let trust = default_trust();
        let edge = default_edge();
        let memory = vec![sample_memory()];
        let ctx = AssemblyContext {
            mode: AgentLoopMode::React,
            snapshot: None,
            spatial_context: None,
            tool_schemas: &[],
            trust_posture: &trust,
            edge_state: &edge,
            memory_entries: &memory,
            skill_entries: &[],
            custom_blocks: vec![],
            volatile_blocks: vec![],
        };
        let blocks = assembler.assemble(&ctx);
        // MEM-05: memory_context moved from block 3 to block 1.
        assert_eq!(blocks[1].label, "memory_context");
        assert!(blocks[1].content.contains("Memory Context"));
        assert!(blocks[1].content.contains("slower approach speed"));
    }

    #[test]
    fn memory_block_is_position_1_and_budget_capped() {
        // Phase 18 PLAN-01: memory stays at position 1 in the 6-block layout;
        // every downstream block shifts by +1 to make room for skills at position 2.
        let entries: Vec<MemoryEntry> = (0..10)
            .map(|i| MemoryEntry {
                memory_id: format!("m-{i}"),
                class: MemoryClass::Operator,
                scope_key: "agent".into(),
                fact: format!("Fact {i}: operator prefers metric units when reporting distances."),
                source_kind: MemorySourceKind::OperatorStated,
                source_ref: None,
                confidence: Confidence::High,
                verified: true,
                stale_after: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            })
            .collect();
        let assembler = PromptAssembler::new("Test constitution".into());
        let trust = default_trust();
        let edge = default_edge();
        let ctx = AssemblyContext {
            mode: AgentLoopMode::React,
            snapshot: None,
            spatial_context: None,
            tool_schemas: &[],
            trust_posture: &trust,
            edge_state: &edge,
            memory_entries: &entries,
            skill_entries: &[],
            custom_blocks: vec![],
            volatile_blocks: vec![],
        };
        let blocks = assembler.assemble(&ctx);
        assert_eq!(blocks.len(), 6);
        assert_eq!(blocks[0].label, "constitution", "block 0 must be constitution");
        assert_eq!(blocks[1].label, "memory_context", "memory at position 1");
        assert_eq!(blocks[2].label, "skills_context", "Phase 18: skills at position 2");
        assert_eq!(blocks[3].label, "tool_catalog", "tool_catalog at position 3");
        assert_eq!(blocks[4].label, "blueprint_context", "blueprint_context at position 4");
        assert_eq!(blocks[5].label, "volatile_context", "volatile at position 5");
        assert!(
            blocks[1].content.len() <= 4096,
            "memory_context block must stay ≤4KB; got {} bytes",
            blocks[1].content.len()
        );
    }

    #[test]
    fn disconnected_edge_appears_in_block_5() {
        let assembler = default_assembler();
        let trust = default_trust();
        let edge = EdgeTransportHealth::Disconnected;
        let ctx = minimal_context(&trust, &edge);
        let blocks = assembler.assemble(&ctx);
        // Phase 18 PLAN-01: volatile_context moves to block 5.
        assert!(
            blocks[5].content.contains("DISCONNECTED"),
            "disconnected edge must be flagged in volatile context"
        );
    }

    #[test]
    fn low_trust_warning_appears_in_block_5() {
        let assembler = default_assembler();
        let trust = TrustPosture {
            physical_execution_trust: TrustLevel::Untrusted,
            ..TrustPosture::default()
        };
        let edge = default_edge();
        let ctx = minimal_context(&trust, &edge);
        let blocks = assembler.assemble(&ctx);
        // Phase 18 PLAN-01: volatile_context moves to block 5.
        assert!(
            blocks[5].content.contains("WARNING"),
            "low physical trust must generate a warning in block 5"
        );
    }
}

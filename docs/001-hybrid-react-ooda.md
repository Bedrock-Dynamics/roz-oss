# ADR-001: Hybrid ReAct-OODA Architecture for the Agent Loop

**Date:** 2026-02-23
**Status:** Accepted

## Context

The Roz agent loop is implemented as `OodaCycle` in `crates/roz-agent/src/ooda.rs`. Despite the OODA naming, its runtime behavior is functionally ReAct (Reason + Act): the model receives a prompt, reasons about it, emits tool calls, observes tool results, and loops. The classic OODA phases (Observe, Orient, Decide, Act) do not map cleanly onto what the code actually does.

### The spatial observation gap

`SpatialContextProvider` (`crates/roz-agent/src/spatial_provider.rs:6`) defines an async `snapshot()` method that returns a `SpatialContext` -- positions, orientations, velocities, relations, constraints, and alerts for all entities in the scene. The `OodaCycle` calls this every iteration (`ooda.rs:79`) but only passes the result to `SafetyStack::evaluate()` (`ooda.rs:109`). The `CompletionRequest` constructed at lines 82-86 contains only `messages`, `tools`, and `max_tokens`. Spatial context is never injected into model messages.

This means:

1. **AI skills pay unnecessary cost.** Pure reasoning tasks (code generation, planning, document synthesis) invoke spatial observation every cycle even though neither the model nor the safety guards need live world state. This adds latency from sensor polling and scene graph serialization.

2. **Physical skills lack world-state reasoning.** For execution skills that drive robot hardware via behavior trees, the model cannot reason about spatial relationships, obstacle proximity, or constraint violations because that data never reaches its context window. The safety stack sees it, but the model is blind to it.

3. **The OODA naming is misleading.** The "Observe" phase fetches spatial data that vanishes into the safety stack. The model's "observation" is actually tool results -- pure ReAct. Developers reading the code expect OODA semantics but get ReAct behavior.

## Research

The decision to adopt a mode-adaptive hybrid is grounded in several converging research threads:

### Dual-process theory (Kahneman, System 1/2)

Cognitive science distinguishes fast, automatic processing (System 1) from slow, deliberate reasoning (System 2). AI skills that chain tools and reason over text operate in a System 2 mode -- purely symbolic, no perceptual grounding needed. Physical skills that react to sensor data operate in a blended mode where perception (System 1) must feed into deliberation (System 2). A single loop architecture forces System 2 overhead on tasks that need only System 1 reflexes, and starves System 2 of perceptual input when it needs it.

### NVIDIA GR00T N1

NVIDIA's GR00T N1 foundation model for humanoid robots uses a dual-system architecture: a high-level "thinking" system that reasons about tasks symbolically, and a low-level "acting" system that processes spatial observations at high frequency. The two systems communicate through structured context, not by forcing all data through a single loop.

### Inner Monologue (Google Research)

Google's Inner Monologue work demonstrated that grounding language model reasoning in continuous environment feedback (scene descriptions, success detectors, human corrections) dramatically improves physical task completion. The key insight: the model must see spatial state as text in its context window, not just have it evaluated externally.

### SayCan (Google Research)

SayCan introduced affordance scoring: grounding language model proposals in physical feasibility by combining model confidence with a value function over world state. This validates the pattern of injecting world-state summaries into model reasoning for physical tasks while keeping pure language reasoning ungrounded.

### RP-ReAct

RP-ReAct (Robot Planning with ReAct) extends the ReAct framework specifically for robotics by augmenting the reasoning trace with spatial observations and physical constraints. It demonstrates that the ReAct loop structure works for physical tasks when augmented with environmental grounding, without requiring a fundamentally different architecture.

### Three-layer robotics architectures (3T, CLARAty)

Classical robotics uses three layers: a reactive/behavioral layer (fast, reflexive), a sequencing/executive layer (skill coordination), and a deliberative layer (planning, reasoning). The Roz architecture maps to this: safety guards are reactive, behavior trees are sequencing, and the agent loop is deliberative. The hybrid approach aligns the deliberative layer's input with whether the active skill requires spatial grounding (physical) or only symbolic grounding (AI).

## Decision

Rename `OodaCycle` to `AgentLoop` and introduce an `AgentLoopMode` enum that determines observation and context-injection behavior per cycle:

```rust
pub enum AgentLoopMode {
    /// Pure ReAct for AI skills.
    /// No spatial observation. Model receives messages + tools only.
    /// Safety stack evaluates with empty/cached spatial context.
    React,

    /// OODA-enriched ReAct for physical/execution skills.
    /// Spatial observation every cycle.
    /// Spatial context injected as a system message addendum.
    /// Safety stack evaluates with live spatial context.
    /// Model reasons about world state explicitly.
    OodaReact,
}
```

### React mode

- Used for AI skills: code generation, planning, document synthesis, data analysis.
- `SpatialContextProvider::snapshot()` is not called (or returns a cached/default context).
- `CompletionRequest` contains only the conversation messages and tool schemas.
- Safety stack evaluates with `SpatialContext::default()` or a stale cached snapshot.
- Lower latency per cycle; no sensor polling overhead.

### OodaReact mode

- Used for physical/execution skills: robot arm manipulation, navigation, sensor-guided assembly.
- `SpatialContextProvider::snapshot()` is called every cycle with fresh sensor data.
- Spatial context is serialized and appended to the system message (or injected as a dedicated context message) so the model can reason about entity positions, velocities, proximity, and constraints.
- Safety stack evaluates with the same live spatial context.
- Higher per-cycle cost, but the model can make spatially-informed decisions -- avoiding obstacles, respecting workspace boundaries, coordinating multi-arm operations.

### Type renames

| Before | After |
|---|---|
| `OodaCycle` | `AgentLoop` |
| `OodaInput` | `AgentLoopInput` |
| `OodaOutput` | `AgentLoopOutput` |

`AgentLoopInput` gains a `mode: AgentLoopMode` field. The `AgentLoop::run()` method branches on the mode to decide whether to observe and inject spatial context.

## Consequences

### Positive

- **AI skills become faster.** Removing unnecessary spatial observation from every cycle reduces latency for pure reasoning tasks. No sensor polling, no scene graph serialization, no wasted context window tokens.
- **Physical skills get better reasoning.** Models can now see and reason about spatial relationships, obstacle proximity, velocity limits, and constraint violations directly in their context window. This enables Inner Monologue-style environment grounding.
- **Safety stack behavior is unchanged.** The safety stack continues to evaluate tool calls against spatial context. In React mode, it receives a default/empty context (which is the effective status quo for AI skills where spatial data is irrelevant). In OodaReact mode, it receives the same live context it always has.
- **Naming reflects reality.** `AgentLoop` is honest about what the code does. `AgentLoopMode` makes the behavioral difference between AI and physical skills explicit in the type system rather than hidden in runtime behavior.
- **Extensible.** Future modes (e.g., `Streaming` for real-time teleoperation, `BatchReact` for offline planning) can be added to the enum without restructuring the loop.

### Negative

- **Breaking rename of public types.** `OodaCycle`, `OodaInput`, `OodaOutput` are renamed. All call sites in `roz-worker`, `roz-cli`, tests, and any external consumers must be updated. This is a one-time migration cost.
- **Mode selection responsibility.** Callers must now choose the correct mode. Skill definitions should declare their mode, and the skill engine should propagate it to `AgentLoopInput`. Incorrect mode selection (React for a physical skill) would silently degrade reasoning quality without causing errors.
- **Increased system message size in OodaReact.** Injecting spatial context as text consumes model context window tokens. For scenes with many entities, this could become significant. A follow-up decision may be needed for context summarization or windowing strategies.

## References

- Yao, S. et al. "ReAct: Synergizing Reasoning and Acting in Language Models." ICLR 2023.
- Boyd, J. "The Essence of Winning and Losing." 1996. (OODA loop origin)
- Kahneman, D. "Thinking, Fast and Slow." 2011.
- NVIDIA. "GR00T N1: An Open Foundation Model for Generalist Humanoid Robots." 2025.
- Huang, W. et al. "Inner Monologue: Embodied Reasoning through Planning with Language Models." CoRL 2022.
- Ahn, M. et al. "Do As I Can, Not As I Say: Grounding Language in Robotic Affordances." CoRL 2022.
- Zheng, K. et al. "RP-ReAct: Robot Planning with ReAct." 2024.
- Gat, E. "Three-Layer Architectures." Artificial Intelligence and Mobile Robots, 1998.
- Nesnas, I. et al. "CLARAty: An Architecture for Reusable Robotic Software." SPIE Aerosense, 2003.

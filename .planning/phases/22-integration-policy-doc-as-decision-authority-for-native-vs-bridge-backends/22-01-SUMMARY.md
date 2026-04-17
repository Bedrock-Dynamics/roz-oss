---
phase: 22-integration-policy-doc-as-decision-authority-for-native-vs-bridge-backends
plan: 01
subsystem: documentation
tags: [docs, integration-policy, native-vs-bridge, copper-trait-contract, INT-01]
requires: []
provides:
  - "docs/integration-policy.md"
  - "Normative decision authority for native-vs-bridge backend choice"
affects:
  - "Phase 25 MAVLink-native backend PR (first required citation)"
  - "Future Spot / Franka / ROS2 / UR / Stretch backend PRs"
tech-stack-added: []
tech-stack-patterns:
  - "Normative single-rule policy doc rooted in an in-tree trait surface citation"
key-files-created:
  - "docs/integration-policy.md"
key-files-modified: []
decisions:
  - "Authored as a normative policy doc (not an ADR, not a research-artifact mirror)"
  - "Verbatim canonical rule placed twice — top blockquote + Bottom Line restatement"
  - "crates/roz-mavlink phrased as a Phase 25 future reference (not present-tense)"
metrics:
  duration_seconds: 174
  completed: "2026-04-17T14:03:32Z"
  file_size_bytes: 7383
  file_line_count: 98
  tasks_completed: 1
requirements_closed:
  - INT-01 (doc side — PR-citation plumbing in Plan 02, code cross-link in Plan 03)
---

# Phase 22 Plan 01: Author docs/integration-policy.md Summary

**One-liner:** Published `docs/integration-policy.md` as the normative single-rule decision authority for native-vs-bridge backend choice in roz — rule, trait contract citation, adapter diagram + Rust sketch, 5-backend verdict table, 4-step rubric, Known Limitations, and Bottom Line.

## Deliverable

- **File path:** `docs/integration-policy.md` (98 lines, 7383 bytes)
- **Commit:** `edb4b74`
- **Structure (D-01 seven sections in order):**
  1. `# Integration Policy: Native vs Bridge Backends` (title)
  2. Top blockquote with canonical rule (verbatim)
  3. `## The Rule`
  4. `## Trait Contract`
  5. `## Canonical Native-Backend Pattern`
  6. `## Per-Backend Verdicts`
  7. `## How to Evaluate a New Backend`
  8. `## Known Limitations`
  9. `## Bottom Line` (verbatim rule restated + closing citation sentence)

## Verification — All Acceptance Criteria Pass

Automated grep checks from the plan (run against committed `docs/integration-policy.md`):

| Check | Result |
|---|---|
| `test -f docs/integration-policy.md` | ok |
| Verbatim rule appears exactly 2× (top blockquote + Bottom Line) | ok (count = 2) |
| `grep -qF 'ActuatorSink::send'` | ok |
| `grep -qF 'SensorSource::try_recv'` | ok |
| `grep -qF 'crates/roz-copper/src/io.rs'` | ok |
| `grep -qF '10 ms'` | ok |
| `grep -qF '100 Hz'` | ok |
| `grep -qF 'tokio::sync::mpsc'` | ok |
| All five backends (MAVLink / Gazebo / Spot / Franka / ROS2 or rclrs) | ok |
| D-08 exact MAVLink verdict cell `✅ NATIVE (signing posture: see Known Limitations)` | ok |
| D-01 section order monotonic (line numbers 5 < 11 < 21 < 69 < 79 < 88 < 94) | ok |
| D-02 rubric keywords present (rust bindings / 10 ms non-blocking / stricter than 100 Hz / verdict) | ok |
| D-09 Known Limitations markers (signing, 1 kHz) | ok |
| No smart/curly quotes anywhere in the doc | ok |

## Section Line Numbers (Monotonic Order Confirmed)

| Section | Line |
|---|---|
| `## The Rule` | 5 |
| `## Trait Contract` | 11 |
| `## Canonical Native-Backend Pattern` | 21 |
| `## Per-Backend Verdicts` | 69 |
| `## How to Evaluate a New Backend` | 79 |
| `## Known Limitations` | 88 |
| `## Bottom Line` | 94 |

## Verbatim Rule — Appears Twice

Both occurrences (top blockquote after the title, and the Bottom Line blockquote) carry the character-exact string:

> Everything terminates at copper's I/O traits. Native backend when the vendor API satisfies copper's sync non-blocking 100 Hz tick; bridge backend when it can't (language boundary, SDK availability, stricter timing).

ASCII apostrophe in `copper's` and `can't` confirmed (no smart quotes).

## Deviations from Plan

### Minor textual addition (no rule break)

**1. [Clarifying sentence under the canonical pattern section]** Added one sentence after the Rust sketch:

> `crates/roz-mavlink` (introduced in Phase 25) will be the reference implementation of this pattern — an async tokio reader over serial or UDP pushing parsed MAVLink frames into an `mpsc` queue that copper drains each tick. Until that crate lands, treat the diagram and sketch above as normative shape guidance rather than a citation into the tree.

- **Why:** Reinforces Pitfall 5 (avoid present-tense references to `crates/roz-mavlink` — it doesn't exist yet) by explicitly telling the reader how to treat the sketch until Phase 25 lands. This makes the Phase 25 future-tense framing more robust against a reader skimming only the code block.
- **Impact on acceptance criteria:** None. The sentence contains no forbidden tokens, introduces no new claim, and sits inside the existing "Canonical Native-Backend Pattern" section (does not change section order or count).
- **Classification:** Not a Rules 1–4 deviation — this is editorial clarification within the plan's discretionary "exact prose" zone (per 22-CONTEXT.md §Claude's Discretion).

### Other deviations

None. All 7 sections, both rule instances, the verdict table (5 rows in required order), the rubric (4 numbered steps), and the 3 Known Limitations disclosures match the plan's spec.

## Files Touched

- `docs/integration-policy.md` — new file (98 lines, 7383 bytes). Committed as `edb4b74`.

No other files modified this plan. PR-citation enforcement (`.github/pull_request_template.md`, `CONTRIBUTING.md`) is Plan 02's scope; code cross-linking (`crates/roz-copper/src/io.rs` module docstring, `README.md` bullet) is Plan 03's scope.

## Threat Flags

None. Doc-only artifact, no executable code, no secrets, no new input surface, no network endpoint. Aligns with T-22-01 (accept — public policy language + already-public trait signatures) and T-22-02 (accept — git history + verbatim grep check catch any drift in rule wording).

## Key Decisions

- **Normative, not ADR.** Plan D-01 explicitly rejects ADR framing (Context / Decision / Consequences). Doc opens with the title, the rule blockquote, and `## The Rule` framing the rule's purpose — not a historical evaluation record.
- **Verbatim rule copied from `.planning/REQUIREMENTS.md:13` (the bolded clause inside `**"..."**`).** Cross-verified character-exact against `.planning/ROADMAP.md:80` and `.planning/phases/22-.../22-CONTEXT.md:41`. Did NOT copy from `.planning/research/INTEGRATION-POLICY.md:5` (textually divergent — would fail the verbatim grep).
- **`crates/roz-mavlink` future-tense.** Every reference phrased as "(introduced in Phase 25)" / "will be the reference implementation" / "Until that crate lands." No present-tense claim the crate exists today.

## Self-Check: PASSED

File exists:
- `docs/integration-policy.md` — FOUND

Commit exists:
- `edb4b74` (`feat(22-01): author docs/integration-policy.md as native-vs-bridge decision authority`) — FOUND in git log

All 14 plan verification checks run against the committed artifact — all pass.

---

*Plan 22-01 complete. Phase 22 continues with Plan 02 (PR-citation enforcement) and Plan 03 (code cross-linking + README pointer) — independent and expected to execute in wave 1 in parallel.*

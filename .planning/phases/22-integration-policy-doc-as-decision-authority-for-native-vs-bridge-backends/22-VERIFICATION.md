---
phase: 22-integration-policy-doc-as-decision-authority-for-native-vs-bridge-backends
verified: 2026-04-17T18:00:00Z
status: passed
score: 9/9 must-haves verified
overrides_applied: 0
---

# Phase 22: Integration Policy Doc as Decision Authority Verification Report

**Phase Goal:** Publish `docs/integration-policy.md` as the normative single-rule decision authority for choosing native vs bridge backends in roz, with supporting plumbing (PR template, CONTRIBUTING section, io.rs docstring pointer, README cross-link).
**Verified:** 2026-04-17T18:00:00Z
**Status:** passed
**Re-verification:** No — initial verification

## Goal Achievement

### Observable Truths

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| 1 | `docs/integration-policy.md` exists at repo-root-relative path | VERIFIED | File present, 99 lines |
| 2 | Canonical verbatim rule appears exactly 2× | VERIFIED | `grep -cF` returns `2` (top blockquote L3 + Bottom Line L96) |
| 3 | All 5 backends covered (MAVLink, Gazebo, Spot, Franka, ROS2/rclrs) | VERIFIED | Per-Backend Verdicts table L73-77 has all 5 rows in required order |
| 4 | MAVLink verdict is authoritative `NATIVE` | VERIFIED | Exact string `✅ NATIVE (signing posture: see Known Limitations)` present (L73) |
| 5 | `.github/pull_request_template.md` exists with single checkbox citing policy | VERIFIED | 1-line file, checkbox matches `^- \[ \].*backend.*cites.*docs/integration-policy\.md` |
| 6 | `CONTRIBUTING.md` has `## Backend integrations` section between Conventions and Testing | VERIFIED | Conventions L23 < Backend integrations L32 < Testing L38 |
| 7 | `crates/roz-copper/src/io.rs` module docstring cross-links to policy doc | VERIFIED | L1-3 contain 3-line `//!` form with backticked `docs/integration-policy.md` pointer |
| 8 | `README.md` cross-links to policy doc | VERIFIED | `## Documentation` section L75 with `[Integration policy](docs/integration-policy.md)` bullet L77 |
| 9 | All 3 plans have SUMMARY.md files in phase directory | VERIFIED | `22-01-SUMMARY.md`, `22-02-SUMMARY.md`, `22-03-SUMMARY.md` all present |

**Score:** 9/9 truths verified

### Required Artifacts

| Artifact | Expected | Status | Details |
|----------|----------|--------|---------|
| `docs/integration-policy.md` | Normative policy doc, 7 H2 sections, verbatim rule 2x, 5-backend table, 4-step rubric, Known Limitations | VERIFIED | 99 lines. All 7 sections present in D-01 order: The Rule (L5), Trait Contract (L11), Canonical Native-Backend Pattern (L21), Per-Backend Verdicts (L69), How to Evaluate a New Backend (L79), Known Limitations (L88), Bottom Line (L94). Monotonic. |
| `.github/pull_request_template.md` | Single-checkbox PR template citing policy (D-06 strict-minimal) | VERIFIED | 1 line, no headings, exact checkbox form present |
| `CONTRIBUTING.md` (modified) | Add `## Backend integrations` H2 between Conventions and Testing (sentence-case) | VERIFIED | H2 count 6 (was 5). Correct position, sentence-case heading, references policy doc. |
| `crates/roz-copper/src/io.rs` (modified) | 3-line module docstring with backticked policy pointer | VERIFIED | Lines 1-3 match expected form. Trait/struct definitions unchanged (L30-40). |
| `README.md` (modified) | `## Documentation` section between Examples and Status linking to policy | VERIFIED | H2 count 8 (was 7). Examples L68 < Documentation L75 < Status L79. |
| `22-01-SUMMARY.md` / `22-02-SUMMARY.md` / `22-03-SUMMARY.md` | Per-plan SUMMARY files | VERIFIED | All 3 present in phase directory |

### Key Link Verification

| From | To | Via | Status | Details |
|------|----|----|--------|---------|
| `docs/integration-policy.md` | `crates/roz-copper/src/io.rs` | Textual citation of `ActuatorSink::send` + `SensorSource::try_recv` + crate path | VERIFIED | All three tokens present: `ActuatorSink::send(&self, frame: &CommandFrame)` (L15), `SensorSource::try_recv(&mut self)` (L16), `crates/roz-copper/src/io.rs` (L13) |
| `.github/pull_request_template.md` | `docs/integration-policy.md` | Markdown reference in checkbox line | WIRED | Single checkbox line cites the path in backticks |
| `CONTRIBUTING.md` | `docs/integration-policy.md` | Markdown reference in `## Backend integrations` section | WIRED | 2 backtick references in section body (L34, L36) |
| `crates/roz-copper/src/io.rs` | `docs/integration-policy.md` | `//!` module docstring pointer | WIRED | L3: `//! Backend-choice policy: see \`docs/integration-policy.md\`.` |
| `README.md` | `docs/integration-policy.md` | Markdown link in `## Documentation` section | WIRED | L77: `[Integration policy](docs/integration-policy.md)` |

### Data-Flow Trace (Level 4)

Not applicable — phase produces documentation and doc-comment artifacts only. No runtime data flow to trace.

### Behavioral Spot-Checks

| Behavior | Command | Result | Status |
|----------|---------|--------|--------|
| Verbatim rule appears exactly 2× | `grep -cF "Everything terminates at copper's I/O traits..."` | `2` | PASS |
| MAVLink authoritative verdict cell present | `grep -F "✅ NATIVE (signing posture: see Known Limitations)"` | 1 match (L73) | PASS |
| All 5 backends present | `grep -c` on MAVLink/Gazebo/Spot/Franka/ROS2 | 5/5/3/4/2 occurrences each | PASS |
| D-01 section order monotonic | Ordered grep of H2 line numbers | 5 < 11 < 21 < 69 < 79 < 88 < 94 | PASS |
| PR template has no H1/H2 (strict-minimal) | `grep -cE '^#'` | `0` | PASS |
| CONTRIBUTING H2 count = 6 | `grep -c '^## '` | `6` | PASS |
| README H2 count = 8 | `grep -c '^## '` | `8` | PASS |
| io.rs docstring 3-line form | `head -3` matches expected 3 `//!` lines | exact match | PASS |

### Requirements Coverage

| Requirement | Source Plan | Description | Status | Evidence |
|-------------|-------------|-------------|--------|----------|
| INT-01 | 22-01, 22-02, 22-03 | Publish `docs/integration-policy.md` capturing the native-vs-bridge rule; covers trait surface at `crates/roz-copper/src/io.rs`, canonical adapter shape, worked verdicts for 5 backends; cited by every new backend PR | SATISFIED | Doc published with verbatim rule 2×; trait surface symbols cited; 5-backend verdict table; cross-linked from PR template, CONTRIBUTING, io.rs, and README (citation plumbing in place for future PRs) |

REQUIREMENTS.md line 96 maps INT-01 to Phase 22. All three plans in this phase declare `requirements: [INT-01]` in frontmatter. No orphaned requirements for this phase.

### Anti-Patterns Found

None. Files modified in this phase are documentation (markdown) and a 2-line doc-comment addition. Scanned for TODO/FIXME/placeholder/stub markers — none introduced. No code paths added.

### Human Verification Required

None. All must-haves are textual artifacts verifiable by grep/file existence checks. The only gray-area item (PR template auto-loading into PR bodies) is a GitHub behavior, not a code path — it will be observed when the first Phase 25 MAVLink PR opens, but that is future-phase exercise rather than Phase 22 verification.

### Gaps Summary

No gaps. All 9 must-haves verified. Phase goal (publish normative decision authority + supporting plumbing) is achieved end-to-end:

- Canonical policy doc present at required path with required structure and exact verbatim rule placement.
- All 5 required backend verdicts present with correct MAVLink authoritative wording.
- Trait surface citation is lossless (symbols + file path + tick budget).
- All 4 cross-link surfaces (PR template, CONTRIBUTING, io.rs docstring, README) reference the doc.
- Section ordering in all modified files is monotonic and non-disruptive.
- All 3 plans produced SUMMARY.md files; phase is internally consistent.

Downstream readiness: Phase 25 (MAVLink-native) and future v3.1+ backend PRs can now cite this doc as required by INT-01.

---

_Verified: 2026-04-17T18:00:00Z_
_Verifier: Claude (gsd-verifier)_

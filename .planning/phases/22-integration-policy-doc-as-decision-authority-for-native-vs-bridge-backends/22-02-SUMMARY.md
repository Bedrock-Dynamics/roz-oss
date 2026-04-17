---
phase: 22-integration-policy-doc-as-decision-authority-for-native-vs-bridge-backends
plan: 02
subsystem: documentation
tags: [docs, contributing, pr-template, integration-policy, INT-01]
requires:
  - phase: 22-integration-policy-doc-as-decision-authority-for-native-vs-bridge-backends
    provides: "docs/integration-policy.md (Plan 22-01) — the doc citations point to"
provides:
  - "Repo-default GitHub PR template carrying the advisory backend-citation checkbox"
  - "CONTRIBUTING.md '## Backend integrations' rule referencing docs/integration-policy.md"
affects:
  - "Phase 25 MAVLink-native backend PR (first required citation via the new template)"
  - "Future Spot / Franka / ROS2 / UR / Stretch backend PRs (durable CONTRIBUTING rule)"
tech-stack:
  added: []
  patterns:
    - "Repo-default .github/pull_request_template.md for advisory contributor enforcement"
    - "CONTRIBUTING.md sentence-case H2 heading consistent with existing sections"
key-files:
  created:
    - .github/pull_request_template.md
  modified:
    - CONTRIBUTING.md
key-decisions:
  - "Strict-minimal PR template shape — single checkbox line, no headings (D-06 expansion lock)"
  - "CONTRIBUTING.md insertion between '## Conventions' and '## Testing' (sentence-case heading)"
  - "No pre-existing CONTRIBUTING.md content modified — pure addition to preserve stable sections"
patterns-established:
  - "Advisory-only PR body enforcement (no CI lint, per D-05 / T-22-03 accept disposition)"
  - "Citation rule surfaces at two points: GitHub PR body (template) + human-readable contributor guide"
requirements-completed:
  - INT-01
metrics:
  duration_seconds: 109
  completed: "2026-04-17T14:05:36Z"
  tasks_completed: 2
  files_created: 1
  files_modified: 1
---

# Phase 22 Plan 02: PR template + CONTRIBUTING backend-integrations rule Summary

**One-liner:** Wired light-touch PR-citation enforcement for `docs/integration-policy.md` — new repo-default GitHub PR template carries a single advisory checkbox (D-05, D-06), and `CONTRIBUTING.md` gains a new sentence-case `## Backend integrations` section (D-07) between `## Conventions` and `## Testing`.

## Performance

- **Duration:** 109 seconds (~2 min)
- **Started:** 2026-04-17T14:03:47Z
- **Completed:** 2026-04-17T14:05:36Z
- **Tasks:** 2
- **Files created:** 1 (`.github/pull_request_template.md`)
- **Files modified:** 1 (`CONTRIBUTING.md`)

## Accomplishments

- Created `.github/pull_request_template.md` as a **strict-minimal single-line file** (exactly one non-blank line, no headings, no prose padding) containing the advisory checkbox referencing `docs/integration-policy.md`. Honors D-06's "(only — do not expand beyond this scope)" expansion lock verbatim.
- Inserted new `## Backend integrations` H2 in `CONTRIBUTING.md` between the existing `## Conventions` (line 23) and `## Testing` (line 38) sections. Two short paragraphs: one stating the rule, one referencing the doc's rubric (Rust bindings / 10 ms non-blocking tick / vendor timing).
- **Zero modifications to pre-existing CONTRIBUTING.md content** — `git diff` on `CONTRIBUTING.md` shows `1 file changed, 6 insertions(+)` and no `-` lines. The 5 pre-existing H2 sections (`Getting Started`, `Development Workflow`, `Conventions`, `Testing`, `License`) are character-identical to their pre-plan state; total H2 count increases from 5 to 6.
- Section-order invariant preserved: `Conventions (L23) < Backend integrations (L32) < Testing (L38)`.

## Task Commits

Each task committed atomically with `--no-verify` per parallel-executor protocol:

1. **Task 1: Create `.github/pull_request_template.md`** — `9e1d74b` (feat)
   - `feat(22-02): add PR template checkbox citing integration policy`
2. **Task 2: Append `## Backend integrations` to `CONTRIBUTING.md`** — `202f600` (docs)
   - `docs(22-02): add CONTRIBUTING.md Backend integrations section`

## Files Created

### `.github/pull_request_template.md` (new, 1 line)

Exact content (single checkbox line + trailing newline, no headings):

```
- [ ] If this PR adds or changes a vendor backend, it cites `docs/integration-policy.md` in the description.
```

GitHub auto-loads this into the PR body for all new PRs in the repo. Strict-minimal shape deliberately chosen per D-06 expansion lock (22-RESEARCH.md Pitfall 3 warning against expansion creep).

## Files Modified

### `CONTRIBUTING.md` (6 line additions, 0 modifications)

Diff summary: `1 file changed, 6 insertions(+)` — pure addition.

Exact block inserted between existing L30 (last bullet of `## Conventions`) and existing L32 (start of `## Testing`):

```markdown
## Backend integrations

PRs that add or change a vendor backend (MAVLink, Gazebo, Spot, Franka, ROS2, or any new robot family) must cite `docs/integration-policy.md` in the PR description and justify the native-vs-bridge choice against the rubric documented there.

New backends are evaluated per the rubric in `docs/integration-policy.md`: Rust bindings availability, copper's 10 ms non-blocking tick compatibility, and vendor timing requirements.
```

Post-edit CONTRIBUTING.md section inventory (6 H2 sections):

| # | Heading | Line |
|---|---|---|
| 1 | `## Getting Started` | 5 |
| 2 | `## Development Workflow` | 15 |
| 3 | `## Conventions` | 23 |
| 4 | `## Backend integrations` *(new)* | 32 |
| 5 | `## Testing` | 38 |
| 6 | `## License` | 44 |

## Verification — All Acceptance Criteria Pass

### Task 1 — `.github/pull_request_template.md`

| Check | Result |
|---|---|
| `test -f .github/pull_request_template.md` | ok |
| `grep -qF 'docs/integration-policy.md' .github/pull_request_template.md` | ok |
| Checkbox regex `^- \[ \].*backend.*cites.*docs/integration-policy\.md` | ok |
| Strict-minimal: no `^#` headings (`grep -cE '^#' == 0`) | ok |
| Line count `wc -l <= 3` (actual: 1) | ok |

### Task 2 — `CONTRIBUTING.md`

| Check | Result |
|---|---|
| `grep -q '^## Backend integrations' CONTRIBUTING.md` (sentence case) | ok |
| `grep -qF 'docs/integration-policy.md' CONTRIBUTING.md` | ok |
| All 5 pre-existing H2s still present (Getting Started / Development Workflow / Conventions / Testing / License) | ok |
| Total H2 count == 6 (5 prior + 1 new) | ok |
| Monotonic placement: `L(Conventions) < L(Backend integrations) < L(Testing)` → `23 < 32 < 38` | ok |
| Negative assertion: no title-case variant `## Backend Integrations` | ok |
| `git diff CONTRIBUTING.md` shows only additions (`6 insertions(+)`, 0 modifications) | ok |

## Decisions Made

- **Strict-minimal PR template shape (single checkbox, no headings).** D-06 locks expansion. 22-RESEARCH.md explicitly surfaces this as a user-confirmation item; defaulted to the safe strict-minimal shape that requires zero user ratification and cleanly honors the plan literal. Added heading scaffolding would have been silent expansion beyond locked scope.
- **CONTRIBUTING.md insertion between `## Conventions` and `## Testing`.** 22-RESEARCH.md §Existing File Shapes §CONTRIBUTING.md named this as the primary recommendation on the rationale that a backend-addition rule is a convention extending `## Conventions` — natural adjacency, no reorder of existing content, no disruption to the PR-workflow narrative in `## Development Workflow`.
- **Sentence-case heading (`## Backend integrations`, lowercase 'integrations').** Matches the existing heading pattern in this file (`## Getting Started`, `## Development Workflow`). Title-case `## Backend Integrations` would have broken the style pattern and triggered the negative-assertion acceptance check.

## Deviations from Plan

None. Plan executed exactly as written.

Both tasks followed the plan's `<action>` sections verbatim, including the recommended text blocks verbatim as specified in Task 1 action and Task 2 action respectively. All explicit and automated acceptance criteria in the plan pass without modification. No Rule 1–4 deviations were triggered.

## Threat Flags

None. Both artifacts are repository documentation — no trust boundary introduced, no executable surface, no input validation, no network endpoint, no new security-relevant state.

Per the plan's threat model:
- **T-22-03 (Repudiation — author bypasses checkbox):** accepted. Advisory-only enforcement by design (D-05 rejected CI lint).
- **T-22-04 (Tampering — CONTRIBUTING drift over time):** accepted. Git history is the amendment trail.

No new threats introduced by this plan's execution.

## Self-Check

**Files created:**
- `.github/pull_request_template.md` — FOUND (1 line)

**Files modified:**
- `CONTRIBUTING.md` — FOUND (46 lines post-edit, 6 H2 sections)

**Commits:**
- `9e1d74b` (`feat(22-02): add PR template checkbox citing integration policy`) — FOUND in git log
- `202f600` (`docs(22-02): add CONTRIBUTING.md Backend integrations section`) — FOUND in git log

**Plan verification commands (re-executed against working tree):**
- All 5 Task 1 checks: pass
- All 7 Task 2 checks: pass
- Combined plan-level verification (9 checks from `<verification>` section): pass

## Self-Check: PASSED

---

*Plan 22-02 complete. PR-citation enforcement is wired. The first required citation will come from Phase 25's MAVLink-native backend PR. Plan 22-03 (code cross-link + README bullet) runs in parallel in wave 1.*

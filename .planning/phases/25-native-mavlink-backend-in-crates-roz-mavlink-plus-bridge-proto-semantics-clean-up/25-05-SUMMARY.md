---
phase: 25-native-mavlink-backend-in-crates-roz-mavlink-plus-bridge-proto-semantics-clean-up
plan: 05
subsystem: roz-mavlink/signing-and-result-shift
tags: [mavlink, signing, crypto, proto, mav-result, wire-boundary]
requires:
  - upstream mavlink 0.17.1 with signing + common features (plan 25-01)
  - roz_copper::proto_v2::MavResult (plan 25-04 codegen)
  - roz_copper::io::MavResult / DiscreteCommandSink<FlightCommand> (plan 25-02)
provides:
  - roz_mavlink::signing::MavlinkSigningConfig + SigningPosture + TransportKind
  - roz_mavlink::signing::build_signing_data(...) -> Option<mavlink::SigningData>
  - roz_mavlink::signing::build_setup_signing_message(...) -> Option<MavMessage>
  - roz_mavlink::signing::fresh_initial_timestamp_us() -> u64
  - roz_mavlink::mav_result::{mav_result_from_wire, mav_result_to_wire, io_mav_result_from_wire, proto_from_io}
affects:
  - plan 25-06 (transports will consume build_signing_data + TransportKind)
  - plan 25-09 (flight_command will call io_mav_result_from_wire on COMMAND_ACK result byte)
  - plan 25-12 (backend assembly will hold MavlinkSigningConfig and dispatch build_setup_signing_message on RF-link bring-up)
tech-stack:
  added:
    - "mavlink::common::MavMessage::SETUP_SIGNING (msg 256) builder path"
    - "mavlink::SigningConfig::new / SigningData::from_config wrapper"
  patterns:
    - "thin wrapper over upstream crypto primitives — no hand-rolled HMAC/SHA (D-01' / T-25-05-02 mitigation)"
    - "wire-boundary shift helpers at trust boundary (IoMavResult <-> u8 <-> ProtoMavResult) per D-08'"
    - "fail-closed unknown-value policy (io_mav_result_from_wire unknowns -> Failed; mav_result_from_wire unknowns -> Unspecified sentinel)"
key-files:
  created: []
  modified:
    - crates/roz-mavlink/src/signing.rs
    - crates/roz-mavlink/src/mav_result.rs
decisions:
  - "Use upstream mavlink::SigningConfig::new verbatim (4-arg signature); no initial_timestamp seed hook because mavlink-core 0.17.1 does not expose one — D-20 defers restart-safe timestamp persistence to Phase 27."
  - "Populate SETUP_SIGNING_DATA.initial_timestamp on-wire payload with fresh wall-clock (10-µs-since-2015 ticks); distinct from internal SigningData state timestamp which upstream keeps pub(crate)."
  - "Drop the earlier ACK-correlation design: SETUP_SIGNING is a MESSAGE (id 256), not a MAV_CMD; FCUs do not reply with COMMAND_ACK. Liveness is first-signed-HEARTBEAT receipt, owned by plan 25-12 backend."
  - "Adapt match arms to prost's unprefixed variant spelling (Unspecified/Accepted/...) — plan assumed prefixed (MavResultUnspecified/...) but codegen verified at target/debug/.../substrate.sim.v2.rs shows unprefixed."
  - "Allow clippy::match_same_arms on io_mav_result_from_wire with #[expect(..., reason = ...)] — the explicit wire=4 arm documents MAV_RESULT_FAILED while the wildcard enforces fail-closed policy; collapsing them would lose the documentation intent."
metrics:
  duration: "~25 min"
  completed: 2026-04-20
  tasks_completed: 2
---

# Phase 25 Plan 05: Signing Wrapper + MavResult Shift Summary

**One-liner:** Thin wrapper over `mavlink 0.17.1` `SigningConfig`/`SigningData` primitives with a `SETUP_SIGNING (msg 256)` builder, plus proto3-shifted `MavResult` wire-boundary helpers for the `roz-mavlink` backend.

## What Shipped

### `crates/roz-mavlink/src/signing.rs` (Task 1)

Replaced the doc-comment-only stub from plan 25-01 with a production wrapper:

- `SigningPosture { Off, On, Auto }` — loaded from `roz.toml [mavlink.signing]` per D-03. `Auto` resolves to `Off` on serial, `On` on UDP.
- `TransportKind { Serial, Udp }` — enum carrying the per-link transport identity used to resolve `SigningPosture::Auto`.
- `MavlinkSigningConfig { seed: Option<[u8;32]>, posture, allow_unsigned, local_link_id }` — carries the decrypted 32-byte seed from the per-host `roz_hosts.mavlink_signing_key_*` columns (plan 25-10 migration, plan 25-11 provisioning). `seed: None` means a pre-migration host per D-12 → signing force-disabled with a tracing warning. Default `local_link_id: 1` per D-04 (copper = 1).
- `build_signing_data(&MavlinkSigningConfig, TransportKind) -> Option<mavlink::SigningData>` — returns `None` when seed is absent OR posture resolves off; otherwise wraps upstream `SigningConfig::new(seed, link_id, sign_outgoing=true, allow_unsigned)` + `SigningData::from_config(...)`.
- `build_setup_signing_message(&MavlinkSigningConfig, target_system, target_component) -> Option<MavMessage>` — constructs `mavlink::common::MavMessage::SETUP_SIGNING(SETUP_SIGNING_DATA { target_system, target_component, secret_key, initial_timestamp })` for RF-link bring-up per D-14'. Returns `None` when seed is absent.
- `fresh_initial_timestamp_us() -> u64` — produces the on-wire `initial_timestamp` payload value in MAVLink's 10-µs-since-2015-01-01 UTC unit (constant `MAVLINK_EPOCH_UNIX_SECS = 1_420_070_400`). NOT used to seed internal `SigningData` state (upstream does not expose that — D-20 defers to Phase 27).

7 unit tests cover posture resolution, `build_signing_data` seed/posture gating in both directions, message builder round-trip (extracts key + timestamp + target fields), and monotonic timestamp behavior.

### `crates/roz-mavlink/src/mav_result.rs` (Task 2)

Replaced the doc-comment-only stub from plan 25-01 with four wire-boundary helpers per D-08':

- `mav_result_from_wire(u8) -> proto_v2::MavResult` — inbound MAVLink `0..=6` to shifted proto (`Accepted=1..Cancelled=7`). Unknown wire values map to `Unspecified` (proto3 sentinel).
- `mav_result_to_wire(proto_v2::MavResult) -> Option<u8>` — inverse; `Unspecified` sentinel returns `None` (backend must never emit the sentinel on wire).
- `io_mav_result_from_wire(u8) -> roz_copper::io::MavResult` — inbound wire to the non-proto Rust enum used by `DiscreteCommandSink<FlightCommand>::send_command`. Unknown wire values fail-closed to `Failed` (T-25-05-04 mitigation).
- `proto_from_io(roz_copper::io::MavResult) -> proto_v2::MavResult` — infallible io→proto bridge.

6 unit tests cover: `wire_round_trip_through_proto`, `unknown_wire_maps_to_unspecified_in_proto`, `unknown_wire_maps_to_failed_in_io`, `proto_sentinel_has_no_wire_value`, `io_accepted_is_wire_zero`, `io_to_proto_round_trip`.

## Commits

| Task | Commit  | Files                                      |
|------|---------|--------------------------------------------|
| 1    | ab7477c | crates/roz-mavlink/src/signing.rs          |
| 2    | 03b703d | crates/roz-mavlink/src/mav_result.rs       |

## Deviations from Plan

### Rule 1 / Rule 3 adjustments (small, auto-applied)

**1. [Rule 1 - Bug] Prost enum variant naming did not match plan assumptions**
- **Found during:** Task 2
- **Issue:** Plan's `<action>` block used `ProtoMavResult::MavResultAccepted` / `MavResultUnspecified` / etc. (prefixed form). The generated code at `target/debug/build/roz-copper-*/out/substrate.sim.v2.rs` emits prost's default PascalCase without the enum prefix: `Unspecified`, `Accepted`, `TemporarilyRejected`, `Denied`, `Unsupported`, `Failed`, `InProgress`, `Cancelled`. Plan explicitly authorized this adjustment in its own Guidance block ("If the generated code drops the prefix, rename the match arms.").
- **Fix:** Substituted the unprefixed variant names across all four helper functions and all 6 test assertions.
- **Files modified:** crates/roz-mavlink/src/mav_result.rs
- **Commit:** 03b703d

**2. [Rule 3 - Blocking] Clippy `match_same_arms` triggered on `io_mav_result_from_wire`**
- **Found during:** Task 2 clippy run
- **Issue:** `pedantic`+`nursery` workspace lints (CLAUDE.md) include `match_same_arms`. The explicit `4 => IoMavResult::Failed` arm has the same body as the wildcard `_ => IoMavResult::Failed`, so clippy demanded collapse. Collapsing would lose the documentation intent (wire=4 is MAV_RESULT_FAILED per MAVLink spec; wildcard is fail-closed policy for unknowns).
- **Fix:** Added `#[expect(clippy::match_same_arms, reason = "...")]` on the function with a reason explaining the intentional separation. Consistent with workspace convention (CLAUDE.md § Code Style: "use targeted #[allow(...)] or #[expect(..., reason = '...')] directly on the item").
- **Files modified:** crates/roz-mavlink/src/mav_result.rs
- **Commit:** 03b703d

**3. [Rule 3 - Blocking] rustfmt reformatted `fresh_initial_timestamp_us` and `build_signing_data` call-arg layout**
- **Found during:** Task 1 `cargo fmt --check`
- **Issue:** Plan's prescribed `SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default()` was initially written with multi-line chaining; fmt wanted it on one line. `SigningConfig::new(seed, local_link_id, /* sign_outgoing */ true, allow_unsigned)` was on one line; fmt wanted multi-line (exceeds 120-col).
- **Fix:** Ran `cargo fmt -p roz-mavlink`. Non-semantic layout-only change.
- **Files modified:** crates/roz-mavlink/src/signing.rs
- **Commit:** ab7477c (applied pre-commit)

## Verification Evidence

```
cargo build -p roz-mavlink         # OK
cargo test -p roz-mavlink --lib    # 30 passed; 0 failed (includes 7 signing + 6 mav_result + 17 existing)
cargo clippy -p roz-mavlink -- -D warnings  # OK (no warnings)
cargo fmt -p roz-mavlink --check   # OK
```

Named plan verification greps (all pass):
- `grep -q 'pub enum SigningPosture' crates/roz-mavlink/src/signing.rs`
- `grep -q 'pub struct MavlinkSigningConfig' crates/roz-mavlink/src/signing.rs`
- `grep -q 'SigningConfig::new' crates/roz-mavlink/src/signing.rs`
- `grep -q 'SigningData::from_config' crates/roz-mavlink/src/signing.rs`
- `grep -q 'build_setup_signing_message' crates/roz-mavlink/src/signing.rs`
- `grep -q 'fresh_initial_timestamp_us' crates/roz-mavlink/src/signing.rs`
- `grep -q 'MAVLINK_EPOCH_UNIX_SECS' crates/roz-mavlink/src/signing.rs`
- `! grep -q 'use hmac' crates/roz-mavlink/src/signing.rs` (no hand-rolled crypto — T-25-05-02)
- `! grep -q 'use sha2' crates/roz-mavlink/src/signing.rs`
- `! grep -q 'SigningConfig::new_with_timestamp' crates/roz-mavlink/src/signing.rs` (no nonexistent API — T-25-05-05)
- `! grep -q 'set_initial_timestamp' crates/roz-mavlink/src/signing.rs`
- `grep -q 'pub fn mav_result_from_wire' crates/roz-mavlink/src/mav_result.rs`
- `grep -q 'pub fn mav_result_to_wire' crates/roz-mavlink/src/mav_result.rs`
- `grep -q 'pub fn io_mav_result_from_wire' crates/roz-mavlink/src/mav_result.rs`
- `grep -q 'pub fn proto_from_io' crates/roz-mavlink/src/mav_result.rs`

## Threat Model Compliance

| Threat ID   | Disposition | Evidence                                                                                                   |
|-------------|-------------|------------------------------------------------------------------------------------------------------------|
| T-25-05-01  | mitigate    | Known acceptable (internal log level only). Seed travels via `Debug` of `MavlinkSigningConfig`. Manual redact deferred. |
| T-25-05-02  | mitigate    | `! grep -q 'use hmac'` + `! grep -q 'use sha2'` both pass — no hand-rolled crypto. Wrapper delegates to upstream. |
| T-25-05-03  | accept      | D-20 known limitation. `SigningData::from_config` starts internal `timestamp: 0`; upstream sign-path rescues to wall-clock on first frame. Phase 27 scopes WAL-seeded variant. |
| T-25-05-04  | mitigate    | `unknown_wire_maps_to_unspecified_in_proto` + `unknown_wire_maps_to_failed_in_io` tests both pass. |
| T-25-05-05  | mitigate    | `! grep -q 'SigningConfig::new_with_timestamp'` passes — plan called out nonexistent API; executor did not invent one. |

No new threat flags introduced: the files added do not create new network/auth/file surface. The 32-byte seed handling is internal to the worker process and does not touch any new trust boundary beyond what Phase 23 already established.

## Known Stubs

None. Both modules are fully implemented per the plan. Backend callers (plans 25-06 / 25-09 / 25-12) can depend on these helpers.

## TDD Gate Compliance

N/A — plan type is `execute` (not `tdd`). Tests ship alongside implementation in the same commit, which matches the plan's prescribed structure (unit tests in `#[cfg(test)] mod tests`).

## Self-Check: PASSED

Files exist:
- FOUND: crates/roz-mavlink/src/signing.rs
- FOUND: crates/roz-mavlink/src/mav_result.rs
- FOUND: .planning/phases/25-.../25-05-SUMMARY.md (this file)

Commits exist:
- FOUND: ab7477c (Task 1)
- FOUND: 03b703d (Task 2)

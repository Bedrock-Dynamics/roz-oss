use roz_core::errors::RozError;

const NATS_SPECIAL_CHARS: &[char] = &['.', '*', '>'];

/// Validate that a subject token is non-empty and contains no NATS special characters.
fn validate_token(name: &str, value: &str) -> Result<(), RozError> {
    if value.is_empty() {
        return Err(RozError::Validation(format!("{name} must not be empty")));
    }
    for ch in NATS_SPECIAL_CHARS {
        if value.contains(*ch) {
            return Err(RozError::Validation(format!("{name} must not contain '{ch}'")));
        }
    }
    Ok(())
}

/// Type-safe NATS subject builders.
///
/// Subject hierarchy:
/// - `telemetry.{host_id}.{sensor}`
/// - `cmd.{host_id}.{command}`
/// - `events.{host_id}.{event}`
/// - `tasks.{task_id}.{action}`
/// - `invoke.{worker_id}.{task_id}`
pub struct Subjects;

impl Subjects {
    /// Build a telemetry subject: `telemetry.{host_id}.{sensor}`.
    pub fn telemetry(host_id: &str, sensor: &str) -> Result<String, RozError> {
        validate_token("host_id", host_id)?;
        validate_token("sensor", sensor)?;
        Ok(format!("telemetry.{host_id}.{sensor}"))
    }

    /// Build a command subject: `cmd.{host_id}.{command}`.
    pub fn command(host_id: &str, command: &str) -> Result<String, RozError> {
        validate_token("host_id", host_id)?;
        validate_token("command", command)?;
        Ok(format!("cmd.{host_id}.{command}"))
    }

    /// Build an event subject: `events.{host_id}.{event}`.
    pub fn event(host_id: &str, event: &str) -> Result<String, RozError> {
        validate_token("host_id", host_id)?;
        validate_token("event", event)?;
        Ok(format!("events.{host_id}.{event}"))
    }

    /// Build a task subject: `tasks.{task_id}.{action}`.
    pub fn task(task_id: &str, action: &str) -> Result<String, RozError> {
        validate_token("task_id", task_id)?;
        validate_token("action", action)?;
        Ok(format!("tasks.{task_id}.{action}"))
    }

    /// Build a wildcard telemetry subject: `telemetry.{host_id}.>`.
    pub fn telemetry_wildcard(host_id: &str) -> Result<String, RozError> {
        validate_token("host_id", host_id)?;
        Ok(format!("telemetry.{host_id}.>"))
    }

    /// Return the catch-all telemetry subject: `telemetry.>`.
    pub fn all_telemetry() -> String {
        "telemetry.>".to_string()
    }

    /// Build an invocation subject: `invoke.{worker_id}.{task_id}`.
    pub fn invoke(worker_id: &str, task_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        validate_token("task_id", task_id)?;
        Ok(format!("invoke.{worker_id}.{task_id}"))
    }

    /// Build a wildcard invocation subject: `invoke.{worker_id}.>`.
    pub fn invoke_wildcard(worker_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        Ok(format!("invoke.{worker_id}.>"))
    }

    /// Build a telemetry state subject: `telemetry.{worker_id}.state`.
    pub fn telemetry_state(worker_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        Ok(format!("telemetry.{worker_id}.state"))
    }

    /// Build a telemetry sensors subject: `telemetry.{worker_id}.sensors`.
    pub fn telemetry_sensors(worker_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        Ok(format!("telemetry.{worker_id}.sensors"))
    }

    /// Build a capabilities subject: `capabilities.{worker_id}`.
    pub fn capabilities(worker_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        Ok(format!("capabilities.{worker_id}"))
    }

    // Pinned from pre-refactor source: subjects are D-18 BLOCKING. Any change to
    // these literals requires coordinated worker + server migration. Format is
    // "session.{worker_id}.{session_id}.{request|response|control}".
    //
    // Workers subscribe via `format!("session.{worker_id}.*.request")` (wildcard
    // hoisted inline in `crates/roz-worker/src/session_relay.rs:spawn_session_relay`).

    /// Build a session request subject: `session.{worker_id}.{session_id}.request`.
    pub fn session_request(worker_id: &str, session_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        validate_token("session_id", session_id)?;
        Ok(format!("session.{worker_id}.{session_id}.request"))
    }

    /// Build a session response subject: `session.{worker_id}.{session_id}.response`.
    pub fn session_response(worker_id: &str, session_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        validate_token("session_id", session_id)?;
        Ok(format!("session.{worker_id}.{session_id}.response"))
    }

    /// Build a session control subject: `session.{worker_id}.{session_id}.control`.
    pub fn session_control(worker_id: &str, session_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        validate_token("session_id", session_id)?;
        Ok(format!("session.{worker_id}.{session_id}.control"))
    }

    /// E-stop subject for a worker: `safety.estop.{worker_id}`.
    pub fn estop(worker_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        Ok(format!("safety.estop.{worker_id}"))
    }

    /// WASM signature verification failure subject:
    /// `safety.trust_failure.{worker_id}`.
    ///
    /// Emitted by `roz-worker` (at the caller boundary) when a `.cwasm`
    /// signature fails verification via `roz-copper::wasm_signature`.
    /// Complements `tracing::error!` at the failure site (Phase 14 / ENF-02).
    pub fn wasm_trust_failure(worker_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        Ok(format!("safety.trust_failure.{worker_id}"))
    }

    /// Phase 23 (FS-04 / D-09) worker-scoped signature-verification failure
    /// subject: `safety.signature_failure.{host_id}`.
    ///
    /// Publish-only. Emitted by the server-side verify gate when an inbound
    /// worker->server envelope fails signature verification, so ops tooling
    /// can subscribe by host. See `safety_signature_failure_server` for the
    /// tenant-level fan-in subject.
    pub fn safety_signature_failure_worker(host_id: &str) -> Result<String, RozError> {
        validate_token("host_id", host_id)?;
        Ok(format!("safety.signature_failure.{host_id}"))
    }

    /// Phase 23 (FS-04 / D-09) tenant-scoped signature-verification failure
    /// subject: `safety.signature_failure.server.{tenant_id}`.
    ///
    /// Publish-only. Server-scoped fan-in so ops can subscribe per tenant
    /// instead of per host. REQUIREMENTS.md section FS-04 requires both
    /// worker and server scoped subjects.
    pub fn safety_signature_failure_server(tenant_id: &str) -> Result<String, RozError> {
        validate_token("tenant_id", tenant_id)?;
        Ok(format!("safety.signature_failure.server.{tenant_id}"))
    }

    /// Build a WebRTC offer subject: `webrtc.{worker_id}.{peer_id}.offer`.
    pub fn webrtc_offer(worker_id: &str, peer_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        validate_token("peer_id", peer_id)?;
        Ok(format!("webrtc.{worker_id}.{peer_id}.offer"))
    }

    /// Build a WebRTC answer subject: `webrtc.{worker_id}.{peer_id}.answer`.
    pub fn webrtc_answer(worker_id: &str, peer_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        validate_token("peer_id", peer_id)?;
        Ok(format!("webrtc.{worker_id}.{peer_id}.answer"))
    }

    /// Build a local ICE candidate subject: `webrtc.{worker_id}.{peer_id}.ice.local`.
    pub fn webrtc_ice_local(worker_id: &str, peer_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        validate_token("peer_id", peer_id)?;
        Ok(format!("webrtc.{worker_id}.{peer_id}.ice.local"))
    }

    /// Build a remote ICE candidate subject: `webrtc.{worker_id}.{peer_id}.ice.remote`.
    pub fn webrtc_ice_remote(worker_id: &str, peer_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        validate_token("peer_id", peer_id)?;
        Ok(format!("webrtc.{worker_id}.{peer_id}.ice.remote"))
    }

    /// Build a wildcard WebRTC subject: `webrtc.{worker_id}.>`.
    pub fn webrtc_wildcard(worker_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        Ok(format!("webrtc.{worker_id}.>"))
    }

    /// Build a camera event subject: `camera.{worker_id}.event`.
    pub fn camera_event(worker_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        Ok(format!("camera.{worker_id}.event"))
    }

    /// Build a camera request subject: `camera.{worker_id}.request`.
    pub fn camera_request(worker_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        Ok(format!("camera.{worker_id}.request"))
    }

    /// Phase 26.5 SC5 (R-02) — session-scoped camera-frame wildcard subject:
    /// `camera.{worker_id}.{session_id}.*`.
    ///
    /// Plan 05's worker `mcap_relay` publishes signed
    /// `foxglove.CompressedVideo` frames to
    /// `camera.{worker_id}.{session_id}.{camera_id}` (4-token); this wildcard
    /// captures every camera_id for the session in one subscription without
    /// the server needing to enumerate cameras up front (R-02 dynamic
    /// registration on first sighting).
    ///
    /// Disjoint from the existing `camera.{worker}.event` +
    /// `camera.{worker}.request` subjects because those are 3-token (no
    /// session_id); the wildcard only matches the 4-token session form.
    pub fn camera_session_wildcard(worker_id: &str, session_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        validate_token("session_id", session_id)?;
        Ok(format!("camera.{worker_id}.{session_id}.*"))
    }

    // -----------------------------------------------------------------------
    // Phase 24 — FS-01 / FS-02 / FS-03 subjects
    // -----------------------------------------------------------------------

    /// Build a safety-policy push subject: `roz.policy.{worker_id}`.
    /// Server -> worker broadcast on `roz_safety_policies` row change (D-04).
    pub fn policy(worker_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        Ok(format!("roz.policy.{worker_id}"))
    }

    /// Build a 1 Hz liveness-report subject: `roz.health.{worker_id}`.
    /// Worker -> server reporting-only; explicitly NOT a control signal (FS-01).
    pub fn health(worker_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        Ok(format!("roz.health.{worker_id}"))
    }

    /// Build a safety-violation audit subject: `safety.violation.{worker_id}`.
    /// Worker -> server per-policy-violation stream (D-13).
    pub fn safety_violation(worker_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        Ok(format!("safety.violation.{worker_id}"))
    }

    /// Return the worker-online reconnect-handshake subject: `roz.state.worker_online`.
    /// Worker -> server publish on NATS reconnect (D-10). Not per-worker parameterized.
    #[must_use]
    pub const fn state_worker_online() -> &'static str {
        "roz.state.worker_online"
    }

    /// Build a signed clear-failsafe command subject: `cmd.{worker_id}.clear_failsafe`.
    /// Server -> worker explicit operator re-arm after deadman (D-02).
    pub fn clear_failsafe(worker_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        Ok(format!("cmd.{worker_id}.clear_failsafe"))
    }

    /// Build a worker-scoped resume-instruction subject: `roz.tasks.{worker_id}`.
    /// Server -> worker dispatch on reconnect (D-10). The worker subscribes
    /// and parses `ResumeInstruction` payloads published after the
    /// `roz.state.worker_online` handshake resolves. See Plan 24-12 Task 4
    /// for the worker-side subscriber.
    pub fn worker_tasks(worker_id: &str) -> Result<String, RozError> {
        validate_token("worker_id", worker_id)?;
        Ok(format!("roz.tasks.{worker_id}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_subject() {
        assert_eq!(Subjects::telemetry("host1", "imu").unwrap(), "telemetry.host1.imu");
    }

    #[test]
    fn command_subject() {
        assert_eq!(Subjects::command("host1", "arm").unwrap(), "cmd.host1.arm");
    }

    #[test]
    fn event_subject() {
        assert_eq!(Subjects::event("host1", "connected").unwrap(), "events.host1.connected");
    }

    #[test]
    fn task_subject() {
        assert_eq!(Subjects::task("task1", "started").unwrap(), "tasks.task1.started");
    }

    #[test]
    fn telemetry_wildcard_subject() {
        assert_eq!(Subjects::telemetry_wildcard("host1").unwrap(), "telemetry.host1.>");
    }

    #[test]
    fn all_telemetry_subject() {
        assert_eq!(Subjects::all_telemetry(), "telemetry.>");
    }

    #[test]
    fn empty_host_id_rejected() {
        let err = Subjects::telemetry("", "imu").unwrap_err();
        assert!(matches!(err, RozError::Validation(_)));
    }

    #[test]
    fn empty_sensor_rejected() {
        let err = Subjects::telemetry("host1", "").unwrap_err();
        assert!(matches!(err, RozError::Validation(_)));
    }

    #[test]
    fn host_id_with_dot_rejected() {
        let err = Subjects::telemetry("host.1", "imu").unwrap_err();
        assert!(matches!(err, RozError::Validation(_)));
    }

    #[test]
    fn host_id_with_star_rejected() {
        let err = Subjects::telemetry("host*1", "imu").unwrap_err();
        assert!(matches!(err, RozError::Validation(_)));
    }

    #[test]
    fn host_id_with_gt_rejected() {
        let err = Subjects::telemetry("host>1", "imu").unwrap_err();
        assert!(matches!(err, RozError::Validation(_)));
    }

    #[test]
    fn invoke_subject_valid() {
        let subject = Subjects::invoke("worker-1", "task-abc").unwrap();
        assert_eq!(subject, "invoke.worker-1.task-abc");
    }

    #[test]
    fn invoke_subject_rejects_empty_worker_id() {
        assert!(Subjects::invoke("", "task-abc").is_err());
    }

    #[test]
    fn invoke_wildcard_valid() {
        let subject = Subjects::invoke_wildcard("worker-1").unwrap();
        assert_eq!(subject, "invoke.worker-1.>");
    }

    #[test]
    fn estop_subject() {
        let subject = Subjects::estop("robot-arm-1").unwrap();
        assert_eq!(subject, "safety.estop.robot-arm-1");
    }

    #[test]
    fn estop_validates_worker_id() {
        assert!(Subjects::estop("valid-worker").is_ok());
        assert!(
            Subjects::estop("worker.with.dots").is_err(),
            "dots would break NATS subject hierarchy"
        );
        assert!(
            Subjects::estop("worker*wildcard").is_err(),
            "wildcards would match unintended subjects"
        );
        assert!(Subjects::estop("").is_err(), "empty worker_id is invalid");
        assert!(Subjects::estop("worker>greater").is_err(), "> is NATS full-wildcard");
    }

    #[test]
    fn wasm_trust_failure_subject() {
        let subject = Subjects::wasm_trust_failure("robot-1").unwrap();
        assert_eq!(subject, "safety.trust_failure.robot-1");
    }

    #[test]
    fn wasm_trust_failure_validates_worker_id() {
        assert!(Subjects::wasm_trust_failure("").is_err(), "empty rejected");
        assert!(Subjects::wasm_trust_failure("a.b").is_err(), "dots break hierarchy");
        assert!(Subjects::wasm_trust_failure("a*b").is_err(), "wildcards");
        assert!(Subjects::wasm_trust_failure("a>b").is_err(), "> is full-wildcard");
        assert!(Subjects::wasm_trust_failure("robot-1").is_ok());
    }

    #[test]
    fn safety_signature_failure_worker_subject() {
        assert_eq!(
            Subjects::safety_signature_failure_worker("abc").unwrap(),
            "safety.signature_failure.abc"
        );
        assert!(Subjects::safety_signature_failure_worker("bad.token").is_err());
        assert!(Subjects::safety_signature_failure_worker("").is_err());
        assert!(Subjects::safety_signature_failure_worker("star*worker").is_err());
        assert!(Subjects::safety_signature_failure_worker("gt>worker").is_err());
    }

    #[test]
    fn safety_signature_failure_server_subject() {
        assert_eq!(
            Subjects::safety_signature_failure_server("tenant-7").unwrap(),
            "safety.signature_failure.server.tenant-7"
        );
        assert!(Subjects::safety_signature_failure_server("bad>token").is_err());
        assert!(Subjects::safety_signature_failure_server("").is_err());
        assert!(Subjects::safety_signature_failure_server("dot.tenant").is_err());
        assert!(Subjects::safety_signature_failure_server("star*tenant").is_err());
    }

    #[test]
    fn telemetry_state_subject() {
        assert_eq!(Subjects::telemetry_state("robot1").unwrap(), "telemetry.robot1.state");
    }

    #[test]
    fn telemetry_sensors_subject() {
        assert_eq!(
            Subjects::telemetry_sensors("robot1").unwrap(),
            "telemetry.robot1.sensors"
        );
    }

    #[test]
    fn session_request_subject() {
        assert_eq!(
            Subjects::session_request("robot1", "sess-123").unwrap(),
            "session.robot1.sess-123.request"
        );
    }

    #[test]
    fn session_response_subject() {
        assert_eq!(
            Subjects::session_response("robot1", "sess-123").unwrap(),
            "session.robot1.sess-123.response"
        );
    }

    #[test]
    fn session_control_subject() {
        assert_eq!(
            Subjects::session_control("robot1", "sess-123").unwrap(),
            "session.robot1.sess-123.control"
        );
    }

    #[test]
    fn capabilities_subject() {
        assert_eq!(Subjects::capabilities("robot1").unwrap(), "capabilities.robot1");
    }

    #[test]
    fn webrtc_offer_subject() {
        assert_eq!(
            Subjects::webrtc_offer("robot1", "peer-abc").unwrap(),
            "webrtc.robot1.peer-abc.offer"
        );
    }

    #[test]
    fn webrtc_answer_subject() {
        assert_eq!(
            Subjects::webrtc_answer("robot1", "peer-abc").unwrap(),
            "webrtc.robot1.peer-abc.answer"
        );
    }

    #[test]
    fn webrtc_ice_subjects() {
        assert_eq!(
            Subjects::webrtc_ice_local("robot1", "peer-abc").unwrap(),
            "webrtc.robot1.peer-abc.ice.local"
        );
        assert_eq!(
            Subjects::webrtc_ice_remote("robot1", "peer-abc").unwrap(),
            "webrtc.robot1.peer-abc.ice.remote"
        );
    }

    #[test]
    fn webrtc_wildcard_subject() {
        assert_eq!(Subjects::webrtc_wildcard("robot1").unwrap(), "webrtc.robot1.>");
    }

    #[test]
    fn camera_event_subject() {
        assert_eq!(Subjects::camera_event("robot1").unwrap(), "camera.robot1.event");
    }

    #[test]
    fn camera_request_subject() {
        assert_eq!(Subjects::camera_request("robot1").unwrap(), "camera.robot1.request");
    }

    // -----------------------------------------------------------------------
    // Phase 26.5 SC5 R-02 — camera session-scoped subjects
    // -----------------------------------------------------------------------

    #[test]
    fn camera_session_wildcard_subject() {
        assert_eq!(
            Subjects::camera_session_wildcard("w1", "sess-abc").unwrap(),
            "camera.w1.sess-abc.*"
        );
    }

    #[test]
    fn camera_session_wildcard_rejects_bad_tokens() {
        assert!(Subjects::camera_session_wildcard("", "sess").is_err());
        assert!(Subjects::camera_session_wildcard("w1", "").is_err());
        assert!(Subjects::camera_session_wildcard("w1", "sess.abc").is_err());
        assert!(Subjects::camera_session_wildcard("w.1", "sess").is_err());
        assert!(Subjects::camera_session_wildcard("w*", "sess").is_err());
        assert!(Subjects::camera_session_wildcard("w1", "sess>").is_err());
    }

    // -----------------------------------------------------------------------
    // Phase 24 — FS-01 / FS-02 / FS-03 subjects (24-01-PLAN Task 2)
    // -----------------------------------------------------------------------

    #[test]
    fn policy_subject_builds() {
        assert_eq!(Subjects::policy("host1").unwrap(), "roz.policy.host1");
    }

    #[test]
    fn policy_subject_rejects_invalid() {
        assert!(Subjects::policy("").is_err());
        assert!(Subjects::policy("a.b").is_err());
        assert!(Subjects::policy("a*").is_err());
        assert!(Subjects::policy("a>").is_err());
    }

    #[test]
    fn health_subject_builds() {
        assert_eq!(Subjects::health("host1").unwrap(), "roz.health.host1");
    }

    #[test]
    fn health_subject_rejects_invalid() {
        assert!(Subjects::health("").is_err());
    }

    #[test]
    fn safety_violation_subject_builds() {
        assert_eq!(Subjects::safety_violation("host1").unwrap(), "safety.violation.host1");
    }

    #[test]
    fn safety_violation_subject_rejects_invalid() {
        assert!(Subjects::safety_violation("").is_err());
    }

    #[test]
    fn state_worker_online_subject_is_static() {
        assert_eq!(Subjects::state_worker_online(), "roz.state.worker_online");
    }

    #[test]
    fn clear_failsafe_subject_builds() {
        assert_eq!(Subjects::clear_failsafe("host1").unwrap(), "cmd.host1.clear_failsafe");
    }

    #[test]
    fn clear_failsafe_subject_rejects_invalid() {
        assert!(Subjects::clear_failsafe("").is_err());
        assert!(Subjects::clear_failsafe("a.b").is_err());
    }

    #[test]
    fn worker_tasks_subject_builds() {
        assert_eq!(Subjects::worker_tasks("host1").unwrap(), "roz.tasks.host1");
    }

    #[test]
    fn worker_tasks_subject_rejects_invalid() {
        assert!(Subjects::worker_tasks("").is_err());
        assert!(Subjects::worker_tasks("a.b").is_err());
        assert!(Subjects::worker_tasks("a*").is_err());
        assert!(Subjects::worker_tasks("a>").is_err());
    }
}

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
    pub fn estop(worker_id: &str) -> String {
        format!("safety.estop.{worker_id}")
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
        let subject = Subjects::estop("robot-arm-1");
        assert_eq!(subject, "safety.estop.robot-arm-1");
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
}

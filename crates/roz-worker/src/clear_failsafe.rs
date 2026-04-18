//! Signed server→worker re-arm subscriber (FS-01 D-02).
//!
//! Subscribes to `cmd.{worker_id}.clear_failsafe`, verifies each inbound
//! message via [`WorkerSigningContext::verify_inbound_worker`], and on
//! success calls [`CommandWatchdog::clear_failsafe`] to un-latch motion.
//!
//! The message body carries no per-call parameters today — the signed
//! envelope's `correlation_id` + (optional) operator-provided reason is
//! sufficient for audit. Future needs may extend [`ClearFailsafePayload`].

// RED placeholder — the GREEN commit ships the module body. Tests below
// reference symbols that do not exist yet; cargo build will fail.

#[cfg(test)]
mod tests {
    use crate::clear_failsafe::{ClearFailsafeError, ClearFailsafePayload, handle_clear_failsafe_message};
    use crate::command_watchdog::CommandWatchdog;
    use crate::signing_hooks::WorkerSigningContext;

    async fn _ref() {
        // symbols referenced so compile errors surface at RED:
        let _: fn(&WorkerSigningContext, &CommandWatchdog, &async_nats::Message)
        -> Result<ClearFailsafePayload, ClearFailsafeError> = handle_clear_failsafe_message;
    }
}

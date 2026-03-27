//! Test implementations of [`ActuatorSink`] and [`SensorSource`].

use std::sync::Arc;

use parking_lot::Mutex;
use roz_core::command::CommandFrame;

use crate::io::{ActuatorSink, SensorFrame, SensorSource};

// ---------------------------------------------------------------------------
// LogActuatorSink
// ---------------------------------------------------------------------------

/// Captures all command frames for test assertions.
pub struct LogActuatorSink {
    commands: Arc<Mutex<Vec<CommandFrame>>>,
}

impl LogActuatorSink {
    /// Create a new, empty log sink.
    pub fn new() -> Self {
        Self {
            commands: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Return a snapshot of all command frames received so far.
    pub fn commands(&self) -> Vec<CommandFrame> {
        self.commands.lock().clone()
    }
}

impl Default for LogActuatorSink {
    fn default() -> Self {
        Self::new()
    }
}

impl ActuatorSink for LogActuatorSink {
    fn send(&self, frame: &CommandFrame) -> anyhow::Result<()> {
        self.commands.lock().push(frame.clone());
        Ok(())
    }
}

impl ActuatorSink for Arc<LogActuatorSink> {
    fn send(&self, frame: &CommandFrame) -> anyhow::Result<()> {
        self.as_ref().send(frame)
    }
}

// ---------------------------------------------------------------------------
// TeeActuatorSink
// ---------------------------------------------------------------------------

/// Sends each frame to two sinks — use for testing with real IO + logging.
pub struct TeeActuatorSink {
    primary: Arc<dyn ActuatorSink>,
    secondary: Arc<dyn ActuatorSink>,
}

impl TeeActuatorSink {
    pub fn new(primary: Arc<dyn ActuatorSink>, secondary: Arc<dyn ActuatorSink>) -> Self {
        Self { primary, secondary }
    }
}

impl ActuatorSink for TeeActuatorSink {
    fn send(&self, frame: &CommandFrame) -> anyhow::Result<()> {
        self.primary.send(frame)?;
        self.secondary.send(frame)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// MockSensorSource
// ---------------------------------------------------------------------------

/// Returns pre-configured sensor data once, then None.
pub struct MockSensorSource {
    frame: Option<SensorFrame>,
}

impl MockSensorSource {
    /// Create a source that yields `frame` on the first call, then None.
    pub const fn new(frame: SensorFrame) -> Self {
        Self { frame: Some(frame) }
    }

    /// Create a source that always returns None.
    pub const fn empty() -> Self {
        Self { frame: None }
    }
}

impl SensorSource for MockSensorSource {
    fn try_recv(&mut self) -> Option<SensorFrame> {
        self.frame.take()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use roz_core::command::CommandFrame;

    use super::*;

    #[test]
    fn log_sink_captures_commands() {
        let sink = LogActuatorSink::new();
        let frame = CommandFrame::zero(3);
        sink.send(&frame).unwrap();
        let captured = sink.commands();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].values, vec![0.0, 0.0, 0.0]);
    }

    #[test]
    fn arc_log_sink_captures_commands() {
        let sink = Arc::new(LogActuatorSink::new());
        let frame = CommandFrame::zero(3);
        sink.send(&frame).unwrap();
        let captured = sink.commands();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].values, vec![0.0, 0.0, 0.0]);
    }

    #[test]
    fn mock_source_returns_data_once() {
        let frame = SensorFrame {
            sim_time_ns: 42,
            ..SensorFrame::default()
        };
        let mut source = MockSensorSource::new(frame);
        let first = source.try_recv();
        assert!(first.is_some());
        assert_eq!(first.unwrap().sim_time_ns, 42);
        let second = source.try_recv();
        assert!(second.is_none());
    }

    #[test]
    fn mock_source_empty_returns_none() {
        let mut source = MockSensorSource::empty();
        assert!(source.try_recv().is_none());
        assert!(source.try_recv().is_none());
    }
}

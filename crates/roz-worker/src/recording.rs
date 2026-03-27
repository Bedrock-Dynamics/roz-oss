use std::collections::{BTreeMap, HashMap};
use std::io::BufWriter;
use std::path::Path;

/// Records telemetry streams to MCAP format during task execution.
///
/// MCAP is a self-describing container for multimodal log data, commonly
/// used in robotics for recording sensor data, commands, and state.
pub struct McapRecorder {
    writer: mcap::Writer<BufWriter<std::fs::File>>,
    channels: HashMap<String, u16>,
    sequence: u32,
}

impl McapRecorder {
    /// Create a new MCAP recorder writing to the given file path.
    pub fn new(path: &Path) -> anyhow::Result<Self> {
        let file = std::fs::File::create(path)?;
        let writer = mcap::Writer::new(BufWriter::new(file))?;
        Ok(Self {
            writer,
            channels: HashMap::new(),
            sequence: 0,
        })
    }

    /// Record a message on the given topic.
    ///
    /// Channels are auto-created on first use for each unique topic.
    pub fn record(&mut self, topic: &str, timestamp_ns: u64, data: &[u8]) -> anyhow::Result<()> {
        let channel_id = self.ensure_channel(topic)?;
        self.writer.write_to_known_channel(
            &mcap::records::MessageHeader {
                channel_id,
                sequence: self.sequence,
                log_time: timestamp_ns,
                publish_time: timestamp_ns,
            },
            data,
        )?;
        self.sequence += 1;
        Ok(())
    }

    /// Finalize the MCAP file (writes summary and footer).
    pub fn finish(mut self) -> anyhow::Result<()> {
        self.writer.finish()?;
        Ok(())
    }

    fn ensure_channel(&mut self, topic: &str) -> anyhow::Result<u16> {
        if let Some(&id) = self.channels.get(topic) {
            return Ok(id);
        }

        let schema_id = self.writer.add_schema(topic, "raw", &[])?;

        let channel_id = self.writer.add_channel(schema_id, topic, "raw", &BTreeMap::new())?;
        self.channels.insert(topic.to_string(), channel_id);
        Ok(channel_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn create_and_finish_empty_recording() {
        let tmp = NamedTempFile::new().unwrap();
        let recorder = McapRecorder::new(tmp.path()).unwrap();
        recorder.finish().unwrap();
        // File should be valid MCAP (non-zero size)
        assert!(tmp.path().metadata().unwrap().len() > 0);
    }

    #[test]
    fn record_and_read_back() {
        let tmp = NamedTempFile::new().unwrap();
        let mut recorder = McapRecorder::new(tmp.path()).unwrap();
        recorder.record("imu/accel", 1_000_000_000, b"hello").unwrap();
        recorder.record("imu/accel", 2_000_000_000, b"world").unwrap();
        recorder.record("gps/fix", 3_000_000_000, b"position").unwrap();
        recorder.finish().unwrap();

        // Read back with mcap crate
        let data = std::fs::read(tmp.path()).unwrap();
        let messages: Vec<_> = mcap::MessageStream::new(&data)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(messages.len(), 3);
    }

    #[test]
    fn channel_auto_creation() {
        let tmp = NamedTempFile::new().unwrap();
        let mut recorder = McapRecorder::new(tmp.path()).unwrap();
        recorder.record("topic_a", 100, b"a").unwrap();
        recorder.record("topic_b", 200, b"b").unwrap();
        recorder.record("topic_a", 300, b"a2").unwrap(); // reuses channel
        recorder.finish().unwrap();

        let data = std::fs::read(tmp.path()).unwrap();
        let messages: Vec<_> = mcap::MessageStream::new(&data)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(messages.len(), 3);
    }
}

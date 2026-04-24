//! Phase 26 OBS-03: MCAP export streaming helpers.
//!
//! Path-safety (`canonicalize` + `starts_with(ROZ_MCAP_DIR)`) is enforced at
//! the gRPC handler entry; this module trusts its caller on path cleanliness
//! and focuses on two tasks:
//!
//! * `stream_file_raw` — chunked file read into an mpsc sender. No re-encoding,
//!   used when no time-range filter is requested.
//! * `filter_by_time_range` — re-encode matching messages via
//!   `mcap::MessageStream` into a fresh in-memory MCAP. Chunks outside
//!   `[start_ns, end_ns)` are dropped.

use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::Path;

use mcap::Writer;
use mcap::records::MessageHeader;
use tokio::io::AsyncReadExt as _;
use tokio::sync::mpsc;

use crate::observability::McapArchiveError;

/// Chunk size for MCAP export streaming.
///
/// Used by both `stream_file_raw` (raw file chunks) and for slicing the
/// filtered output of `filter_by_time_range` before emission on the wire.
/// 256 KiB balances per-message gRPC overhead against memory pressure on
/// high-throughput clients.
pub const EXPORT_CHUNK_BYTES: usize = 256 * 1024;

/// Raw file-byte stream. No time filtering; fastest path.
///
/// Reads `path` into 256 KiB chunks and sends each to `tx`. Returns `Ok(())`
/// once EOF is reached or the receiver has been dropped.
///
/// # Errors
/// * `McapArchiveError::Io` — the underlying `tokio::fs::File` open or read failed.
pub async fn stream_file_raw(
    path: &Path,
    tx: &mpsc::Sender<Result<Vec<u8>, McapArchiveError>>,
) -> Result<(), McapArchiveError> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut buf = vec![0u8; EXPORT_CHUNK_BYTES];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        if tx.send(Ok(buf[..n].to_vec())).await.is_err() {
            break; // client disconnected
        }
    }
    Ok(())
}

/// Time-range filter. Re-encodes matching messages into a fresh MCAP byte
/// buffer via `mcap::MessageStream` + a new `mcap::Writer`. Chunks outside
/// `[start_ns, end_ns)` are dropped.
///
/// Bounds semantics: `start_ns` inclusive, `end_ns` exclusive. Either may be
/// `None` (open start / open end).
///
/// # Errors
/// * `McapArchiveError::McapWrite` — `add_schema`, `add_channel`, or
///   `write_to_known_channel` / `finish` rejected a record.
/// * `McapArchiveError::Io` — internal cursor write error (should not occur
///   for an in-memory buffer).
pub fn filter_by_time_range(
    data: &[u8],
    start_ns: Option<u64>,
    end_ns: Option<u64>,
) -> Result<Vec<u8>, McapArchiveError> {
    let mut out = Vec::with_capacity(data.len());
    {
        let mut writer = Writer::new(Cursor::new(&mut out))?;

        // Map from input schema/channel ids to the ids returned by the
        // fresh writer. Needed because `add_schema` / `add_channel` allocate
        // sequentially; the input and output id spaces may differ (e.g., if
        // messages for a channel are filtered away we skip registering it).
        let mut schema_map: BTreeMap<u16, u16> = BTreeMap::new();
        let mut channel_map: BTreeMap<u16, u16> = BTreeMap::new();
        let mut sequence: u32 = 0;

        for msg in mcap::MessageStream::new(data)? {
            let msg = msg?;
            if let Some(s) = start_ns
                && msg.log_time < s
            {
                continue;
            }
            if let Some(e) = end_ns
                && msg.log_time >= e
            {
                continue;
            }

            let schema_id = match msg.channel.schema.as_ref() {
                Some(schema) => {
                    if let Some(&id) = schema_map.get(&schema.id) {
                        id
                    } else {
                        let new_id = writer.add_schema(&schema.name, &schema.encoding, schema.data.as_ref())?;
                        schema_map.insert(schema.id, new_id);
                        new_id
                    }
                }
                None => 0,
            };

            let channel_id = if let Some(&id) = channel_map.get(&msg.channel.id) {
                id
            } else {
                let new_id = writer.add_channel(
                    schema_id,
                    &msg.channel.topic,
                    &msg.channel.message_encoding,
                    &msg.channel.metadata,
                )?;
                channel_map.insert(msg.channel.id, new_id);
                new_id
            };

            let header = MessageHeader {
                channel_id,
                sequence,
                log_time: msg.log_time,
                publish_time: msg.publish_time,
            };
            writer.write_to_known_channel(&header, msg.data.as_ref())?;
            sequence = sequence.wrapping_add(1);
        }
        writer.finish()?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an empty but valid MCAP in memory. Used as the input fixture
    /// for the zero-message filter test.
    fn empty_mcap_bytes() -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = mcap::Writer::new(Cursor::new(&mut buf)).expect("writer");
            w.finish().expect("finish");
        }
        buf
    }

    #[test]
    fn filter_by_time_range_empty_input_returns_valid_empty_mcap() {
        let input = empty_mcap_bytes();
        let out = filter_by_time_range(&input, Some(0), Some(1_000)).expect("filter");
        assert!(!out.is_empty(), "filter should emit a valid (empty) mcap file");
        let mut count = 0u64;
        for msg in mcap::MessageStream::new(&out).expect("open filtered mcap") {
            let _ = msg.expect("decode message");
            count += 1;
        }
        assert_eq!(count, 0, "empty input must yield zero messages");
    }

    #[test]
    fn filter_by_time_range_open_bounds_round_trip_empty() {
        let input = empty_mcap_bytes();
        let out = filter_by_time_range(&input, None, None).expect("filter");
        assert!(!out.is_empty());
        let mut count = 0u64;
        for msg in mcap::MessageStream::new(&out).expect("open filtered mcap") {
            let _ = msg.expect("decode message");
            count += 1;
        }
        assert_eq!(count, 0);
    }

    /// Builds a fixture MCAP containing four messages on a single channel at
    /// `log_time` = 100, 200, 300, 400. Returns the raw MCAP bytes.
    fn fixture_with_messages(times: &[u64]) -> Vec<u8> {
        use mcap::records::MessageHeader;
        let mut buf = Vec::new();
        {
            let mut w = mcap::Writer::new(Cursor::new(&mut buf)).expect("writer");
            let schema_id = w
                .add_schema("test.Msg", "protobuf", b"\x00\x01\x02")
                .expect("add_schema");
            let chan_id = w
                .add_channel(schema_id, "/test", "protobuf", &BTreeMap::new())
                .expect("add_channel");
            for (seq, &t) in times.iter().enumerate() {
                let header = MessageHeader {
                    channel_id: chan_id,
                    sequence: u32::try_from(seq).expect("sequence fits u32"),
                    log_time: t,
                    publish_time: t,
                };
                w.write_to_known_channel(&header, b"payload").expect("write");
            }
            w.finish().expect("finish");
        }
        buf
    }

    #[test]
    fn filter_by_time_range_drops_out_of_range_messages() {
        let input = fixture_with_messages(&[100, 200, 300, 400]);
        // Keep only 200 <= log_time < 400 → 200 and 300.
        let out = filter_by_time_range(&input, Some(200), Some(400)).expect("filter");
        let times: Vec<u64> = mcap::MessageStream::new(&out)
            .expect("open filtered")
            .map(|m| m.expect("decode").log_time)
            .collect();
        assert_eq!(times, vec![200, 300]);
    }

    #[test]
    fn filter_by_time_range_start_only_keeps_tail() {
        let input = fixture_with_messages(&[100, 200, 300, 400]);
        let out = filter_by_time_range(&input, Some(300), None).expect("filter");
        let times: Vec<u64> = mcap::MessageStream::new(&out)
            .expect("open filtered")
            .map(|m| m.expect("decode").log_time)
            .collect();
        assert_eq!(times, vec![300, 400]);
    }

    #[test]
    fn filter_by_time_range_end_only_keeps_head() {
        let input = fixture_with_messages(&[100, 200, 300, 400]);
        let out = filter_by_time_range(&input, None, Some(300)).expect("filter");
        let times: Vec<u64> = mcap::MessageStream::new(&out)
            .expect("open filtered")
            .map(|m| m.expect("decode").log_time)
            .collect();
        assert_eq!(times, vec![100, 200]);
    }

    #[tokio::test]
    async fn stream_file_raw_chunks_file_contents() {
        use tokio::io::AsyncWriteExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sample.bin");
        let mut file = tokio::fs::File::create(&path).await.expect("create");
        let payload = vec![0xAB_u8; EXPORT_CHUNK_BYTES + 1024];
        file.write_all(&payload).await.expect("write");
        file.flush().await.expect("flush");
        drop(file);

        let (tx, mut rx) = mpsc::channel::<Result<Vec<u8>, McapArchiveError>>(4);
        let path_clone = path.clone();
        let handle = tokio::spawn(async move {
            let res = stream_file_raw(&path_clone, &tx).await;
            drop(tx);
            res
        });

        let mut gathered = Vec::new();
        while let Some(chunk) = rx.recv().await {
            gathered.extend_from_slice(&chunk.expect("io ok"));
        }
        handle.await.expect("join").expect("stream ok");
        assert_eq!(gathered, payload, "round-trip bytes must match input");
    }
}

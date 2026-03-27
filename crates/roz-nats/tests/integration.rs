use std::time::Duration;

use roz_nats::provisioning::TenantStreams;
use roz_nats::subjects::Subjects;

/// Proves that `TenantStreams` subject filters correctly capture messages
/// published to `Subjects`-built subjects. Validates the contract between
/// `Subjects` (message producers) and `TenantStreams` (JetStream consumers).
///
/// **Must run with `--test-threads=1`** to avoid multiple NATS containers.
#[tokio::test]
async fn tenant_streams_capture_subject_messages() {
    let guard = roz_test::nats_container().await;
    let client = async_nats::connect(guard.url()).await.expect("connect");
    let jetstream = async_nats::jetstream::new(client.clone());

    let streams = TenantStreams::for_tenant("tenant-test-1");

    // Create the telemetry stream using production stream name + filter.
    let mut stream = jetstream
        .create_stream(async_nats::jetstream::stream::Config {
            name: streams.telemetry.clone(),
            subjects: vec![Subjects::all_telemetry()],
            ..Default::default()
        })
        .await
        .expect("create stream with TenantStreams name");

    // Publish to a Subjects-built telemetry subject — should be captured.
    let subject = Subjects::telemetry("host-42", "imu").expect("valid subject");
    jetstream
        .publish(subject, b"telemetry-payload".to_vec().into())
        .await
        .expect("publish")
        .await
        .expect("ack");

    let info = stream.info().await.expect("stream info");
    assert_eq!(
        info.state.messages, 1,
        "TenantStreams filter must capture Subjects telemetry messages"
    );

    // Publish to a command subject via core NATS — should NOT be captured by the
    // telemetry stream (no JetStream ack needed since no stream matches).
    let cmd_subject = Subjects::command("host-42", "arm").expect("valid subject");
    client
        .publish(cmd_subject, b"cmd-payload".to_vec().into())
        .await
        .expect("publish cmd");
    client.flush().await.expect("flush");

    let info = stream.info().await.expect("stream info after cmd");
    assert_eq!(
        info.state.messages, 1,
        "telemetry stream must not capture command subjects"
    );
}

/// Proves that `Subjects::telemetry_wildcard()` generates a pattern that NATS
/// matches against `Subjects::telemetry()`-built subjects for the same host,
/// while rejecting subjects from other hosts.
///
/// **Must run with `--test-threads=1`** to avoid multiple NATS containers.
#[tokio::test]
async fn wildcard_subscription_matches_specific_subjects() {
    let guard = roz_test::nats_container().await;
    let client = async_nats::connect(guard.url()).await.expect("connect");

    // Subscribe using the production wildcard builder.
    let wildcard = Subjects::telemetry_wildcard("host-1").expect("valid");
    let mut sub = client.subscribe(wildcard).await.expect("subscribe");

    // Publish to two different sensors on the same host.
    let imu_subject = Subjects::telemetry("host-1", "imu").expect("valid");
    let gps_subject = Subjects::telemetry("host-1", "gps").expect("valid");
    // Publish to a different host — should NOT match.
    let other_host = Subjects::telemetry("host-2", "imu").expect("valid");

    client
        .publish(imu_subject, b"imu-data".to_vec().into())
        .await
        .expect("pub imu");
    client
        .publish(gps_subject, b"gps-data".to_vec().into())
        .await
        .expect("pub gps");
    client
        .publish(other_host, b"other-host".to_vec().into())
        .await
        .expect("pub other");
    client.flush().await.expect("flush");

    // Should receive exactly the two host-1 messages.
    let msg1 = tokio::time::timeout(Duration::from_secs(2), futures::StreamExt::next(&mut sub))
        .await
        .expect("timeout msg1")
        .expect("sub closed");
    let msg2 = tokio::time::timeout(Duration::from_secs(2), futures::StreamExt::next(&mut sub))
        .await
        .expect("timeout msg2")
        .expect("sub closed");

    let payloads: Vec<&[u8]> = vec![&msg1.payload, &msg2.payload];
    assert!(payloads.contains(&&b"imu-data"[..]), "should receive imu-data");
    assert!(payloads.contains(&&b"gps-data"[..]), "should receive gps-data");

    // Third message (host-2) must NOT arrive.
    let timeout_result = tokio::time::timeout(Duration::from_millis(200), futures::StreamExt::next(&mut sub)).await;
    assert!(timeout_result.is_err(), "host-2 message must not match host-1 wildcard");
}

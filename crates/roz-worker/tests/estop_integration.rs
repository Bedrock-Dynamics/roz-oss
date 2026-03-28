use std::time::Duration;

/// Verifies that the estop listener flips the watch channel to `true`
/// when an e-stop message is received on the correct NATS subject.
#[tokio::test]
async fn estop_listener_flips_on_nats_message() {
    let guard = roz_test::nats_container().await;
    let client = async_nats::connect(guard.url()).await.expect("connect to NATS");

    // Subscribe to the estop subject for this worker.
    let sub = roz_worker::estop::subscribe_estop(&client, "test-worker")
        .await
        .expect("subscribe estop");

    // Spawn the listener and get the watch receiver.
    let mut rx = roz_worker::estop::spawn_estop_listener(sub);

    // Initially, estop should not be triggered.
    assert!(!*rx.borrow(), "estop should start as false (not triggered)");

    // Publish an estop message.
    client
        .publish("safety.estop.test-worker", bytes::Bytes::from_static(b"{}"))
        .await
        .expect("publish estop");
    client.flush().await.expect("flush");

    // Wait for the watch to change within 5 seconds.
    tokio::time::timeout(Duration::from_secs(5), rx.changed())
        .await
        .expect("timed out waiting for estop signal")
        .expect("watch channel closed unexpectedly");

    assert!(*rx.borrow(), "estop should be true after receiving message");
}

/// Verifies that the estop listener treats a NATS client drain
/// (subscription close) as an e-stop, following the fail-safe design.
///
/// `Client::drain()` gracefully shuts down all subscriptions, causing
/// the subscription stream to return `None` — the same signal as a
/// connection loss. The estop listener must treat this as an e-stop.
#[tokio::test]
async fn estop_listener_triggers_on_nats_disconnect() {
    let guard = roz_test::nats_container().await;
    let client = async_nats::connect(guard.url()).await.expect("connect to NATS");

    // Subscribe to the estop subject.
    let sub = roz_worker::estop::subscribe_estop(&client, "disconnect-worker")
        .await
        .expect("subscribe estop");

    // Spawn the listener.
    let mut rx = roz_worker::estop::spawn_estop_listener(sub);

    // Initially not triggered.
    assert!(!*rx.borrow(), "estop should start as false");

    // Drain the NATS client — this closes all subscriptions gracefully,
    // simulating the behavior the worker sees when NATS becomes unreachable.
    client.drain().await.expect("drain NATS client");

    // Wait for the watch to change within 5 seconds.
    tokio::time::timeout(Duration::from_secs(5), rx.changed())
        .await
        .expect("timed out waiting for estop signal on disconnect")
        .expect("watch channel closed unexpectedly");

    assert!(*rx.borrow(), "estop should be true after NATS disconnect (fail-safe)");

    // Keep guard alive until test completes to avoid container cleanup races.
    drop(guard);
}

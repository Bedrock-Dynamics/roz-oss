use std::sync::Arc;
use std::time::Duration;

/// Verifies that the estop listener flips the watch channel to `true`
/// when an e-stop message is received on the correct NATS subject.
#[tokio::test]
#[ignore = "requires Docker for NATS testcontainer"]
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
#[ignore = "requires Docker for NATS testcontainer"]
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

/// Proves that when an e-stop fires during long-running work, the
/// `tokio::select!` pattern (used in `session_relay.rs` and `main.rs`)
/// actually interrupts execution instead of waiting for the work to complete.
///
/// A simulated "agent turn" sleeps for 10 seconds. An e-stop fires after
/// 500ms. If the select! pattern works, the test completes in ~500ms,
/// not 10 seconds.
#[tokio::test]
async fn estop_interrupts_agent_run_mid_execution() {
    let (estop_tx, mut estop_rx) = tokio::sync::watch::channel(false);

    let start = std::time::Instant::now();

    // Fire e-stop after 500ms (simulates external safety signal).
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        estop_tx.send(true).expect("send estop");
    });

    // The "agent work" that would normally take 10 seconds.
    let long_running_work = async {
        tokio::time::sleep(Duration::from_secs(10)).await;
        "completed normally"
    };

    let result = tokio::select! {
        output = long_running_work => {
            panic!("agent should NOT complete — estop should have fired: {output}");
        }
        () = async { estop_rx.changed().await.expect("watch closed"); } => {
            if *estop_rx.borrow() {
                "estop_fired"
            } else {
                panic!("estop_rx changed but value is false");
            }
        }
    };

    let elapsed = start.elapsed();

    assert_eq!(result, "estop_fired");
    assert!(
        elapsed < Duration::from_secs(2),
        "estop should interrupt within 2s, took {elapsed:?}",
    );
    assert!(
        elapsed >= Duration::from_millis(400),
        "estop should not fire before ~500ms, fired at {elapsed:?}",
    );
}

/// Proves that `CommandWatchdog::run` actually cancels the token when the
/// deadline expires without a `pet()` call.
#[tokio::test]
async fn command_watchdog_fires_and_cancels_token() {
    // Deadline is 1ms — the watchdog loop ticks every 1s, so this will
    // expire on the first tick.
    let watchdog = Arc::new(roz_worker::command_watchdog::CommandWatchdog::new(
        Duration::from_millis(1),
    ));
    let cancel = tokio_util::sync::CancellationToken::new();

    let wd = Arc::clone(&watchdog);
    let wd_cancel = cancel.clone();
    tokio::spawn(async move { wd.run(wd_cancel).await });

    // The watchdog ticks every 1s, so it should notice the expired deadline
    // on the first tick and cancel within ~2s.
    let result = tokio::time::timeout(Duration::from_secs(5), cancel.cancelled()).await;

    assert!(result.is_ok(), "watchdog should have cancelled within 5s");
}

/// Proves that calling `pet()` keeps the watchdog alive, and that it
/// only fires once petting stops.
#[tokio::test]
async fn command_watchdog_pet_prevents_firing() {
    // 3-second deadline. The watchdog ticks every 1s.
    let watchdog = Arc::new(roz_worker::command_watchdog::CommandWatchdog::new(Duration::from_secs(
        3,
    )));
    let cancel = tokio_util::sync::CancellationToken::new();

    let wd = Arc::clone(&watchdog);
    let wd_cancel = cancel.clone();
    tokio::spawn(async move { wd.run(wd_cancel).await });

    // Pet every 1s for 5s — each pet resets the 3s deadline, so the
    // watchdog should never fire during this window.
    for _ in 0..5 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        watchdog.pet();
    }

    assert!(
        !cancel.is_cancelled(),
        "watchdog should NOT have fired — was being petted"
    );

    // Now stop petting. The 3s deadline will expire on the next tick.
    let result = tokio::time::timeout(Duration::from_secs(10), cancel.cancelled()).await;

    assert!(result.is_ok(), "watchdog should fire after petting stops");
}

/// Full vertical integration: e-stop published to NATS reaches the watch
/// channel and interrupts a `tokio::select!` loop — the same pattern used
/// in the worker's production code.
#[tokio::test]
#[ignore = "requires Docker for NATS testcontainer"]
async fn estop_via_nats_interrupts_select_pattern() {
    let guard = roz_test::nats_container().await;
    let client = async_nats::connect(guard.url()).await.expect("connect");

    // Set up estop listener (same as worker main.rs).
    let sub = roz_worker::estop::subscribe_estop(&client, "select-test-worker")
        .await
        .expect("subscribe estop");
    let estop_rx = roz_worker::estop::spawn_estop_listener(sub);

    let start = std::time::Instant::now();

    // Publish e-stop after 500ms.
    let pub_client = client.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let subject = roz_nats::subjects::Subjects::estop("select-test-worker").expect("valid");
        pub_client
            .publish(subject, bytes::Bytes::from_static(b"{}"))
            .await
            .expect("publish");
        pub_client.flush().await.expect("flush");
    });

    // Simulate long-running agent work with select! — same pattern as production.
    let mut rx = estop_rx.clone();
    let result = tokio::select! {
        () = tokio::time::sleep(Duration::from_secs(10)) => "work_completed",
        () = async { rx.changed().await.expect("watch closed"); } => {
            if *rx.borrow() { "estop_fired" } else { "spurious" }
        }
    };

    let elapsed = start.elapsed();
    assert_eq!(result, "estop_fired");
    assert!(
        elapsed < Duration::from_secs(2),
        "should interrupt within 2s, took {elapsed:?}"
    );
}

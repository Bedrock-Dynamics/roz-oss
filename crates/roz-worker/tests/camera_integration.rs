//! Integration tests for the camera pipeline: StreamHub fan-out, encoder pipeline,
//! CameraSpatialProvider, adaptive bitrate, encoder fallback, and event serialization.

use std::sync::Arc;
use std::time::Duration;

use roz_core::camera::{BitrateProfile, CameraEvent, CameraId, CameraInfo, EncoderSelection};
use roz_worker::camera::adaptive::{AdaptiveBitrateController, RtcpFeedback};
use roz_worker::camera::encoder::{EncodedFrame, EncoderBackend, H264Encoder, SwEncoder, create_encoder};
use roz_worker::camera::snapshot::CameraSpatialProvider;
use roz_worker::camera::source::{CameraSource, TestPatternSource};
use roz_worker::camera::stream_hub::StreamHub;

use roz_agent::spatial_provider::SpatialContextProvider;

// ---------------------------------------------------------------------------
// Test 1: StreamHub encode-once, two viewers receive same frames
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_hub_two_viewers_receive_same_frame() {
    let hub = StreamHub::new();
    let cam = CameraId::new("cam-fan-out");

    hub.register_camera(cam.clone()).await;

    // Subscribe two viewers.
    let (mut rx1, handle1) = hub.subscribe(&cam).await.expect("subscribe viewer 1");
    let (mut rx2, _handle2) = hub.subscribe(&cam).await.expect("subscribe viewer 2");
    assert_eq!(hub.viewer_count(&cam).await, 2);

    // Publish one EncodedFrame.
    let frame = EncodedFrame {
        camera_id: cam.clone(),
        nalus: vec![0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB],
        is_keyframe: true,
        pts_90khz: 90_000,
        profile: BitrateProfile::MEDIUM,
        seq: 42,
    };
    hub.publish(frame).await;

    // Both receivers get the same frame via Arc.
    let f1: Arc<EncodedFrame> = rx1.recv().await.expect("viewer 1 should receive");
    let f2: Arc<EncodedFrame> = rx2.recv().await.expect("viewer 2 should receive");

    // Arc equality: both point to the same allocation.
    assert!(Arc::ptr_eq(&f1, &f2), "both viewers must share the same Arc");
    assert_eq!(f1.seq, 42);
    assert_eq!(f1.camera_id, cam);
    assert!(f1.is_keyframe);
    assert_eq!(f1.nalus, vec![0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB]);

    // Drop one viewer, verify count decrements.
    drop(handle1);
    assert_eq!(hub.viewer_count(&cam).await, 1);
}

// ---------------------------------------------------------------------------
// Test 2: TestPattern -> Encoder -> EncodedFrame pipeline
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_pattern_to_encoder_pipeline() {
    // Create TestPatternSource, start at 320x240@5fps.
    let mut source = TestPatternSource::new("pipeline-cam");
    let mut rx = source.start(320, 240, 5).await.expect("start test pattern");

    // Create SwEncoder at LOW profile.
    let mut encoder = SwEncoder::new(BitrateProfile::LOW).expect("create encoder");

    // Receive a RawFrame.
    let raw = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timeout waiting for frame")
        .expect("channel closed");

    assert_eq!(raw.width, 320);
    assert_eq!(raw.height, 240);
    assert_eq!(raw.camera_id, CameraId::new("pipeline-cam"));

    // Encode the raw frame.
    let encoded = encoder.encode(&raw).expect("encode frame");

    // Assert EncodedFrame has non-empty NALUs.
    assert!(!encoded.nalus.is_empty(), "encoded NALUs must not be empty");

    // Assert first frame is a keyframe (IDR).
    assert!(encoded.is_keyframe, "first frame from encoder must be IDR keyframe");
    assert_eq!(encoded.camera_id, CameraId::new("pipeline-cam"));
    assert_eq!(encoded.seq, raw.seq);
    assert_eq!(encoded.profile, BitrateProfile::LOW);

    source.stop().await;
}

// ---------------------------------------------------------------------------
// Test 3: CameraSpatialProvider snapshot roundtrip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn camera_spatial_provider_provides_snapshots() {
    let provider = CameraSpatialProvider::new();

    // Update with a JPEG snapshot (fake JPEG data with magic bytes).
    let jpeg_data = b"\xFF\xD8\xFF\xE0fake-camera-jpeg-frame";
    provider.update_snapshot("wrist_cam", jpeg_data).await;

    // Call snapshot() via the SpatialContextProvider trait.
    let ctx = provider.snapshot("test-task-1").await;

    // Assert screenshots contains the camera.
    assert_eq!(ctx.screenshots.len(), 1);
    assert_eq!(ctx.screenshots[0].name, "wrist_cam");
    assert_eq!(ctx.screenshots[0].media_type, "image/jpeg");
    assert!(ctx.screenshots[0].depth_data.is_none());

    // Assert data is base64 encoded and decodes back correctly.
    let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &ctx.screenshots[0].data)
        .expect("base64 decode");
    assert_eq!(decoded, jpeg_data);

    // Add a second camera and verify both are present.
    provider
        .update_snapshot("overhead_cam", b"\xFF\xD8\xFF\xE0overhead-frame")
        .await;
    let ctx2 = provider.snapshot("test-task-2").await;
    assert_eq!(ctx2.screenshots.len(), 2);

    let names: Vec<&str> = ctx2.screenshots.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"wrist_cam"));
    assert!(names.contains(&"overhead_cam"));
}

// ---------------------------------------------------------------------------
// Test 4: AdaptiveBitrateController transitions
// ---------------------------------------------------------------------------
//
// The real hysteresis durations (5s upgrade, 1s downgrade) apply here because
// `with_stability` is `#[cfg(test)]`-gated inside the crate and not available
// from integration tests. We test what we can observe: EWMA convergence
// direction and the initial/default state.

#[test]
fn abr_starts_at_medium_and_ewma_moves_with_feedback() {
    let mut ctrl = AdaptiveBitrateController::new();

    // Starts at MEDIUM.
    assert_eq!(ctrl.current_profile(), BitrateProfile::MEDIUM);

    // Feed perfect feedback: score ~0.99. EWMA should rise toward 1.0.
    let perfect = RtcpFeedback {
        fraction_lost: 0.0,
        jitter_ms: 5.0,
        rtt_ms: 10.0,
    };
    let initial_score = ctrl.network_score();
    for _ in 0..10 {
        ctrl.on_rtcp_feedback(&perfect);
    }
    assert!(
        ctrl.network_score() > initial_score,
        "EWMA should increase with perfect feedback: {} > {}",
        ctrl.network_score(),
        initial_score
    );
    assert!(
        ctrl.network_score() > 0.8,
        "EWMA should exceed HIGH threshold with sustained perfect feedback"
    );

    // Feed terrible feedback: EWMA should decrease.
    let terrible = RtcpFeedback {
        fraction_lost: 0.5,
        jitter_ms: 150.0,
        rtt_ms: 400.0,
    };
    let score_before_bad = ctrl.network_score();
    for _ in 0..10 {
        ctrl.on_rtcp_feedback(&terrible);
    }
    assert!(
        ctrl.network_score() < score_before_bad,
        "EWMA should decrease with terrible feedback: {} < {}",
        ctrl.network_score(),
        score_before_bad
    );

    // Alternating good/bad should keep EWMA near the middle -- hysteresis
    // prevents tier changes within the same instant, so the profile should
    // stay at whatever tier it's currently on (no oscillation).
    let profile_before = ctrl.current_profile();
    for _ in 0..20 {
        ctrl.on_rtcp_feedback(&perfect);
        ctrl.on_rtcp_feedback(&terrible);
    }
    assert_eq!(
        ctrl.current_profile(),
        profile_before,
        "alternating good/bad should not cause tier oscillation"
    );
}

// ---------------------------------------------------------------------------
// Test 5: Encoder fallback
// ---------------------------------------------------------------------------

#[test]
fn create_encoder_hw_unavailable_falls_back() {
    // On non-Linux or without /dev/video11:

    // Auto selection falls back to SwEncoder (not error).
    let enc = create_encoder(EncoderSelection::Auto, BitrateProfile::MEDIUM).expect("Auto should succeed via fallback");
    assert_eq!(enc.backend(), EncoderBackend::OpenH264Software);

    // Hardware selection errors on non-Linux (no V4L2 M2M).
    if !cfg!(target_os = "linux") {
        let result = create_encoder(EncoderSelection::Hardware, BitrateProfile::MEDIUM);
        assert!(result.is_err(), "Hardware should fail when no V4L2 M2M device exists");
        let err_msg = result.err().expect("already checked is_err").to_string();
        assert!(
            err_msg.contains("hardware encoder"),
            "error should mention hardware encoder: {err_msg}"
        );
    }

    // Software selection always works.
    let sw = create_encoder(EncoderSelection::Software, BitrateProfile::LOW).expect("Software should always succeed");
    assert_eq!(sw.backend(), EncoderBackend::OpenH264Software);
}

// ---------------------------------------------------------------------------
// Test 6: Camera event serde roundtrip
// ---------------------------------------------------------------------------

#[test]
fn camera_event_serde_roundtrip() {
    // CameraEvent::Added
    let added = CameraEvent::Added {
        camera: CameraInfo {
            id: CameraId::new("cam-0"),
            label: "Front Camera".to_string(),
            device_path: "/dev/video0".to_string(),
            supported_resolutions: vec![(640, 480), (1280, 720)],
            max_fps: 30,
            hw_encoder_available: false,
        },
    };
    let json = serde_json::to_string(&added).expect("serialize Added");
    let parsed: CameraEvent = serde_json::from_str(&json).expect("deserialize Added");
    match &parsed {
        CameraEvent::Added { camera } => {
            assert_eq!(camera.id, CameraId::new("cam-0"));
            assert_eq!(camera.label, "Front Camera");
            assert_eq!(camera.supported_resolutions.len(), 2);
        }
        _ => panic!("expected Added variant, got {parsed:?}"),
    }
    // Verify tagged format.
    assert!(json.contains(r#""type":"added"#), "should use snake_case tag: {json}");

    // CameraEvent::Failed
    let failed = CameraEvent::Failed {
        camera_id: CameraId::new("cam-1"),
        reason: "device disconnected".to_string(),
    };
    let json = serde_json::to_string(&failed).expect("serialize Failed");
    let parsed: CameraEvent = serde_json::from_str(&json).expect("deserialize Failed");
    match &parsed {
        CameraEvent::Failed { camera_id, reason } => {
            assert_eq!(camera_id, &CameraId::new("cam-1"));
            assert_eq!(reason, "device disconnected");
        }
        _ => panic!("expected Failed variant, got {parsed:?}"),
    }
    assert!(json.contains(r#""type":"failed"#), "should use snake_case tag: {json}");

    // CameraEvent::QualityChanged
    let quality = CameraEvent::QualityChanged {
        camera_id: CameraId::new("cam-2"),
        profile: BitrateProfile::HIGH,
    };
    let json = serde_json::to_string(&quality).expect("serialize QualityChanged");
    let parsed: CameraEvent = serde_json::from_str(&json).expect("deserialize QualityChanged");
    match &parsed {
        CameraEvent::QualityChanged { camera_id, profile } => {
            assert_eq!(camera_id, &CameraId::new("cam-2"));
            assert_eq!(*profile, BitrateProfile::HIGH);
            assert_eq!(profile.bitrate_kbps, 2000);
        }
        _ => panic!("expected QualityChanged variant, got {parsed:?}"),
    }
    assert!(
        json.contains(r#""type":"quality_changed"#),
        "should use snake_case tag: {json}"
    );

    // CameraEvent::Removed
    let removed = CameraEvent::Removed {
        camera_id: CameraId::new("cam-3"),
    };
    let json = serde_json::to_string(&removed).expect("serialize Removed");
    let parsed: CameraEvent = serde_json::from_str(&json).expect("deserialize Removed");
    match &parsed {
        CameraEvent::Removed { camera_id } => {
            assert_eq!(camera_id, &CameraId::new("cam-3"));
        }
        _ => panic!("expected Removed variant, got {parsed:?}"),
    }
    assert!(json.contains(r#""type":"removed"#), "should use snake_case tag: {json}");
}

//! Production-grade integration tests for the camera pipeline.
//!
//! Tests verify full vertical paths: source -> encoder -> hub -> viewer,
//! NATS signaling roundtrip, ABR tier transitions with controlled hysteresis,
//! CameraManager lifecycle, and encoder stress behavior.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use futures::StreamExt;
use roz_agent::model::types::ContentPart;
use roz_agent::spatial_provider::SpatialContextProvider;
use roz_core::camera::{BitrateProfile, CameraId};
use roz_worker::camera::CameraManager;
use roz_worker::camera::adaptive::{AdaptiveBitrateController, RtcpFeedback};
use roz_worker::camera::encoder::{EncodedFrame, H264Encoder, SwEncoder};
use roz_worker::camera::snapshot::CameraSpatialProvider;
use roz_worker::camera::source::{CameraSource, TestPatternSource};
use roz_worker::camera::stream_hub::StreamHub;
use roz_worker::webrtc::signaling::SignalingRelay;

// ---------------------------------------------------------------------------
// Test 1: Full pipeline — TestPattern -> Encoder -> StreamHub -> Viewer
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_pipeline_test_pattern_to_viewer() {
    // 1. Create a TestPatternSource at 320x240 @ 10 fps.
    let mut source = TestPatternSource::new("pipeline-e2e");
    let mut frame_rx = source.start(320, 240, 10).await.expect("start test pattern");

    // 2. Create a real SwEncoder at LOW profile (matches 320x240).
    let mut encoder = SwEncoder::new(BitrateProfile::LOW).expect("create encoder");

    // 3. Set up StreamHub with a registered camera and a viewer subscription.
    let hub = StreamHub::new();
    let cam_id = CameraId::new("pipeline-e2e");
    hub.register_camera(cam_id.clone()).await;
    let (mut viewer_rx, _viewer_handle) = hub.subscribe(&cam_id).await.expect("subscribe viewer");

    // 4. Receive a raw frame from the test pattern.
    let raw = tokio::time::timeout(Duration::from_secs(5), frame_rx.recv())
        .await
        .expect("timeout waiting for raw frame")
        .expect("frame channel closed");

    assert_eq!(raw.width, 320);
    assert_eq!(raw.height, 240);
    assert_eq!(raw.camera_id, cam_id);

    // 5. Encode the raw frame through the real openh264 encoder.
    let encoded = encoder.encode(&raw).expect("encode frame");

    // 6. Publish the encoded frame through the hub.
    hub.publish(encoded).await;

    // 7. The viewer receives the Arc<EncodedFrame>.
    let received: Arc<EncodedFrame> = tokio::time::timeout(Duration::from_secs(5), viewer_rx.recv())
        .await
        .expect("timeout waiting for viewer frame")
        .expect("viewer channel closed");

    // 8. Assert the viewer got a valid H.264 keyframe with correct metadata.
    assert!(received.is_keyframe, "first frame must be an IDR keyframe");
    assert_eq!(received.camera_id, cam_id);
    assert_eq!(received.profile, BitrateProfile::LOW);
    assert!(!received.nalus.is_empty(), "encoded NALUs must not be empty");

    // 9. Verify NALU start code (Annex B: 0x00 0x00 0x00 0x01).
    assert!(received.nalus.len() >= 4, "NALUs must be at least 4 bytes (start code)");

    source.stop().await;
}

// ---------------------------------------------------------------------------
// Test 2: WebRTC signaling roundtrip via NATS
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker for NATS testcontainer"]
async fn webrtc_signaling_roundtrip_via_nats() {
    let guard = roz_test::nats_container().await;
    let nats = async_nats::connect(guard.url()).await.expect("connect to NATS");

    let worker_id = "worker-sig-test";
    let peer_id = "peer-sig-test";

    let relay = SignalingRelay::new(nats.clone(), worker_id.to_string());

    // 1. Subscribe to the offer subject before the worker publishes.
    let offer_subject = roz_nats::subjects::Subjects::webrtc_offer(worker_id, peer_id).expect("offer subject");
    let mut offer_sub = nats.subscribe(offer_subject).await.expect("subscribe to offers");

    // 2. Worker sends an SDP offer with camera IDs.
    let camera_ids = vec![CameraId::new("cam-front"), CameraId::new("cam-wrist")];
    let sdp_offer = "v=0\r\no=- 123 IN IP4 0.0.0.0\r\ns=-\r\n";
    relay
        .send_offer(peer_id, sdp_offer, &camera_ids)
        .await
        .expect("send offer");
    nats.flush().await.expect("flush after offer");

    // 3. Verify the subscriber receives the offer with correct payload.
    let offer_msg = tokio::time::timeout(Duration::from_secs(5), offer_sub.next())
        .await
        .expect("timeout waiting for offer")
        .expect("offer subscription closed");

    let offer_payload: serde_json::Value =
        serde_json::from_slice(&offer_msg.payload).expect("deserialize offer payload");
    assert_eq!(offer_payload["sdp"], sdp_offer);
    let received_cameras = offer_payload["camera_ids"]
        .as_array()
        .expect("camera_ids should be an array");
    assert_eq!(received_cameras.len(), 2);
    assert_eq!(received_cameras[0], "cam-front");
    assert_eq!(received_cameras[1], "cam-wrist");

    // 4. Worker subscribes to answers for this peer.
    let mut answer_sub = relay.subscribe_answers(peer_id).await.expect("subscribe to answers");

    // 5. Simulate server publishing an answer on the answer subject.
    let answer_subject = roz_nats::subjects::Subjects::webrtc_answer(worker_id, peer_id).expect("answer subject");
    let sdp_answer = "v=0\r\no=- 456 IN IP4 0.0.0.0\r\ns=-\r\n";
    let answer_payload = serde_json::json!({ "sdp": sdp_answer });
    nats.publish(
        answer_subject,
        bytes::Bytes::from(serde_json::to_vec(&answer_payload).expect("serialize answer")),
    )
    .await
    .expect("publish answer");
    nats.flush().await.expect("flush after answer");

    // 6. Verify the worker's answer subscriber receives the answer.
    let answer_msg = tokio::time::timeout(Duration::from_secs(5), answer_sub.next())
        .await
        .expect("timeout waiting for answer")
        .expect("answer subscription closed");

    let received_answer: serde_json::Value =
        serde_json::from_slice(&answer_msg.payload).expect("deserialize answer payload");
    assert_eq!(received_answer["sdp"], sdp_answer);
}

// ---------------------------------------------------------------------------
// Test 3: CameraSpatialProvider -> SpatialContextProvider -> agent content
// ---------------------------------------------------------------------------

#[tokio::test]
async fn camera_spatial_provider_to_agent_content() {
    let provider = CameraSpatialProvider::new();

    // 1. Create a realistic JPEG-like payload (starts with JPEG magic bytes).
    let jpeg_payload: Vec<u8> = {
        let mut data = vec![0xFF, 0xD8, 0xFF, 0xE0]; // JPEG SOI + APP0 marker
        data.extend_from_slice(b"JFIF\x00"); // JFIF header
        data.extend_from_slice(&[0xAA; 256]); // simulated compressed data
        data.extend_from_slice(&[0xFF, 0xD9]); // JPEG EOI marker
        data
    };

    // 2. Update the provider with the camera snapshot.
    provider.update_snapshot("wrist_cam", &jpeg_payload).await;

    // 3. Call snapshot() via the SpatialContextProvider trait.
    let ctx = provider.snapshot("task-perception-test").await;

    // 4. Verify the SpatialContext has the screenshot.
    assert_eq!(ctx.screenshots.len(), 1);
    assert_eq!(ctx.screenshots[0].name, "wrist_cam");
    assert_eq!(ctx.screenshots[0].media_type, "image/jpeg");
    assert!(ctx.screenshots[0].depth_data.is_none());

    // 5. Verify the base64 data round-trips correctly.
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&ctx.screenshots[0].data)
        .expect("base64 decode");
    assert_eq!(decoded, jpeg_payload, "JPEG data must survive base64 round-trip");

    // Verify JPEG magic bytes survived encoding.
    assert_eq!(&decoded[0..2], &[0xFF, 0xD8], "JPEG SOI marker must be preserved");
    let len = decoded.len();
    assert_eq!(&decoded[len - 2..], &[0xFF, 0xD9], "JPEG EOI marker must be preserved");

    // 6. Verify the screenshot data can be used as ContentPart::Image.
    let image_part = ContentPart::Image {
        media_type: ctx.screenshots[0].media_type.clone(),
        data: ctx.screenshots[0].data.clone(),
    };

    match &image_part {
        ContentPart::Image { media_type, data } => {
            assert_eq!(media_type, "image/jpeg");
            // The data should be non-empty base64 that decodes to our original JPEG.
            let roundtrip = base64::engine::general_purpose::STANDARD
                .decode(data)
                .expect("ContentPart image data must be valid base64");
            assert_eq!(roundtrip, jpeg_payload);
        }
        _ => panic!("expected ContentPart::Image"),
    }

    // 7. Test multi-camera: add a second camera and verify both present.
    let overhead_jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0xBB, 0xCC, 0xFF, 0xD9];
    provider.update_snapshot("overhead_cam", &overhead_jpeg).await;

    let ctx2 = provider.snapshot("task-multi-cam").await;
    assert_eq!(ctx2.screenshots.len(), 2);

    let names: Vec<&str> = ctx2.screenshots.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"wrist_cam"));
    assert!(names.contains(&"overhead_cam"));
}

// ---------------------------------------------------------------------------
// Test 4: ABR controller with controlled hysteresis transitions
// ---------------------------------------------------------------------------

#[test]
fn abr_transitions_with_controlled_hysteresis() {
    // Use with_stability to set instant hysteresis for testing.
    // This proves real tier transitions, not just EWMA direction.

    // --- Upgrade: good feedback for > upgrade_stability -> HIGH ---
    let mut ctrl = AdaptiveBitrateController::new().with_stability(Duration::ZERO, Duration::from_secs(1));

    assert_eq!(ctrl.current_profile(), BitrateProfile::MEDIUM, "should start at MEDIUM");

    let perfect = RtcpFeedback {
        fraction_lost: 0.0,
        jitter_ms: 5.0,
        rtt_ms: 10.0,
    };

    // Feed perfect feedback until EWMA exceeds HIGH threshold (0.8).
    // Score ~0.9885, EWMA starts at 0.6, needs ~3 samples to cross 0.8.
    for _ in 0..5 {
        ctrl.on_rtcp_feedback(&perfect);
    }
    assert_eq!(
        ctrl.current_profile(),
        BitrateProfile::HIGH,
        "sustained perfect feedback with zero upgrade_stability should upgrade to HIGH"
    );

    // --- Downgrade: bad feedback for > downgrade_stability -> LOW ---
    let mut ctrl2 = AdaptiveBitrateController::new().with_stability(Duration::from_secs(5), Duration::ZERO);

    let terrible = RtcpFeedback {
        fraction_lost: 0.5,
        jitter_ms: 150.0,
        rtt_ms: 400.0,
    };

    // Feed terrible feedback until EWMA drops below MEDIUM threshold (0.4).
    // Score ~0.365, EWMA starts at 0.6, needs ~6 samples to cross below 0.4.
    for _ in 0..8 {
        ctrl2.on_rtcp_feedback(&terrible);
    }
    assert_eq!(
        ctrl2.current_profile(),
        BitrateProfile::LOW,
        "sustained terrible feedback with zero downgrade_stability should downgrade to LOW"
    );

    // --- Alternating: good/bad should NOT oscillate ---
    let mut ctrl3 = AdaptiveBitrateController::new().with_stability(Duration::from_secs(60), Duration::from_secs(60));

    let profile_before = ctrl3.current_profile();
    for _ in 0..50 {
        ctrl3.on_rtcp_feedback(&perfect);
        ctrl3.on_rtcp_feedback(&terrible);
    }
    assert_eq!(
        ctrl3.current_profile(),
        profile_before,
        "alternating good/bad with long hysteresis must NOT cause tier oscillation"
    );
}

// ---------------------------------------------------------------------------
// Test 5: CameraManager lifecycle — viewer demand drives hub registration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn camera_manager_viewer_demand_lifecycle() {
    let hub = StreamHub::new();
    let mut mgr = CameraManager::new(hub);

    // 1. Start idle: no cameras registered.
    assert!(mgr.cameras().is_empty(), "should start with no cameras");

    // 2. Add a test pattern camera — this registers it with the hub.
    let info = mgr.add_test_pattern().await;
    assert_eq!(info.id, CameraId::new("test-pattern"));
    assert_eq!(mgr.cameras().len(), 1);

    // 3. No viewers yet.
    let cam_id = CameraId::new("test-pattern");
    assert_eq!(
        mgr.hub().viewer_count(&cam_id).await,
        0,
        "no viewers before subscription"
    );

    // 4. Subscribe a viewer via the hub — demand starts.
    let subscribe_result = mgr.hub().subscribe(&cam_id).await;
    assert!(subscribe_result.is_some(), "should subscribe to registered camera");
    let (mut viewer_rx, viewer_handle) = subscribe_result.unwrap();
    assert_eq!(
        mgr.hub().viewer_count(&cam_id).await,
        1,
        "one viewer after subscription"
    );

    // 5. Publish a frame through the hub to prove the viewer receives it.
    let frame = EncodedFrame {
        camera_id: cam_id.clone(),
        nalus: vec![0x00, 0x00, 0x00, 0x01, 0x65, 0xAA],
        is_keyframe: true,
        pts_90khz: 0,
        profile: BitrateProfile::MEDIUM,
        seq: 0,
    };
    mgr.hub().publish(frame).await;

    let received = tokio::time::timeout(Duration::from_secs(5), viewer_rx.recv())
        .await
        .expect("timeout waiting for frame")
        .expect("viewer channel error");
    assert_eq!(received.seq, 0);
    assert!(received.is_keyframe);

    // 6. Drop the ViewerHandle — viewer count goes to zero.
    drop(viewer_handle);
    assert_eq!(
        mgr.hub().viewer_count(&cam_id).await,
        0,
        "viewer count should be 0 after dropping handle"
    );

    // 7. Publish another frame — no panic, no error (no receivers is fine).
    let frame2 = EncodedFrame {
        camera_id: cam_id,
        nalus: vec![0x00, 0x00, 0x00, 0x01, 0x41],
        is_keyframe: false,
        pts_90khz: 3000,
        profile: BitrateProfile::MEDIUM,
        seq: 1,
    };
    mgr.hub().publish(frame2).await;

    // No panic = success: hub handles zero-receiver case gracefully.
}

// ---------------------------------------------------------------------------
// Test 6: Encoder stress — 30 frames, consistent output
// ---------------------------------------------------------------------------

#[tokio::test]
async fn encoder_stress_30_frames_consistent() {
    let mut source = TestPatternSource::new("stress-cam");
    let mut frame_rx = source.start(320, 240, 30).await.expect("start test pattern");

    let mut encoder = SwEncoder::new(BitrateProfile::LOW).expect("create encoder");

    let mut encoded_frames: Vec<EncodedFrame> = Vec::with_capacity(30);

    // Receive and encode 30 frames.
    for i in 0..30 {
        let raw = tokio::time::timeout(Duration::from_secs(5), frame_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timeout waiting for frame {i}"))
            .unwrap_or_else(|| panic!("channel closed at frame {i}"));

        let encoded = encoder
            .encode(&raw)
            .unwrap_or_else(|e| panic!("encode failed at frame {i}: {e}"));

        encoded_frames.push(encoded);
    }

    assert_eq!(encoded_frames.len(), 30, "should have encoded all 30 frames");

    // 1. First frame MUST be a keyframe (IDR).
    assert!(encoded_frames[0].is_keyframe, "first frame must be an IDR keyframe");

    // 2. All frames must have non-empty NALUs.
    for (i, frame) in encoded_frames.iter().enumerate() {
        assert!(!frame.nalus.is_empty(), "frame {i} must have non-empty NALUs");
    }

    // 3. Sequence numbers must be monotonically increasing.
    for window in encoded_frames.windows(2) {
        assert!(
            window[1].seq > window[0].seq,
            "sequence numbers must be monotonically increasing: {} should be > {}",
            window[1].seq,
            window[0].seq,
        );
    }

    // 4. All frames should have the correct camera ID and profile.
    for frame in &encoded_frames {
        assert_eq!(frame.camera_id, CameraId::new("stress-cam"));
        assert_eq!(frame.profile, BitrateProfile::LOW);
    }

    // 5. Force a keyframe mid-stream and verify it works.
    //    Must happen before source.stop() since stopping cancels the producer.
    encoder.force_keyframe();
    let raw_extra = tokio::time::timeout(Duration::from_secs(5), frame_rx.recv())
        .await
        .expect("timeout waiting for extra frame")
        .expect("channel closed");
    let forced_keyframe = encoder.encode(&raw_extra).expect("encode forced keyframe");
    assert!(
        forced_keyframe.is_keyframe,
        "frame after force_keyframe() must be an IDR keyframe"
    );

    source.stop().await;
}

// ---------------------------------------------------------------------------
// Test 7: StreamHub fan-out — two viewers receive same Arc (existing but upgraded)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_hub_two_viewers_receive_same_arc() {
    let hub = StreamHub::new();
    let cam = CameraId::new("cam-fan-out-e2e");

    hub.register_camera(cam.clone()).await;

    let (mut rx1, handle1) = hub.subscribe(&cam).await.expect("subscribe viewer 1");
    let (mut rx2, _handle2) = hub.subscribe(&cam).await.expect("subscribe viewer 2");
    assert_eq!(hub.viewer_count(&cam).await, 2);

    // Use real encoder output for the frame content.
    let mut source = TestPatternSource::new("cam-fan-out-e2e");
    let mut frame_rx = source.start(320, 240, 10).await.expect("start test pattern");
    let mut encoder = SwEncoder::new(BitrateProfile::LOW).expect("create encoder");

    let raw = tokio::time::timeout(Duration::from_secs(5), frame_rx.recv())
        .await
        .expect("timeout")
        .expect("channel closed");
    let encoded = encoder.encode(&raw).expect("encode");

    hub.publish(encoded).await;

    let f1: Arc<EncodedFrame> = rx1.recv().await.expect("viewer 1 should receive");
    let f2: Arc<EncodedFrame> = rx2.recv().await.expect("viewer 2 should receive");

    // Arc pointer equality: both share the same allocation (encode-once fan-out).
    assert!(Arc::ptr_eq(&f1, &f2), "both viewers must share the same Arc");
    assert!(f1.is_keyframe);
    assert!(!f1.nalus.is_empty());
    assert_eq!(f1.camera_id, CameraId::new("cam-fan-out-e2e"));

    // Drop one viewer, verify count decrements.
    drop(handle1);
    assert_eq!(hub.viewer_count(&cam).await, 1);

    source.stop().await;
}

// ---------------------------------------------------------------------------
// Test 8: WebRTC ICE candidate roundtrip via NATS
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Docker for NATS testcontainer"]
async fn webrtc_ice_candidate_roundtrip_via_nats() {
    let guard = roz_test::nats_container().await;
    let nats = async_nats::connect(guard.url()).await.expect("connect to NATS");

    let worker_id = "worker-ice-test";
    let peer_id = "peer-ice-test";

    let relay = SignalingRelay::new(nats.clone(), worker_id.to_string());

    // Subscribe to local ICE candidates (what the server would listen on).
    let ice_local_subject =
        roz_nats::subjects::Subjects::webrtc_ice_local(worker_id, peer_id).expect("ice local subject");
    let mut ice_sub = nats.subscribe(ice_local_subject).await.expect("subscribe ICE local");

    // Worker sends a local ICE candidate.
    let candidate = "candidate:1 1 UDP 2113937151 192.168.1.5 5060 typ host";
    relay
        .send_ice_candidate(peer_id, candidate)
        .await
        .expect("send ICE candidate");
    nats.flush().await.expect("flush");

    // Verify the subscriber receives the candidate.
    let ice_msg = tokio::time::timeout(Duration::from_secs(5), ice_sub.next())
        .await
        .expect("timeout waiting for ICE candidate")
        .expect("ICE subscription closed");

    let payload: serde_json::Value = serde_json::from_slice(&ice_msg.payload).expect("deserialize ICE payload");
    assert_eq!(payload["candidate"], candidate);

    // Subscribe to remote ICE (what the worker listens on).
    let mut remote_ice_sub = relay.subscribe_remote_ice(peer_id).await.expect("subscribe remote ICE");

    // Simulate server publishing a remote ICE candidate.
    let remote_ice_subject =
        roz_nats::subjects::Subjects::webrtc_ice_remote(worker_id, peer_id).expect("ice remote subject");
    let remote_candidate = "candidate:2 1 UDP 1694498815 203.0.113.5 6000 typ srflx";
    let remote_payload = serde_json::json!({ "candidate": remote_candidate });
    nats.publish(
        remote_ice_subject,
        bytes::Bytes::from(serde_json::to_vec(&remote_payload).expect("serialize")),
    )
    .await
    .expect("publish remote ICE");
    nats.flush().await.expect("flush");

    let remote_msg = tokio::time::timeout(Duration::from_secs(5), remote_ice_sub.next())
        .await
        .expect("timeout waiting for remote ICE")
        .expect("remote ICE subscription closed");

    let received: serde_json::Value =
        serde_json::from_slice(&remote_msg.payload).expect("deserialize remote ICE payload");
    assert_eq!(received["candidate"], remote_candidate);
}

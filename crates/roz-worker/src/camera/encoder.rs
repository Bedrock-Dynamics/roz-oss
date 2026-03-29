use openh264::encoder::{BitRate, EncoderConfig, FrameRate, FrameType, RateControlMode};
use openh264::formats::YUVBuffer;
use roz_core::camera::{BitrateProfile, CameraId, EncoderSelection};

/// Encoded H.264 frame ready for RTP packetization or fan-out.
pub struct EncodedFrame {
    pub camera_id: CameraId,
    /// H.264 Annex B NAL units (with start codes).
    pub nalus: Vec<u8>,
    pub is_keyframe: bool,
    /// RTP timestamp at 90 kHz clock rate.
    pub pts_90khz: u32,
    pub profile: BitrateProfile,
    pub seq: u64,
}

/// Which encoder backend produced the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderBackend {
    OpenH264Software,
    V4l2M2MHardware,
}

/// Trait for H.264 encoding backends.
///
/// Implementations must be `Send` so they can live in a tokio spawn context,
/// but encoding itself is synchronous (blocking CPU work).
pub trait H264Encoder: Send {
    /// Encode a single raw I420 frame into H.264 NAL units.
    fn encode(&mut self, frame: &super::source::RawFrame) -> anyhow::Result<EncodedFrame>;

    /// Reconfigure the encoder for a new bitrate profile.
    ///
    /// The next `encode()` call will use the new settings. Resolution changes
    /// cause an automatic IDR frame.
    fn reconfigure(&mut self, profile: BitrateProfile) -> anyhow::Result<()>;

    /// Request that the next encoded frame be a keyframe (IDR).
    fn force_keyframe(&mut self);

    /// Which backend this encoder uses.
    fn backend(&self) -> EncoderBackend;
}

// ---------------------------------------------------------------------------
// Software encoder (openh264)
// ---------------------------------------------------------------------------

/// Software H.264 encoder wrapping the openh264 crate.
pub struct SwEncoder {
    encoder: openh264::encoder::Encoder,
    current_profile: BitrateProfile,
    pending_keyframe: bool,
}

impl SwEncoder {
    /// Create a new software encoder configured for the given bitrate profile.
    pub fn new(profile: BitrateProfile) -> anyhow::Result<Self> {
        let config = Self::build_config(profile);
        let encoder = openh264::encoder::Encoder::with_api_config(openh264::OpenH264API::from_source(), config)?;
        Ok(Self {
            encoder,
            current_profile: profile,
            pending_keyframe: false,
        })
    }

    #[allow(clippy::cast_precision_loss)] // fps values are small integers, no precision loss
    const fn build_config(profile: BitrateProfile) -> EncoderConfig {
        EncoderConfig::new()
            .max_frame_rate(FrameRate::from_hz(profile.fps as f32))
            .bitrate(BitRate::from_bps(profile.bitrate_kbps * 1000))
            .rate_control_mode(RateControlMode::Bitrate)
    }

    /// Convert a `RawFrame`'s microsecond timestamp to a 90 kHz RTP timestamp.
    #[allow(clippy::cast_possible_truncation)] // wrapping u32 is intentional for RTP
    const fn to_rtp_ts(timestamp_us: u64) -> u32 {
        // 90 kHz clock: ts_90k = ts_us * 90_000 / 1_000_000 = ts_us * 9 / 100
        // Use wrapping arithmetic -- RTP timestamps wrap at u32::MAX by design.
        ((timestamp_us * 9) / 100) as u32
    }
}

impl H264Encoder for SwEncoder {
    fn encode(&mut self, frame: &super::source::RawFrame) -> anyhow::Result<EncodedFrame> {
        if self.pending_keyframe {
            self.encoder.force_intra_frame();
            self.pending_keyframe = false;
        }

        // Wrap the raw I420 data as a YUVBuffer the encoder can consume.
        let yuv = YUVBuffer::from_vec(frame.data.clone(), frame.width as usize, frame.height as usize);
        let bitstream = self.encoder.encode(&yuv)?;

        let mut nalus = Vec::new();
        bitstream.write_vec(&mut nalus);

        let is_keyframe = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);

        Ok(EncodedFrame {
            camera_id: frame.camera_id.clone(),
            nalus,
            is_keyframe,
            pts_90khz: Self::to_rtp_ts(frame.timestamp_us),
            profile: self.current_profile,
            seq: frame.seq,
        })
    }

    fn reconfigure(&mut self, profile: BitrateProfile) -> anyhow::Result<()> {
        let config = Self::build_config(profile);
        self.encoder = openh264::encoder::Encoder::with_api_config(openh264::OpenH264API::from_source(), config)?;
        self.current_profile = profile;
        // Force a keyframe after reconfigure so decoders can resync.
        self.pending_keyframe = true;
        Ok(())
    }

    fn force_keyframe(&mut self) {
        self.pending_keyframe = true;
    }

    fn backend(&self) -> EncoderBackend {
        EncoderBackend::OpenH264Software
    }
}

// ---------------------------------------------------------------------------
// Hardware encoder detect (V4L2 M2M)
// ---------------------------------------------------------------------------

/// Check for a V4L2 M2M hardware encoder device.
///
/// On Linux, `/dev/video11` is the conventional device for the Broadcom codec
/// on Raspberry Pi 4. Returns the device path if present.
///
/// On non-Linux platforms this always returns `None`.
#[cfg(target_os = "linux")]
pub fn detect_hw_encoder() -> Option<String> {
    let path = std::path::Path::new("/dev/video11");
    if path.exists() {
        Some("/dev/video11".to_string())
    } else {
        None
    }
}

#[cfg(not(target_os = "linux"))]
pub const fn detect_hw_encoder() -> Option<String> {
    None
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Create an encoder according to the selection policy.
///
/// - `Auto`: try hardware, fall back to software.
/// - `Hardware`: require hardware (error if unavailable).
/// - `Software`: always use openh264.
pub fn create_encoder(selection: EncoderSelection, profile: BitrateProfile) -> anyhow::Result<Box<dyn H264Encoder>> {
    match selection {
        EncoderSelection::Auto => {
            if detect_hw_encoder().is_some() {
                // TODO: instantiate real V4L2 M2M encoder when implemented.
                tracing::info!("hardware encoder detected but not yet implemented, falling back to software");
            }
            Ok(Box::new(SwEncoder::new(profile)?))
        }
        EncoderSelection::Hardware => {
            if detect_hw_encoder().is_some() {
                // TODO: instantiate real V4L2 M2M encoder.
                anyhow::bail!("V4L2 M2M hardware encoder not yet implemented");
            }
            anyhow::bail!("no hardware encoder detected (missing /dev/video11)")
        }
        EncoderSelection::Software => Ok(Box::new(SwEncoder::new(profile)?)),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::source::{RawFrame, TestPatternSource};

    fn test_frame(width: u32, height: u32, seq: u64) -> RawFrame {
        RawFrame {
            camera_id: CameraId::new("test-enc"),
            width,
            height,
            data: TestPatternSource::generate_frame(width, height, seq),
            timestamp_us: seq * 33_333, // ~30 fps
            seq,
        }
    }

    #[test]
    fn sw_encoder_produces_output() {
        let profile = BitrateProfile::LOW;
        let mut enc = SwEncoder::new(profile).expect("encoder creation");

        let frame = test_frame(profile.width, profile.height, 0);
        let encoded = enc.encode(&frame).expect("encode");

        assert!(!encoded.nalus.is_empty(), "encoded output must be non-empty");
        assert_eq!(encoded.seq, 0);
        assert_eq!(encoded.camera_id, CameraId::new("test-enc"));
        assert_eq!(encoded.profile, profile);
        // First frame is always an IDR keyframe.
        assert!(encoded.is_keyframe, "first frame should be a keyframe");
    }

    #[test]
    fn sw_encoder_keyframe_request() {
        let profile = BitrateProfile::LOW;
        let mut enc = SwEncoder::new(profile).expect("encoder creation");

        // Encode a few P-frames first.
        for seq in 0..5 {
            let frame = test_frame(profile.width, profile.height, seq);
            enc.encode(&frame).expect("encode");
        }

        // Now force a keyframe.
        enc.force_keyframe();
        let frame = test_frame(profile.width, profile.height, 5);
        let encoded = enc.encode(&frame).expect("encode after force_keyframe");
        assert!(encoded.is_keyframe, "frame after force_keyframe must be IDR");
    }

    #[test]
    fn sw_encoder_reconfigure() {
        let mut enc = SwEncoder::new(BitrateProfile::LOW).expect("encoder creation");

        // Encode at LOW.
        let frame_low = test_frame(BitrateProfile::LOW.width, BitrateProfile::LOW.height, 0);
        let encoded_low = enc.encode(&frame_low).expect("encode low");
        assert_eq!(encoded_low.profile, BitrateProfile::LOW);

        // Reconfigure to MEDIUM.
        enc.reconfigure(BitrateProfile::MEDIUM).expect("reconfigure");

        let frame_med = test_frame(BitrateProfile::MEDIUM.width, BitrateProfile::MEDIUM.height, 1);
        let encoded_med = enc.encode(&frame_med).expect("encode medium");
        assert_eq!(encoded_med.profile, BitrateProfile::MEDIUM);
        assert!(!encoded_med.nalus.is_empty());
        // After reconfigure, next frame should be a keyframe.
        assert!(encoded_med.is_keyframe, "reconfigure should trigger keyframe");
    }

    #[test]
    fn detect_hw_returns_none_on_macos() {
        // On macOS (and any non-Linux CI), there is no V4L2 M2M device.
        if !cfg!(target_os = "linux") {
            assert!(
                detect_hw_encoder().is_none(),
                "detect_hw_encoder should return None on non-Linux"
            );
        }
    }

    #[test]
    fn create_encoder_software_selection() {
        let enc = create_encoder(EncoderSelection::Software, BitrateProfile::LOW).expect("create");
        assert_eq!(enc.backend(), EncoderBackend::OpenH264Software);
    }

    #[test]
    fn create_encoder_auto_falls_back_to_software() {
        // On CI (non-Linux or Linux without /dev/video11) Auto should give software.
        let enc = create_encoder(EncoderSelection::Auto, BitrateProfile::MEDIUM).expect("create auto");
        assert_eq!(enc.backend(), EncoderBackend::OpenH264Software);
    }

    #[test]
    fn rtp_timestamp_calculation() {
        // 1 second = 1_000_000 us -> 90_000 ticks at 90 kHz
        assert_eq!(SwEncoder::to_rtp_ts(1_000_000), 90_000);
        // 0
        assert_eq!(SwEncoder::to_rtp_ts(0), 0);
        // 33_333 us (~30fps) -> ~3000 ticks
        assert_eq!(SwEncoder::to_rtp_ts(33_333), 2999);
    }
}

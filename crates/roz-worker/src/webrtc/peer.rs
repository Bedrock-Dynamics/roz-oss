use std::collections::HashMap;
use std::time::Instant;

use roz_core::camera::CameraId;
use str0m::Rtc;
use str0m::change::SdpPendingOffer;
use str0m::media::{Direction, MediaKind, Mid};

/// Wraps a str0m `Rtc` instance for one viewer WebRTC connection.
///
/// str0m is sans-IO: it produces UDP packets to send and consumes
/// received UDP packets. This struct handles the signaling-level
/// operations (offer/answer/ICE candidates). The full run loop
/// with UDP socket bridging is not implemented here -- that requires
/// integration with tokio UDP and will be wired in the session manager.
pub struct ViewerPeer {
    peer_id: String,
    rtc: Rtc,
    /// Maps each camera to its media line identifier.
    tracks: HashMap<CameraId, Mid>,
    /// Pending offer state held between `create_offer` and `apply_answer`.
    pending_offer: Option<SdpPendingOffer>,
}

impl ViewerPeer {
    /// Create a new `ViewerPeer` with the given identifier.
    ///
    /// Configures str0m in ICE-lite mode (server-side, public IP).
    /// STUN/TURN configuration is handled externally via ICE candidates.
    pub fn new(peer_id: String) -> Self {
        let rtc = Rtc::builder().set_ice_lite(true).build(Instant::now());
        Self {
            peer_id,
            rtc,
            tracks: HashMap::new(),
            pending_offer: None,
        }
    }

    /// Create an SDP offer that includes one `SendOnly` video track per camera.
    ///
    /// Returns the SDP offer as a string. The caller must send this to the
    /// remote peer and collect the answer via `apply_answer`.
    ///
    /// # Errors
    ///
    /// Returns an error if no cameras are provided or if SDP generation fails.
    pub fn create_offer(&mut self, camera_ids: &[CameraId]) -> anyhow::Result<String> {
        anyhow::ensure!(!camera_ids.is_empty(), "at least one camera is required");

        let mut sdp_api = self.rtc.sdp_api();

        for cam_id in camera_ids {
            let mid = sdp_api.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
            self.tracks.insert(cam_id.clone(), mid);
        }

        let (offer, pending) = sdp_api
            .apply()
            .ok_or_else(|| anyhow::anyhow!("SDP API produced no offer (no media changes)"))?;

        self.pending_offer = Some(pending);

        Ok(offer.to_sdp_string())
    }

    /// Apply a remote SDP answer received from the viewer.
    ///
    /// Must be called after `create_offer`. Consumes the pending offer state.
    ///
    /// # Errors
    ///
    /// Returns an error if no offer is pending or if the answer is invalid.
    pub fn apply_answer(&mut self, sdp: &str) -> anyhow::Result<()> {
        let pending = self
            .pending_offer
            .take()
            .ok_or_else(|| anyhow::anyhow!("no pending offer to accept answer for"))?;

        let answer =
            str0m::change::SdpAnswer::from_sdp_string(sdp).map_err(|e| anyhow::anyhow!("invalid SDP answer: {e}"))?;

        self.rtc
            .sdp_api()
            .accept_answer(pending, answer)
            .map_err(|e| anyhow::anyhow!("failed to accept SDP answer: {e}"))?;

        Ok(())
    }

    /// Add a remote ICE candidate (trickle ICE).
    ///
    /// The candidate string should be in SDP candidate attribute format
    /// (e.g. `"candidate:... udp ... typ host ..."`).
    ///
    /// # Errors
    ///
    /// Returns an error if the candidate string is malformed.
    pub fn add_remote_candidate(&mut self, candidate: &str) -> anyhow::Result<()> {
        let parsed =
            str0m::Candidate::from_sdp_string(candidate).map_err(|e| anyhow::anyhow!("invalid ICE candidate: {e}"))?;
        self.rtc.add_remote_candidate(parsed);
        Ok(())
    }

    /// The unique identifier for this peer connection.
    #[must_use]
    pub fn peer_id(&self) -> &str {
        &self.peer_id
    }

    /// The camera-to-Mid track mapping. Useful for routing encoded frames
    /// to the correct media line.
    #[must_use]
    pub const fn tracks(&self) -> &HashMap<CameraId, Mid> {
        &self.tracks
    }

    /// Mutable access to the underlying str0m `Rtc` instance.
    ///
    /// Needed for the run loop to call `poll_output()`, `handle_input()`,
    /// and `writer()`.
    pub const fn rtc_mut(&mut self) -> &mut Rtc {
        &mut self.rtc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_id_returned() {
        let peer = ViewerPeer::new("viewer-42".to_string());
        assert_eq!(peer.peer_id(), "viewer-42");
    }

    #[test]
    fn create_offer_empty_cameras_rejected() {
        let mut peer = ViewerPeer::new("v1".to_string());
        let result = peer.create_offer(&[]);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("at least one camera"),
            "error should mention cameras"
        );
    }

    #[test]
    fn create_offer_produces_sdp() {
        let mut peer = ViewerPeer::new("v2".to_string());
        let cams = vec![CameraId::new("cam-front"), CameraId::new("cam-rear")];

        let sdp = peer.create_offer(&cams).expect("offer creation should succeed");

        // SDP must start with the v= line.
        assert!(
            sdp.starts_with("v=0"),
            "SDP should start with v=0, got: {}",
            &sdp[..20.min(sdp.len())]
        );
        // Should contain video media lines.
        assert!(sdp.contains("m=video"), "SDP should contain video m-lines");

        // Tracks should be populated.
        assert_eq!(peer.tracks().len(), 2);
        assert!(peer.tracks().contains_key(&CameraId::new("cam-front")));
        assert!(peer.tracks().contains_key(&CameraId::new("cam-rear")));
    }

    #[test]
    fn apply_answer_without_offer_fails() {
        let mut peer = ViewerPeer::new("v3".to_string());
        let result = peer.apply_answer("v=0\r\n");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no pending offer"));
    }

    #[test]
    fn add_remote_candidate_rejects_garbage() {
        let mut peer = ViewerPeer::new("v4".to_string());
        let result = peer.add_remote_candidate("not a valid candidate");
        assert!(result.is_err());
    }
}

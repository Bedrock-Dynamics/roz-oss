use roz_nats::subjects::Subjects;

/// NATS-based signaling relay for WebRTC SDP/ICE exchange.
///
/// The worker publishes offers and local ICE candidates, and subscribes
/// to answers and remote ICE candidates for each peer connection. The
/// server side mirrors this (publishing answers, subscribing to offers).
pub struct SignalingRelay {
    nats: async_nats::Client,
    worker_id: String,
}

impl SignalingRelay {
    /// Create a new signaling relay bound to a worker.
    #[must_use]
    pub const fn new(nats: async_nats::Client, worker_id: String) -> Self {
        Self { nats, worker_id }
    }

    /// Publish an SDP offer for a peer.
    ///
    /// Subject: `webrtc.{worker_id}.{peer_id}.offer`
    ///
    /// The payload is a JSON object containing the SDP string and the
    /// list of camera IDs included in the offer.
    ///
    /// # Errors
    ///
    /// Returns an error if subject construction or NATS publish fails.
    pub async fn send_offer(
        &self,
        peer_id: &str,
        sdp: &str,
        camera_ids: &[roz_core::camera::CameraId],
    ) -> anyhow::Result<()> {
        let subject = Subjects::webrtc_offer(&self.worker_id, peer_id)?;
        let payload = serde_json::json!({
            "sdp": sdp,
            "camera_ids": camera_ids,
        });
        self.nats
            .publish(subject, bytes::Bytes::from(serde_json::to_vec(&payload)?))
            .await?;
        Ok(())
    }

    /// Publish a local ICE candidate for a peer.
    ///
    /// Subject: `webrtc.{worker_id}.{peer_id}.ice.local`
    ///
    /// # Errors
    ///
    /// Returns an error if subject construction or NATS publish fails.
    pub async fn send_ice_candidate(&self, peer_id: &str, candidate: &str) -> anyhow::Result<()> {
        let subject = Subjects::webrtc_ice_local(&self.worker_id, peer_id)?;
        let payload = serde_json::json!({ "candidate": candidate });
        self.nats
            .publish(subject, bytes::Bytes::from(serde_json::to_vec(&payload)?))
            .await?;
        Ok(())
    }

    /// Subscribe to SDP answers from the server for a peer.
    ///
    /// Subject: `webrtc.{worker_id}.{peer_id}.answer`
    ///
    /// # Errors
    ///
    /// Returns an error if subject construction or NATS subscribe fails.
    pub async fn subscribe_answers(&self, peer_id: &str) -> anyhow::Result<async_nats::Subscriber> {
        let subject = Subjects::webrtc_answer(&self.worker_id, peer_id)?;
        let sub = self.nats.subscribe(subject).await?;
        Ok(sub)
    }

    /// Subscribe to remote ICE candidates from the server for a peer.
    ///
    /// Subject: `webrtc.{worker_id}.{peer_id}.ice.remote`
    ///
    /// # Errors
    ///
    /// Returns an error if subject construction or NATS subscribe fails.
    pub async fn subscribe_remote_ice(&self, peer_id: &str) -> anyhow::Result<async_nats::Subscriber> {
        let subject = Subjects::webrtc_ice_remote(&self.worker_id, peer_id)?;
        let sub = self.nats.subscribe(subject).await?;
        Ok(sub)
    }

    /// The worker ID this relay is bound to.
    #[must_use]
    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }
}

#[cfg(test)]
mod tests {
    use roz_nats::subjects::Subjects;

    #[test]
    fn offer_subject_construction() {
        let subject = Subjects::webrtc_offer("worker-1", "peer-abc").unwrap();
        assert_eq!(subject, "webrtc.worker-1.peer-abc.offer");
    }

    #[test]
    fn answer_subject_construction() {
        let subject = Subjects::webrtc_answer("worker-1", "peer-abc").unwrap();
        assert_eq!(subject, "webrtc.worker-1.peer-abc.answer");
    }

    #[test]
    fn ice_local_subject_construction() {
        let subject = Subjects::webrtc_ice_local("worker-1", "peer-abc").unwrap();
        assert_eq!(subject, "webrtc.worker-1.peer-abc.ice.local");
    }

    #[test]
    fn ice_remote_subject_construction() {
        let subject = Subjects::webrtc_ice_remote("worker-1", "peer-abc").unwrap();
        assert_eq!(subject, "webrtc.worker-1.peer-abc.ice.remote");
    }

    #[test]
    fn subject_rejects_invalid_tokens() {
        // Dots in worker_id should be rejected.
        assert!(Subjects::webrtc_offer("worker.1", "peer-abc").is_err());
        // Empty peer_id should be rejected.
        assert!(Subjects::webrtc_offer("worker-1", "").is_err());
    }
}

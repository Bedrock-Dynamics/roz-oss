use std::time::{Duration, Instant};

use roz_core::camera::BitrateProfile;

/// RTCP feedback used to compute network quality score.
#[derive(Debug, Clone)]
pub struct RtcpFeedback {
    /// Fraction of packets lost, 0.0 (none) to 1.0 (all).
    pub fraction_lost: f64,
    /// Interarrival jitter in milliseconds.
    pub jitter_ms: f64,
    /// Round-trip time in milliseconds.
    pub rtt_ms: f64,
}

/// Adaptive bitrate controller using EWMA network scoring and hysteresis.
///
/// Uses `BitrateProfile::LADDER` (HIGH, MEDIUM, LOW) as the quality tiers.
/// Downgrades quickly (1 s sustained below threshold) to prevent visible
/// artifacts; upgrades slowly (5 s sustained above threshold) to avoid
/// oscillation.
pub struct AdaptiveBitrateController {
    /// Current index into `BitrateProfile::LADDER` (0 = HIGH, 1 = MEDIUM, 2 = LOW).
    current_tier: usize,
    /// Exponentially-weighted moving average of the network score.
    ewma_score: f64,
    /// When the current tier was entered (for hysteresis timing).
    tier_entered_at: Instant,
    /// How long the score must exceed the upgrade threshold before moving up.
    upgrade_stability: Duration,
    /// How long the score must stay below the downgrade threshold before moving down.
    downgrade_stability: Duration,
}

// Score weights (sum = 1.0).
const LOSS_WEIGHT: f64 = 0.5;
const JITTER_WEIGHT: f64 = 0.3;
const RTT_WEIGHT: f64 = 0.2;

// Normalization ceilings.
const MAX_JITTER_MS: f64 = 200.0;
const MAX_RTT_MS: f64 = 500.0;

// EWMA smoothing factor: new sample weight.
const EWMA_ALPHA: f64 = 0.3;

// Tier thresholds.
const HIGH_THRESHOLD: f64 = 0.8;
const MEDIUM_THRESHOLD: f64 = 0.4;

// Tier indices.
const TIER_HIGH: usize = 0;
const TIER_MEDIUM: usize = 1;
const TIER_LOW: usize = 2;

impl AdaptiveBitrateController {
    /// Create a new controller starting at MEDIUM tier.
    #[must_use]
    pub fn new() -> Self {
        Self {
            current_tier: TIER_MEDIUM,
            ewma_score: 0.6, // neutral starting score (in MEDIUM band)
            tier_entered_at: Instant::now(),
            upgrade_stability: Duration::from_secs(5),
            downgrade_stability: Duration::from_secs(1),
        }
    }

    /// Process RTCP feedback and potentially change the quality tier.
    ///
    /// Returns `Some(profile)` if the tier changed, `None` otherwise.
    pub fn on_rtcp_feedback(&mut self, feedback: &RtcpFeedback) -> Option<BitrateProfile> {
        let raw_score = Self::compute_score(feedback);
        self.ewma_score = (1.0 - EWMA_ALPHA).mul_add(self.ewma_score, EWMA_ALPHA * raw_score);

        let desired_tier = Self::score_to_tier(self.ewma_score);
        let elapsed = self.tier_entered_at.elapsed();

        match desired_tier.cmp(&self.current_tier) {
            std::cmp::Ordering::Less => {
                // Upgrade (lower index = higher quality). Require sustained stability.
                if elapsed >= self.upgrade_stability {
                    self.current_tier = desired_tier;
                    self.tier_entered_at = Instant::now();
                    tracing::info!(
                        tier = Self::tier_name(self.current_tier),
                        ewma = self.ewma_score,
                        "adaptive bitrate: upgraded"
                    );
                    return Some(self.current_profile());
                }
            }
            std::cmp::Ordering::Greater => {
                // Downgrade (higher index = lower quality). React quickly.
                if elapsed >= self.downgrade_stability {
                    self.current_tier = desired_tier;
                    self.tier_entered_at = Instant::now();
                    tracing::info!(
                        tier = Self::tier_name(self.current_tier),
                        ewma = self.ewma_score,
                        "adaptive bitrate: downgraded"
                    );
                    return Some(self.current_profile());
                }
            }
            std::cmp::Ordering::Equal => {
                // Desired tier matches current -- reset the clock so hysteresis
                // only counts *continuous* time in a different band.
                self.tier_entered_at = Instant::now();
            }
        }

        None
    }

    /// The current bitrate profile.
    #[must_use]
    pub const fn current_profile(&self) -> BitrateProfile {
        BitrateProfile::LADDER[self.current_tier]
    }

    /// The current EWMA network quality score (0.0 = terrible, 1.0 = perfect).
    #[must_use]
    pub const fn network_score(&self) -> f64 {
        self.ewma_score
    }

    /// Compute a raw network score from a single RTCP feedback sample.
    fn compute_score(feedback: &RtcpFeedback) -> f64 {
        let normalized_jitter = (feedback.jitter_ms / MAX_JITTER_MS).min(1.0);
        let normalized_rtt = (feedback.rtt_ms / MAX_RTT_MS).min(1.0);
        let loss = feedback.fraction_lost.clamp(0.0, 1.0);

        let score = 1.0
            - RTT_WEIGHT.mul_add(
                normalized_rtt,
                LOSS_WEIGHT.mul_add(loss, JITTER_WEIGHT * normalized_jitter),
            );
        score.clamp(0.0, 1.0)
    }

    /// Map a score to a tier index.
    const fn score_to_tier(score: f64) -> usize {
        if score > HIGH_THRESHOLD {
            TIER_HIGH
        } else if score > MEDIUM_THRESHOLD {
            TIER_MEDIUM
        } else {
            TIER_LOW
        }
    }

    const fn tier_name(tier: usize) -> &'static str {
        match tier {
            TIER_HIGH => "HIGH",
            TIER_MEDIUM => "MEDIUM",
            TIER_LOW => "LOW",
            _ => "UNKNOWN",
        }
    }

    /// Override hysteresis durations (for testing).
    #[doc(hidden)]
    #[must_use]
    pub const fn with_stability(mut self, upgrade: Duration, downgrade: Duration) -> Self {
        self.upgrade_stability = upgrade;
        self.downgrade_stability = downgrade;
        self
    }
}

impl Default for AdaptiveBitrateController {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perfect_feedback() -> RtcpFeedback {
        RtcpFeedback {
            fraction_lost: 0.0,
            jitter_ms: 5.0,
            rtt_ms: 10.0,
        }
    }

    fn terrible_feedback() -> RtcpFeedback {
        RtcpFeedback {
            fraction_lost: 0.5,
            jitter_ms: 150.0,
            rtt_ms: 400.0,
        }
    }

    #[test]
    fn initial_tier_is_medium() {
        let ctrl = AdaptiveBitrateController::new();
        assert_eq!(ctrl.current_profile(), BitrateProfile::MEDIUM);
    }

    #[test]
    fn good_network_upgrades_to_high() {
        // Use instant hysteresis for the test: 0ms upgrade stability.
        let mut ctrl = AdaptiveBitrateController::new().with_stability(Duration::ZERO, Duration::from_secs(1));

        let feedback = perfect_feedback();

        // Feed enough good samples to push EWMA above HIGH_THRESHOLD (0.8).
        // perfect_feedback score ~ 1.0 - (0.5*0 + 0.3*(5/200) + 0.2*(10/500))
        //                        = 1.0 - (0 + 0.0075 + 0.004) = 0.9885
        // Starting EWMA = 0.6. After N samples: ewma = 0.7*ewma + 0.3*0.9885
        // Need ewma > 0.8.
        // After 1: 0.7*0.6 + 0.3*0.9885 = 0.42 + 0.2966 = 0.7166
        // After 2: 0.7*0.7166 + 0.3*0.9885 = 0.5016 + 0.2966 = 0.7982
        // After 3: 0.7*0.7982 + 0.3*0.9885 = 0.5587 + 0.2966 = 0.8553 -> above 0.8
        for _ in 0..3 {
            ctrl.on_rtcp_feedback(&feedback);
        }

        assert_eq!(
            ctrl.current_profile(),
            BitrateProfile::HIGH,
            "should have upgraded to HIGH after sustained good feedback"
        );
    }

    #[test]
    fn bad_network_downgrades_to_low() {
        // Use instant hysteresis for the test: 0ms downgrade stability.
        let mut ctrl = AdaptiveBitrateController::new().with_stability(Duration::from_secs(5), Duration::ZERO);

        let feedback = terrible_feedback();

        // terrible_feedback score ~ 1.0 - (0.5*0.5 + 0.3*(150/200) + 0.2*(400/500))
        //                         = 1.0 - (0.25 + 0.225 + 0.16) = 0.365
        // Starting EWMA = 0.6. After N samples:
        // After 1: 0.7*0.6 + 0.3*0.365 = 0.42 + 0.1095 = 0.5295
        // After 2: 0.7*0.5295 + 0.3*0.365 = 0.3707 + 0.1095 = 0.4802
        // After 3: 0.7*0.4802 + 0.3*0.365 = 0.3361 + 0.1095 = 0.4456
        // After 4: 0.7*0.4456 + 0.3*0.365 = 0.3119 + 0.1095 = 0.4214
        // After 5: 0.7*0.4214 + 0.3*0.365 = 0.2950 + 0.1095 = 0.4045
        // After 6: 0.7*0.4045 + 0.3*0.365 = 0.2832 + 0.1095 = 0.3927 -> below 0.4
        for _ in 0..6 {
            ctrl.on_rtcp_feedback(&feedback);
        }

        assert_eq!(
            ctrl.current_profile(),
            BitrateProfile::LOW,
            "should have downgraded to LOW after sustained bad feedback"
        );
    }

    #[test]
    fn hysteresis_prevents_oscillation() {
        // Use real-ish hysteresis: upgrades require 5s, downgrades require 1s.
        // Since we don't actually wait, alternating good/bad won't trigger tier changes.
        let mut ctrl = AdaptiveBitrateController::new();

        let good = perfect_feedback();
        let bad = terrible_feedback();

        // Alternate good and bad. The EWMA will hover around the middle, and the
        // hysteresis timer will reset each time the desired tier matches the current.
        // This should keep us at MEDIUM.
        for _ in 0..20 {
            ctrl.on_rtcp_feedback(&good);
            ctrl.on_rtcp_feedback(&bad);
        }

        assert_eq!(
            ctrl.current_profile(),
            BitrateProfile::MEDIUM,
            "alternating good/bad should stay at MEDIUM due to hysteresis"
        );
    }
}

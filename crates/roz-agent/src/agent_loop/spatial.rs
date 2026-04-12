//! Spatial-context helpers used by [`AgentLoop`](super::AgentLoop) when injecting
//! world-state observations into the model conversation.
//!
//! Both helpers are `#[doc(hidden)] pub` (per accepted deviation #8) so the
//! integration test crate added in Plan 12-02 can reach them while keeping
//! them out of the public rustdoc.

use crate::model::types::Message;
use roz_core::spatial::WorldState;

/// Format a [`WorldState`] snapshot as a human-readable observation string.
#[doc(hidden)]
pub fn format_spatial_context(ctx: &WorldState) -> String {
    use std::fmt::Write;

    let mut lines = Vec::new();

    for entity in &ctx.entities {
        let mut desc = format!("Entity '{}' ({})", entity.id, entity.kind);
        if let Some([x, y, z, ..]) = entity.position {
            let _ = write!(desc, " at [{x:.2}, {y:.2}, {z:.2}]");
        }
        if let Some([vx, vy, vz, ..]) = entity.velocity {
            let _ = write!(desc, " vel=[{vx:.2}, {vy:.2}, {vz:.2}]");
        }
        lines.push(desc);
    }

    for alert in &ctx.alerts {
        lines.push(format!(
            "ALERT [{:?}]: {} ({})",
            alert.severity, alert.message, alert.source
        ));
    }

    for constraint in &ctx.constraints {
        if constraint.active {
            lines.push(format!("Constraint [{}]: {}", constraint.name, constraint.description));
        }
    }

    if lines.is_empty() {
        "No spatial observations.".to_string()
    } else {
        lines.join("\n")
    }
}

/// Build the observation message for spatial context injection.
///
/// When screenshots are present, returns a user message with both text and
/// image content blocks (Anthropic requires images in user messages).
/// Otherwise returns a system message with text-only observation.
#[doc(hidden)]
pub fn build_spatial_observation(ctx: &WorldState) -> Message {
    let formatted = format_spatial_context(ctx);
    let observation_text = format!("[Spatial Observation]\n{formatted}");
    if ctx.screenshots.is_empty() {
        Message::system(observation_text)
    } else {
        let images: Vec<(String, String)> = ctx
            .screenshots
            .iter()
            .map(|s| (s.media_type.clone(), s.data.clone()))
            .collect();
        Message::user_with_images(observation_text, images)
    }
}

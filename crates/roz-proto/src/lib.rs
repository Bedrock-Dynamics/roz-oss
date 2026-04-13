//! Client-only roz.v1 gRPC surface.
//!
//! This crate exposes generated tonic client stubs for the `roz.v1` services
//! plus thin async helpers for use by external clients (substrate-ide, CLI,
//! future tools). It intentionally has NO server dependencies (sqlx, axum,
//! restate-sdk, reqwest) so clients can link it without dragging the full
//! `roz-server` dep graph.

#![deny(unsafe_code)]

/// Generated prost/tonic types for the `roz.v1` proto package.
#[allow(
    clippy::default_trait_access,
    clippy::derive_partial_eq_without_eq,
    clippy::doc_markdown,
    clippy::enum_variant_names,
    clippy::missing_const_for_fn,
    clippy::too_long_first_doc_paragraph,
    clippy::too_many_lines,
    clippy::wildcard_imports
)]
pub mod roz_v1 {
    tonic::include_proto!("roz.v1");
}

// Re-exports — primary client surface.
pub use roz_v1::agent_service_client::AgentServiceClient;
pub use roz_v1::{
    AnalyzeMediaChunk, AnalyzeMediaRequest, AudioHints, Done, ImageHints, MediaPart, ModalityHints, Usage, VideoHints,
};

use futures::Stream;

/// Helper: call `AnalyzeMedia` and return the response stream.
///
/// Per D-18: thin wrapper — substrate-ide and CLI collect `TextDelta` chunks
/// themselves. No builder API in v1.
///
/// # Errors
/// Returns `tonic::Status` if the initial RPC dispatch fails. Per-chunk errors
/// are delivered via the returned stream's `Result` items.
pub async fn analyze_media_stream(
    client: &mut AgentServiceClient<tonic::transport::Channel>,
    part: MediaPart,
    prompt: String,
    model_hint: Option<String>,
) -> Result<impl Stream<Item = Result<AnalyzeMediaChunk, tonic::Status>>, tonic::Status> {
    let request = AnalyzeMediaRequest {
        media: Some(part),
        prompt,
        model_hint,
    };
    let response = client.analyze_media(request).await?;
    Ok(response.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_reexport_compile() {
        // Compile-time assertion that the primary types are reachable.
        let _ = std::mem::size_of::<AnalyzeMediaRequest>();
        let _ = std::mem::size_of::<MediaPart>();
        let _ = std::mem::size_of::<AnalyzeMediaChunk>();
        let _ = std::mem::size_of::<Usage>();
        let _ = std::mem::size_of::<Done>();
        let _ = std::mem::size_of::<VideoHints>();
        let _ = std::mem::size_of::<AudioHints>();
        let _ = std::mem::size_of::<ImageHints>();
    }
}

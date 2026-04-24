//! Phase 26.7 SC2: `ArtifactService` gRPC implementation — TDD RED.
//!
//! This file currently contains only failing tests; the `ArtifactServiceImpl`
//! module is intentionally absent until the GREEN task replaces it.

#[cfg(test)]
mod tests {
    use super::super::artifacts::*;

    #[test]
    fn extension_for_maps_artifact_type_to_on_disk_extension() {
        assert_eq!(ArtifactServiceImpl::extension_for("copper"), "copper");
        assert_eq!(ArtifactServiceImpl::extension_for("ulog"), "ulg");
        assert_eq!(ArtifactServiceImpl::extension_for("video"), "mp4");
        assert_eq!(ArtifactServiceImpl::extension_for("bundle"), "tar");
        assert_eq!(ArtifactServiceImpl::extension_for("weird"), "bin");
    }

    #[test]
    fn content_type_is_allowed_rejects_mcap_this_phase() {
        // D-03: 'mcap' is reserved in DB CHECK enum but MUST NOT be written
        // by ArtifactService this phase.
        assert!(!ArtifactServiceImpl::content_type_is_allowed_this_phase("mcap"));
        assert!(ArtifactServiceImpl::content_type_is_allowed_this_phase("copper"));
        assert!(ArtifactServiceImpl::content_type_is_allowed_this_phase("ulog"));
        assert!(ArtifactServiceImpl::content_type_is_allowed_this_phase("video"));
        assert!(ArtifactServiceImpl::content_type_is_allowed_this_phase("bundle"));
        assert!(!ArtifactServiceImpl::content_type_is_allowed_this_phase("garbage"));
    }

    #[test]
    fn upload_chunk_size_is_one_mib() {
        assert_eq!(UPLOAD_CHUNK_SIZE, 1024 * 1024);
    }
}

# Vendored Foxglove Schemas

These `.proto` files are vendored verbatim from upstream Foxglove SDK and
supply the raw `FileDescriptorSet` bytes that
[`mcap::Writer::add_schema`](https://docs.rs/mcap) consumes when registering
the Foxglove-native channels exposed by Phase 26 unified MCAP observability
(`/tf`, `/roz/telemetry/pose`, `/roz/log`) and the Phase 26.5 multimedia
channels (camera frames, point clouds, scene updates, image annotations).

## Source

- **Upstream repo:** <https://github.com/foxglove/foxglove-sdk>
- **Snapshot path:** `schemas/proto/foxglove/`
- **Commit hash:** `7c7a179e32eff0855f94b83d177affcc1709ee32`
- **Snapshot URL:** <https://github.com/foxglove/foxglove-sdk/tree/7c7a179e32eff0855f94b83d177affcc1709ee32/schemas/proto/foxglove>

## Files

| File | Role |
| --- | --- |
| `FrameTransform.proto` | Target schema for `/tf` channel (Phase 26 OBS-02). |
| `PoseInFrame.proto` | Target schema for `/roz/telemetry/pose` channel. |
| `Log.proto` | Target schema for `/roz/log` unified text timeline. |
| `Pose.proto` | Transitive dep of `PoseInFrame`. |
| `Quaternion.proto` | Transitive dep of `FrameTransform` and `Pose`. |
| `Vector3.proto` | Transitive dep of `FrameTransform` and `Pose`. |
| `CompressedVideo.proto` | Target schema for `/roz/camera/{camera_id}` H.264 channels (Phase 26.5 R-01). |
| `CompressedImage.proto` | Schema-only vendor for future JPEG/PNG/WEBP/AVIF camera paths (Phase 26.5; no producer this phase per R-01). |
| `RawImage.proto` | Schema-only vendor for future raw-frame paths (Phase 26.5). |
| `PointCloud.proto` | Target schema for `/roz/perception/pointcloud` channel (Phase 26.5; producer in Phase 29+). |
| `SceneUpdate.proto` | Target schema for `/roz/perception/scene_update` channel (Phase 26.5; producer in Phase 29+). |
| `ImageAnnotations.proto` | Target schema for `/roz/perception/annotations` channel (Phase 26.5; producer in Phase 29+). |
| `PackedElementField.proto` | Transitive dep of `PointCloud`. |
| `SceneEntity.proto` | Transitive dep of `SceneUpdate`. |
| `SceneEntityDeletion.proto` | Transitive dep of `SceneUpdate`. |
| `ArrowPrimitive.proto` | Transitive dep of `SceneEntity`. |
| `CubePrimitive.proto` | Transitive dep of `SceneEntity`. |
| `CylinderPrimitive.proto` | Transitive dep of `SceneEntity`. |
| `LinePrimitive.proto` | Transitive dep of `SceneEntity`. |
| `ModelPrimitive.proto` | Transitive dep of `SceneEntity`. |
| `SpherePrimitive.proto` | Transitive dep of `SceneEntity`. |
| `TextPrimitive.proto` | Transitive dep of `SceneEntity`. |
| `TriangleListPrimitive.proto` | Transitive dep of `SceneEntity`. |
| `CircleAnnotation.proto` | Transitive dep of `ImageAnnotations`. |
| `PointsAnnotation.proto` | Transitive dep of `ImageAnnotations`. |
| `TextAnnotation.proto` | Transitive dep of `ImageAnnotations`. |
| `KeyValuePair.proto` | Transitive dep of `SceneEntity` metadata. |
| `Color.proto` | Transitive dep of `SceneEntity` primitives and annotations. |
| `Point2.proto` | Transitive dep of `ImageAnnotations`. |
| `Point3.proto` | Transitive dep of `LinePrimitive` and `TriangleListPrimitive`. |

> Phase 26.5 additions (2026-04-23): 24 new files — CompressedVideo family + PointCloud/SceneUpdate/ImageAnnotations transitive closure.

### Note on `LogLevel`

Phase 26 Plan 26-01 originally listed a seventh file (`LogLevel.proto`).
Upstream foxglove-sdk at the pinned commit declares the log severity enum
inline as `enum Level` inside `Log.proto` — there is no standalone
`LogLevel.proto` in `schemas/proto/foxglove/`. To preserve the verbatim
vendoring contract (see "Rule" below) the file list reflects upstream
reality: six files, not seven. The severity values remain available at the
qualified name `foxglove.Log.Level`.

## Rule

**DO NOT EDIT** these files. They are the upstream source of truth for the
Foxglove Studio rendering contract; any drift breaks 3D-panel + timeline
compatibility. Re-vendor by re-running the procedure below if Foxglove
releases a compatible update.

## Re-vendor procedure

```bash
# Use `-c filter.lfs.*=` so clone succeeds without git-lfs installed —
# the proto files are plain text and are not LFS-managed.
git -c filter.lfs.smudge= -c filter.lfs.clean= \
    -c filter.lfs.process= -c filter.lfs.required=false \
    clone --depth=1 https://github.com/foxglove/foxglove-sdk /tmp/foxglove-sdk
cd /tmp/foxglove-sdk && git rev-parse HEAD  # capture new commit hash

cp /tmp/foxglove-sdk/schemas/proto/foxglove/{FrameTransform,PoseInFrame,Log,Pose,Quaternion,Vector3}.proto \
    $REPO_ROOT/proto/foxglove/

# Phase 26.5 additions — camera + perception schemas:
cp /tmp/foxglove-sdk/schemas/proto/foxglove/{CompressedVideo,CompressedImage,RawImage,PointCloud,SceneUpdate,ImageAnnotations}.proto \
    $REPO_ROOT/proto/foxglove/
cp /tmp/foxglove-sdk/schemas/proto/foxglove/{PackedElementField,SceneEntity,SceneEntityDeletion}.proto \
    $REPO_ROOT/proto/foxglove/
cp /tmp/foxglove-sdk/schemas/proto/foxglove/{ArrowPrimitive,CubePrimitive,CylinderPrimitive,LinePrimitive,ModelPrimitive,SpherePrimitive,TextPrimitive,TriangleListPrimitive}.proto \
    $REPO_ROOT/proto/foxglove/
cp /tmp/foxglove-sdk/schemas/proto/foxglove/{CircleAnnotation,PointsAnnotation,TextAnnotation}.proto \
    $REPO_ROOT/proto/foxglove/
cp /tmp/foxglove-sdk/schemas/proto/foxglove/{KeyValuePair,Color,Point2,Point3}.proto \
    $REPO_ROOT/proto/foxglove/

# Update the commit hash above, then:
# - re-run `cargo build -p roz-server` (regenerates foxglove_descriptor.bin)
# - run the full Phase 26 OBS verification suite before merging
```

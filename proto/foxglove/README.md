# Vendored Foxglove Schemas

These `.proto` files are vendored verbatim from upstream Foxglove SDK and
supply the raw `FileDescriptorSet` bytes that
[`mcap::Writer::add_schema`](https://docs.rs/mcap) consumes when registering
the Foxglove-native channels exposed by Phase 26 unified MCAP observability
(`/tf`, `/roz/telemetry/pose`, `/roz/log`).

## Source

- **Upstream repo:** <https://github.com/foxglove/foxglove-sdk>
- **Snapshot path:** `schemas/proto/foxglove/`
- **Commit hash:** `9c5983956b7601c6a91d6908d88861563e8ef305`
- **Snapshot URL:** <https://github.com/foxglove/foxglove-sdk/tree/9c5983956b7601c6a91d6908d88861563e8ef305/schemas/proto/foxglove>

## Files

| File | Role |
| --- | --- |
| `FrameTransform.proto` | Target schema for `/tf` channel (Phase 26 OBS-02). |
| `PoseInFrame.proto` | Target schema for `/roz/telemetry/pose` channel. |
| `Log.proto` | Target schema for `/roz/log` unified text timeline. |
| `Pose.proto` | Transitive dep of `PoseInFrame`. |
| `Quaternion.proto` | Transitive dep of `FrameTransform` and `Pose`. |
| `Vector3.proto` | Transitive dep of `FrameTransform` and `Pose`. |

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

# Update the commit hash above, then:
# - re-run `cargo build -p roz-server` (regenerates foxglove_descriptor.bin)
# - run the full Phase 26 OBS verification suite before merging
```

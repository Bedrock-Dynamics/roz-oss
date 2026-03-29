//! I420 (YUV 4:2:0 planar) to JPEG frame conversion.
//!
//! Used by the snapshot feeder to convert raw camera frames into JPEG
//! images that can be injected into the agent's spatial context as
//! base64-encoded screenshots.

use super::source::RawFrame;

/// Convert an I420 raw frame to a JPEG image at the specified target resolution.
///
/// 1. Extracts Y, U, V planes from the I420 data
/// 2. Converts YUV to RGB using BT.601 coefficients
/// 3. Resizes to `target_width x target_height` if different from source
/// 4. Encodes as JPEG at the given quality (1-100)
///
/// Returns the JPEG bytes.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn i420_to_jpeg(raw: &RawFrame, target_width: u32, target_height: u32, quality: u8) -> anyhow::Result<Vec<u8>> {
    let width = raw.width as usize;
    let height = raw.height as usize;
    let y_size = width * height;
    let uv_size = (width / 2) * (height / 2);
    let expected_len = y_size + 2 * uv_size;

    anyhow::ensure!(
        raw.data.len() >= expected_len,
        "I420 buffer too small: expected at least {expected_len} bytes, got {}",
        raw.data.len()
    );

    let uv_stride = width / 2;

    // Convert I420 to RGB using BT.601 coefficients
    let mut rgb = vec![0u8; width * height * 3];
    for row in 0..height {
        for col in 0..width {
            let luma = f32::from(raw.data[row * width + col]);
            let uv_row = row / 2;
            let uv_col = col / 2;
            let cb = f32::from(raw.data[y_size + uv_row * uv_stride + uv_col]) - 128.0;
            let cr = f32::from(raw.data[y_size + y_size / 4 + uv_row * uv_stride + uv_col]) - 128.0;

            let red = 1.402_f32.mul_add(cr, luma).clamp(0.0, 255.0) as u8;
            let green = 0.714_136_f32
                .mul_add(-cr, 0.344_136_f32.mul_add(-cb, luma))
                .clamp(0.0, 255.0) as u8;
            let blue = 1.772_f32.mul_add(cb, luma).clamp(0.0, 255.0) as u8;

            let idx = (row * width + col) * 3;
            rgb[idx] = red;
            rgb[idx + 1] = green;
            rgb[idx + 2] = blue;
        }
    }

    // Build an image buffer and resize if needed
    let img = image::RgbImage::from_raw(raw.width, raw.height, rgb)
        .ok_or_else(|| anyhow::anyhow!("RGB buffer size mismatch"))?;

    let resized = if raw.width != target_width || raw.height != target_height {
        image::imageops::resize(&img, target_width, target_height, image::imageops::FilterType::Triangle)
    } else {
        image::ImageBuffer::from_raw(target_width, target_height, img.into_raw())
            .ok_or_else(|| anyhow::anyhow!("resize buffer size mismatch"))?
    };

    // Encode to JPEG
    let mut jpeg_buf = std::io::Cursor::new(Vec::new());
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg_buf, quality);
    image::ImageEncoder::write_image(
        encoder,
        resized.as_raw(),
        target_width,
        target_height,
        image::ExtendedColorType::Rgb8,
    )
    .map_err(|e| anyhow::anyhow!("JPEG encoding failed: {e}"))?;

    Ok(jpeg_buf.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::source::{RawFrame, TestPatternSource};
    use roz_core::camera::CameraId;

    #[test]
    fn i420_to_jpeg_produces_valid_jpeg() {
        let data = TestPatternSource::generate_frame(320, 240, 0);
        let frame = RawFrame {
            camera_id: CameraId::new("test"),
            width: 320,
            height: 240,
            data,
            timestamp_us: 0,
            seq: 0,
        };

        let jpeg = i420_to_jpeg(&frame, 320, 240, 80).expect("conversion should succeed");

        // JPEG magic bytes
        assert!(jpeg.len() > 2, "JPEG too small");
        assert_eq!(jpeg[0], 0xFF, "missing JPEG SOI marker");
        assert_eq!(jpeg[1], 0xD8, "missing JPEG SOI marker");
    }

    #[test]
    fn i420_to_jpeg_resizes() {
        let data = TestPatternSource::generate_frame(640, 480, 0);
        let frame = RawFrame {
            camera_id: CameraId::new("test"),
            width: 640,
            height: 480,
            data,
            timestamp_us: 0,
            seq: 0,
        };

        let jpeg = i420_to_jpeg(&frame, 160, 120, 60).expect("conversion should succeed");

        // Verify it produced valid JPEG (resize happened internally)
        assert_eq!(jpeg[0], 0xFF);
        assert_eq!(jpeg[1], 0xD8);
        // Resized JPEG should be smaller than full-resolution
        let full_jpeg = i420_to_jpeg(&frame, 640, 480, 60).expect("conversion should succeed");
        assert!(jpeg.len() < full_jpeg.len(), "resized should be smaller");
    }
}

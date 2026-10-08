//! Uploaded avatar pictures. A picture is decoded and drawn again before it is
//! stored, so what the control plane serves is a small PNG it wrote itself:
//! nothing the upload carried besides its pixels survives, metadata included.

use image::ImageFormat;
use image::imageops::FilterType;

/// The side of the square every stored picture is drawn at.
pub const AVATAR_SIDE: u32 = 256;

/// The largest upload accepted, before decoding.
pub const AVATAR_UPLOAD_MAX_BYTES: usize = 5 * 1024 * 1024;

/// The largest side a decoded picture may have. A small file can declare a
/// huge image, so the decoder is limited before it allocates.
const DECODE_MAX_SIDE: u32 = 8192;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AvatarError {
    #[error("the picture must be a PNG, JPEG or WebP image")]
    Format,
    #[error("the picture could not be read: {0}")]
    Decode(String),
}

/// Decodes a PNG, JPEG or WebP upload, crops it to a centred square, scales
/// it to `AVATAR_SIDE`, and encodes it as PNG. Any other format, SVG
/// included, is refused: an SVG can carry script.
pub fn normalize_picture(bytes: &[u8]) -> Result<Vec<u8>, AvatarError> {
    let format = image::guess_format(bytes).map_err(|_| AvatarError::Format)?;
    if !matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP
    ) {
        return Err(AvatarError::Format);
    }
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(DECODE_MAX_SIDE);
    limits.max_image_height = Some(DECODE_MAX_SIDE);
    reader.limits(limits);
    let picture = reader
        .decode()
        .map_err(|error| AvatarError::Decode(error.to_string()))?;
    let square = picture.resize_to_fill(AVATAR_SIDE, AVATAR_SIDE, FilterType::Lanczos3);
    let mut png = Vec::new();
    square
        .write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png)
        .map_err(|error| AvatarError::Decode(error.to_string()))?;
    Ok(png)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(width: u32, height: u32, format: ImageFormat) -> Vec<u8> {
        let picture = image::DynamicImage::new_rgb8(width, height);
        let mut bytes = Vec::new();
        picture
            .write_to(&mut std::io::Cursor::new(&mut bytes), format)
            .unwrap();
        bytes
    }

    #[test]
    fn a_picture_is_stored_as_a_square_png_of_the_avatar_size() {
        for format in [ImageFormat::Png, ImageFormat::Jpeg] {
            let png = normalize_picture(&encoded(640, 480, format)).unwrap();
            let stored = image::load_from_memory_with_format(&png, ImageFormat::Png).unwrap();
            assert_eq!(
                (stored.width(), stored.height()),
                (AVATAR_SIDE, AVATAR_SIDE)
            );
        }
    }

    #[test]
    fn an_svg_or_anything_else_is_refused() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg"><script>alert(1)</script></svg>"#;
        assert_eq!(normalize_picture(svg), Err(AvatarError::Format));
        assert_eq!(
            normalize_picture(b"not a picture"),
            Err(AvatarError::Format)
        );
    }

    #[test]
    fn a_truncated_picture_fails_to_decode() {
        let png = encoded(64, 64, ImageFormat::Png);
        assert!(matches!(
            normalize_picture(&png[..png.len() / 2]),
            Err(AvatarError::Decode(_))
        ));
    }
}

use std::fs::File;
use std::io::{Cursor, Read as _};

use image::{DynamicImage, ImageDecoder as _, ImageFormat, ImageReader, Limits};

use crate::ui::raster_preview::{RasterPreview, raster_size};

const MAX_IMAGE_BYTES: u64 = 32 << 20;

pub(super) fn format(extension: &str) -> Option<ImageFormat> {
    match extension {
        "png" => Some(ImageFormat::Png),
        "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
        "gif" => Some(ImageFormat::Gif),
        "webp" => Some(ImageFormat::WebP),
        "bmp" => Some(ImageFormat::Bmp),
        _ => None,
    }
}

pub(super) fn load(file: File, format: ImageFormat) -> Result<RasterPreview, String> {
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err("image previews are limited to 32 MB".into());
    }
    let reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut decoder = reader.into_decoder().map_err(|error| error.to_string())?;
    Limits::default()
        .reserve(decoder.total_bytes())
        .map_err(|error| error.to_string())?;
    let orientation = decoder.orientation().map_err(|error| error.to_string())?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(|error| error.to_string())?;
    image.apply_orientation(orientation);
    let mut pixels = image.into_rgba8();
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    let size = raster_size(pixels.width(), pixels.height());
    RasterPreview::new(pixels, size)
}

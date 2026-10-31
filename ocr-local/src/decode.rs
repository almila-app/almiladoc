use std::io::Cursor;

use image::codecs::png::PngDecoder;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, RgbImage};

use crate::{Error, Limits};

const RGB_BYTES_PER_PIXEL: u64 = 3;
const RGBA_BYTES_PER_PIXEL: u64 = 4;

pub(crate) fn buffer_bytes(pixels: u64, source_bytes: u64) -> Option<u64> {
    pixels.checked_mul(RGBA_BYTES_PER_PIXEL + RGB_BYTES_PER_PIXEL)?.checked_add(source_bytes)
}

pub(crate) fn decode(bytes: &[u8], bounds: &Limits) -> Result<RgbImage, Error> {
    decode_with_cancel(bytes, bounds, &|| false)
}

pub(crate) fn decode_with_cancel(
    bytes: &[u8],
    bounds: &Limits,
    cancelled: &(impl Fn() -> bool + Sync),
) -> Result<RgbImage, Error> {
    crate::checkpoint(cancelled)?;
    if bytes.len() > bounds.max_image_bytes {
        return Err(Error::ImageLimit);
    }
    let format = image::guess_format(bytes).map_err(|_| Error::ImageFormat)?;
    if !matches!(format, ImageFormat::Png | ImageFormat::Jpeg) {
        return Err(Error::ImageFormat);
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(bounds.max_dimension);
    limits.max_image_height = Some(bounds.max_dimension);
    limits.max_alloc = Some(bounds.max_decode_bytes);
    if format == ImageFormat::Png {
        let decoder =
            PngDecoder::with_limits(Cursor::new(bytes), limits).map_err(|e| image_error(&e))?;
        if decoder.is_apng().map_err(|e| image_error(&e))? {
            return Err(Error::ImageFormat);
        }
        finish(decoder, bounds, cancelled)
    } else {
        let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
        reader.limits(limits);
        finish(reader.into_decoder().map_err(|e| image_error(&e))?, bounds, cancelled)
    }
}

fn finish(
    decoder: impl ImageDecoder,
    bounds: &Limits,
    cancelled: &(impl Fn() -> bool + Sync),
) -> Result<RgbImage, Error> {
    let (width, height) = decoder.dimensions();
    let pixels = u64::from(width) * u64::from(height);
    let buffers = buffer_bytes(pixels, decoder.total_bytes());
    if pixels == 0
        || pixels > bounds.max_pixels
        || buffers.is_none_or(|bytes| bytes > bounds.max_decode_bytes)
    {
        return Err(Error::ImageLimit);
    }
    let rgba = DynamicImage::from_decoder(decoder).map_err(|e| image_error(&e))?.into_rgba8();
    crate::checkpoint(cancelled)?;
    let mut rgb = RgbImage::new(width, height);
    for (index, (source, destination)) in rgba.pixels().zip(rgb.pixels_mut()).enumerate() {
        if index % 4096 == 0 {
            crate::checkpoint(cancelled)?;
        }
        let alpha = u16::from(source[3]);
        for (channel, target) in source.0[..3].iter().zip(destination.0.iter_mut()) {
            let value = (u16::from(*channel) * alpha + 255 * (255 - alpha) + 127) / 255;
            *target = u8::try_from(value).map_err(|_| Error::ImageDecode)?;
        }
    }
    Ok(rgb)
}

fn image_error(error: &image::ImageError) -> Error {
    match error {
        image::ImageError::Limits(_) => Error::ImageLimit,
        _ => Error::ImageDecode,
    }
}

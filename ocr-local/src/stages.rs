use std::sync::Arc;

use image::RgbImage;
use oar_ocr_core::domain::TextRegion;
use oar_ocr_core::models::detection::db::DBModelOutput;
use oar_ocr_core::models::recognition::crnn::CRNNModelOutput;
use oar_ocr_core::processors::{BoundingBox, Point};
use oar_ocr_core::utils::get_rotate_crop_image;

use crate::{Error, Limits, Page, output};

pub(crate) fn validate_batch_shape(shape: &[usize], channels: usize) -> Result<(), Error> {
    match shape {
        [1, actual, height, width] if *actual == channels && *height > 0 && *width > 0 => Ok(()),
        _ => Err(Error::InvalidOutput),
    }
}

pub(crate) fn validate_heatmap(
    shape: &[usize],
    values: impl IntoIterator<Item = f32>,
) -> Result<(), Error> {
    validate_batch_shape(shape, 1)?;
    validate_values(shape, values, |value| value.is_finite() && (0.0..=1.0).contains(&value))
}

pub(crate) fn validate_recognition(
    shape: &[usize],
    values: impl IntoIterator<Item = f32>,
    vocabulary: usize,
) -> Result<(), Error> {
    match shape {
        [1, time, classes] if *time > 0 && *classes == vocabulary && vocabulary > 0 => {}
        _ => return Err(Error::InvalidOutput),
    }
    validate_values(shape, values, f32::is_finite)
}

fn validate_values(
    shape: &[usize],
    values: impl IntoIterator<Item = f32>,
    accepts: impl Fn(f32) -> bool,
) -> Result<(), Error> {
    let expected = shape
        .iter()
        .try_fold(1usize, |count, &dimension| count.checked_mul(dimension))
        .ok_or(Error::InvalidOutput)?;
    let mut count = 0usize;
    for value in values {
        if !accepts(value) {
            return Err(Error::InvalidOutput);
        }
        count = count.checked_add(1).ok_or(Error::InvalidOutput)?;
    }
    if count != expected {
        return Err(Error::InvalidOutput);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn recognize_regions(
    image: &Arc<RgbImage>,
    detected: DBModelOutput,
    limits: &Limits,
    recognize: impl FnMut(&RgbImage) -> Result<CRNNModelOutput, Error>,
) -> Result<Page, Error> {
    recognize_regions_cancelled(image, detected, limits, recognize, &|| false)
}

pub(crate) fn recognize_regions_cancelled(
    image: &Arc<RgbImage>,
    detected: DBModelOutput,
    limits: &Limits,
    mut recognize: impl FnMut(&RgbImage) -> Result<CRNNModelOutput, Error>,
    cancelled: &(impl Fn() -> bool + Sync),
) -> Result<Page, Error> {
    crate::checkpoint(cancelled)?;
    let (width, height) = image.dimensions();
    let boxes = detection_boxes(detected, width, height, limits)?;
    let regions = boxes.into_iter().map(|bbox| {
        crate::checkpoint(cancelled)?;
        let crop = crop(image, &bbox.points, limits, || {
            get_rotate_crop_image(image.as_ref(), &bbox.points).map_err(|_| Error::Recognition)
        })?;
        crate::checkpoint(cancelled)?;
        let recognized = recognize(&crop)?;
        crate::checkpoint(cancelled)?;
        let (text, score) = one_recognition(recognized)?;
        Ok(TextRegion::with_recognition(bbox, Some(Arc::from(text)), Some(score)))
    });
    output::page(regions, width, height, limits)
}

fn detection_boxes(
    detected: DBModelOutput,
    width: u32,
    height: u32,
    limits: &Limits,
) -> Result<Vec<BoundingBox>, Error> {
    let [boxes]: [_; 1] = detected.boxes.try_into().map_err(|_| Error::InvalidOutput)?;
    let [scores]: [_; 1] = detected.scores.try_into().map_err(|_| Error::InvalidOutput)?;
    if boxes.len() != scores.len() {
        return Err(Error::InvalidOutput);
    }
    if boxes.len() > limits.max_lines {
        return Err(Error::OutputLimit);
    }
    for (bbox, score) in boxes.iter().zip(scores) {
        if !score.is_finite() || !(0.0..=1.0).contains(&score) {
            return Err(Error::InvalidOutput);
        }
        output::rectangle(&bbox.points, width, height)?;
    }
    Ok(boxes)
}

pub(crate) fn crop(
    image: &RgbImage,
    points: &[Point],
    limits: &Limits,
    transform: impl FnOnce() -> Result<RgbImage, Error>,
) -> Result<RgbImage, Error> {
    let (width, height) = crop_dimensions(points, image.width(), image.height())?;
    validate_dimensions(width, height, limits)?;
    let image = transform()?;
    validate_crop(&image, limits)?;
    Ok(image)
}

pub(crate) fn validate_crop(image: &RgbImage, limits: &Limits) -> Result<(), Error> {
    let (width, height) = image.dimensions();
    validate_dimensions(width, height, limits)
}

fn validate_dimensions(width: u32, height: u32, limits: &Limits) -> Result<(), Error> {
    if width == 0 || height == 0 {
        return Err(Error::InvalidOutput);
    }
    if width > limits.max_dimension
        || height > limits.max_dimension
        || u64::from(width) * u64::from(height) > limits.max_pixels
    {
        return Err(Error::OutputLimit);
    }
    Ok(())
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "Match the pinned crop kernel's coordinate and rounded size conversions."
)]
pub(crate) fn crop_dimensions(
    points: &[Point],
    width: u32,
    height: u32,
) -> Result<(u32, u32), Error> {
    let bounds = output::rectangle(points, width, height)?;
    let (left, top) = (bounds.left as u32, bounds.top as u32);
    let (width, height) = (bounds.right as u32 - left, bounds.bottom as u32 - top);
    if width == 0 || height == 0 {
        return Err(Error::Recognition);
    }
    let mut corners: [Point; 4] = points.try_into().map_err(|_| Error::InvalidOutput)?;
    for point in &mut corners {
        point.x -= left as f32;
        point.y -= top as f32;
    }
    corners.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal));
    if corners[1].y < corners[0].y {
        corners.swap(0, 1);
    }
    if corners[3].y < corners[2].y {
        corners.swap(2, 3);
    }
    let [a, d, b, c] = corners;
    let axis = [
        Point::new(0.0, 0.0),
        Point::new(width as f32, 0.0),
        Point::new(width as f32, height as f32),
        Point::new(0.0, height as f32),
    ];
    if [a, b, c, d] == axis {
        return Ok((width, height));
    }
    let distance = |p: Point, q: Point| (p.x - q.x).hypot(p.y - q.y);
    let width = distance(a, b).max(distance(c, d)).round() as u32;
    let height = distance(a, d).max(distance(b, c)).round() as u32;
    if width == 0 || height == 0 {
        return Err(Error::Recognition);
    }
    Ok((width, height))
}

fn one_recognition(output: CRNNModelOutput) -> Result<(String, f32), Error> {
    let [text]: [_; 1] = output.texts.try_into().map_err(|_| Error::InvalidOutput)?;
    let [score]: [_; 1] = output.scores.try_into().map_err(|_| Error::InvalidOutput)?;
    if !score.is_finite() || !(0.0..=1.0).contains(&score) {
        return Err(Error::InvalidOutput);
    }
    Ok((text, score))
}

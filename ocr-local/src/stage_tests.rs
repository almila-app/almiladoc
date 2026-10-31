use std::sync::Arc;

use image::{Rgb, RgbImage};
use oar_ocr_core::models::detection::db::DBModelOutput;
use oar_ocr_core::models::recognition::crnn::CRNNModelOutput;
use oar_ocr_core::processors::BoundingBox;

use crate::{Error, Limits, stages};

fn image() -> Arc<RgbImage> {
    Arc::new(RgbImage::from_pixel(10, 10, Rgb([255, 255, 255])))
}

fn boxes() -> Vec<BoundingBox> {
    vec![BoundingBox::from_coords(1.0, 1.0, 4.0, 3.0), BoundingBox::from_coords(5.0, 1.0, 9.0, 4.0)]
}

fn detection() -> DBModelOutput {
    DBModelOutput { boxes: vec![boxes()], scores: vec![vec![0.8, 0.9]] }
}

fn recognition(texts: &[&str], scores: Vec<f32>) -> CRNNModelOutput {
    CRNNModelOutput {
        texts: texts.iter().map(|text| (*text).to_owned()).collect(),
        scores,
        char_positions: Vec::new(),
        char_col_indices: Vec::new(),
        sequence_lengths: Vec::new(),
    }
}

#[test]
fn every_detected_crop_is_recognized_once_in_detector_order() {
    let mut requested = Vec::new();
    let page = stages::recognize_regions(&image(), detection(), &Limits::default(), |crop| {
        requested.push(crop.dimensions());
        let text = match crop.dimensions() {
            (3, 2) => "first",
            (4, 3) => "second",
            other => panic!("unexpected crop dimensions {other:?}"),
        };
        Ok(recognition(&[text], vec![0.5]))
    })
    .unwrap();
    assert_eq!(requested, [(3, 2), (4, 3)]);
    assert_eq!(page.lines.iter().map(crate::Line::text).collect::<Vec<_>>(), ["first", "second"]);
    assert_eq!(page.lines[0].rectangle().left, 1.0);
    assert_eq!(page.lines[1].rectangle().left, 5.0);
}

#[test]
fn fewer_or_more_recognition_rows_refuse_even_after_a_valid_region() {
    let cases = [
        recognition(&[], vec![]),
        recognition(&[], vec![0.5]),
        recognition(&["one"], vec![]),
        recognition(&["one", "extra"], vec![0.5]),
        recognition(&["one"], vec![0.5, 0.6]),
        recognition(&["one", "extra"], vec![0.5, 0.6]),
    ];
    for malformed in cases {
        let mut remaining = vec![malformed, recognition(&["first"], vec![0.5])];
        let mut calls = 0;
        let result = stages::recognize_regions(&image(), detection(), &Limits::default(), |_| {
            calls += 1;
            Ok(remaining.pop().unwrap())
        });
        assert_eq!(result, Err(Error::InvalidOutput));
        assert_eq!(calls, 2);
    }
}

#[test]
fn omitted_or_extra_image_rows_and_unpaired_detection_scores_refuse_before_recognition() {
    let cases = [
        DBModelOutput { boxes: vec![], scores: vec![] },
        DBModelOutput { boxes: vec![boxes()], scores: vec![] },
        DBModelOutput { boxes: vec![], scores: vec![vec![0.8, 0.9]] },
        DBModelOutput { boxes: vec![boxes(), vec![]], scores: vec![vec![0.8, 0.9], vec![]] },
        DBModelOutput { boxes: vec![boxes()], scores: vec![vec![0.8]] },
        DBModelOutput { boxes: vec![boxes()], scores: vec![vec![0.8, 0.9, 1.0]] },
    ];
    for detected in cases {
        let result = stages::recognize_regions(&image(), detected, &Limits::default(), |_| {
            panic!("malformed detection must stop before recognition")
        });
        assert_eq!(result, Err(Error::InvalidOutput));
    }
}

#[test]
fn cropped_images_must_fit_each_dimension_and_pixel_bound() {
    for (width, height, limits, expected) in [
        (0, 1, Limits::default(), Error::InvalidOutput),
        (1, 0, Limits::default(), Error::InvalidOutput),
        (10, 1, Limits { max_dimension: 9, ..Limits::default() }, Error::OutputLimit),
        (1, 10, Limits { max_dimension: 9, ..Limits::default() }, Error::OutputLimit),
        (10, 10, Limits { max_pixels: 99, ..Limits::default() }, Error::OutputLimit),
    ] {
        assert_eq!(stages::validate_crop(&RgbImage::new(width, height), &limits), Err(expected));
    }
    let exact = Limits { max_dimension: 10, max_pixels: 100, ..Limits::default() };
    assert_eq!(stages::validate_crop(&image(), &exact), Ok(()));
}

#[test]
fn crop_allocation_waits_for_every_predicted_output_bound() {
    let diagonal = [(0.0, 0.0), (63.0, 62.0), (62.0, 63.0), (0.0, 1.0)];
    let tall = [(0.0, 0.0), (63.0, 0.0), (63.0, 63.0), (62.0, 63.0)];
    let thin = [(0.0, 7000.0), (2.0, 0.0), (3.0, 7000.0), (2.0, 8192.0)];
    for (points, size, limits) in [
        (diagonal, (64, 64), Limits { max_dimension: 64, ..Limits::default() }),
        (tall, (64, 64), Limits { max_dimension: 64, ..Limits::default() }),
        (tall, (64, 64), Limits { max_dimension: 100, max_pixels: 4096, ..Limits::default() }),
        (thin, (3, 8192), Limits::default()),
    ] {
        let points = points.map(|(x, y)| oar_ocr_core::processors::Point::new(x, y));
        let image = RgbImage::new(size.0, size.1);
        let mut calls = 0;
        let result = stages::crop(&image, &points, &limits, || {
            calls += 1;
            Ok(RgbImage::new(1, 1))
        });
        assert_eq!(calls, 0);
        assert_eq!(result, Err(Error::OutputLimit));
    }
}

#[test]
fn transformed_crop_is_independently_admitted_after_prediction() {
    let points = BoundingBox::from_coords(0.0, 0.0, 4.0, 3.0).points;
    for (width, height, limits, expected) in [
        (0, 1, Limits::default(), Error::InvalidOutput),
        (1, 0, Limits::default(), Error::InvalidOutput),
        (10, 1, Limits { max_dimension: 9, ..Limits::default() }, Error::OutputLimit),
        (1, 10, Limits { max_dimension: 9, ..Limits::default() }, Error::OutputLimit),
        (10, 10, Limits { max_pixels: 99, ..Limits::default() }, Error::OutputLimit),
    ] {
        let mut calls = 0;
        let result = stages::crop(&image(), &points, &limits, || {
            calls += 1;
            Ok(RgbImage::new(width, height))
        });
        assert_eq!(calls, 1);
        assert_eq!(result, Err(expected));
    }
}

#[test]
fn crop_size_prediction_matches_every_corner_order_and_pinned_kernel_rotation() {
    let image = RgbImage::new(128, 128);
    for points in [
        [(10.0, 20.0), (50.0, 20.0), (50.0, 34.0), (10.0, 34.0)],
        [(10.25, 20.5), (50.75, 19.5), (48.5, 35.75), (9.5, 33.25)],
        [(10.0, 20.0), (20.0, 20.0), (20.0, 62.0), (10.0, 62.0)],
        [(0.0, 0.0), (63.0, 62.0), (62.0, 63.0), (0.0, 1.0)],
        [(0.0, 0.0), (63.0, 0.0), (63.0, 63.0), (62.0, 63.0)],
    ] {
        let points = points.map(|(x, y)| oar_ocr_core::processors::Point::new(x, y));
        for a in 0..4 {
            for b in 0..4 {
                if a == b {
                    continue;
                }
                for c in 0..4 {
                    if [a, b].contains(&c) {
                        continue;
                    }
                    let d = (0..4).find(|i| ![a, b, c].contains(i)).unwrap();
                    let points = [points[a], points[b], points[c], points[d]];
                    let (width, height) = stages::crop_dimensions(&points, 128, 128).unwrap();
                    let mut predicted = [width, height];
                    predicted.sort_unstable();
                    let result =
                        oar_ocr_core::utils::get_rotate_crop_image(&image, &points).unwrap();
                    let mut actual = [result.width(), result.height()];
                    actual.sort_unstable();
                    assert_eq!(predicted, actual, "{points:?}");
                    let exact = Limits {
                        max_dimension: actual[1],
                        max_pixels: u64::from(actual[0]) * u64::from(actual[1]),
                        ..Limits::default()
                    };
                    assert!(stages::crop(&image, &points, &exact, || Ok(result.clone())).is_ok());
                }
            }
        }
    }
}

#[test]
fn crop_prediction_keeps_integer_axis_bounds_and_kernel_refusal_classification() {
    let axis = [(1.0, 0.0), (17_000_000.0, 0.0), (17_000_000.0, 2.0), (1.0, 2.0)]
        .map(|(x, y)| oar_ocr_core::processors::Point::new(x, y));
    assert_eq!(stages::crop_dimensions(&axis, 17_000_000, 2), Ok((16_999_999, 2)));
    for points in [
        [(0.1, 7.0), (0.15, 0.0), (0.2, 7.0), (0.15, 8.0)],
        [(0.1, 0.9), (1.1, 0.9), (1.1, 1.1), (0.1, 1.1)],
    ] {
        let points = points.map(|(x, y)| oar_ocr_core::processors::Point::new(x, y));
        assert_eq!(stages::crop_dimensions(&points, 10, 10), Err(Error::Recognition));
        assert!(oar_ocr_core::utils::get_rotate_crop_image(&image(), &points).is_err());
    }
}

#[test]
fn crop_admission_precedes_recognition_for_each_output_bound() {
    let diagonal = [(0.0, 0.0), (63.0, 62.0), (62.0, 63.0), (0.0, 1.0)];
    let tall = [(0.0, 0.0), (63.0, 0.0), (63.0, 63.0), (62.0, 63.0)];
    for (points, dimension, pixels) in
        [(diagonal, 64, 4096), (tall, 64, 16 << 20), (tall, 100, 4096)]
    {
        let mut bbox = BoundingBox::from_coords(0.0, 0.0, 63.0, 63.0);
        bbox.points = points.map(|(x, y)| oar_ocr_core::processors::Point::new(x, y)).to_vec();
        let detected = DBModelOutput { boxes: vec![vec![bbox]], scores: vec![vec![0.8]] };
        let image = Arc::new(RgbImage::from_pixel(64, 64, Rgb([255, 255, 255])));
        let limits = Limits { max_dimension: dimension, max_pixels: pixels, ..Limits::default() };
        let mut calls = 0;
        let result = stages::recognize_regions(&image, detected, &limits, |_| {
            calls += 1;
            Ok(recognition(&["unexpected recognition"], vec![0.8]))
        });
        assert_eq!(result, Err(Error::OutputLimit));
        assert_eq!(calls, 0);
    }
}

#[test]
fn failed_crop_discards_prior_recognition_instead_of_filtering_the_region() {
    let mut detected = detection();
    detected.boxes[0][1].points = [(1.0, 1.0), (2.0, 2.0), (3.0, 3.0), (4.0, 4.0)]
        .map(|(x, y)| oar_ocr_core::processors::Point::new(x, y))
        .to_vec();
    let mut calls = 0;
    assert_eq!(
        stages::recognize_regions(&image(), detected, &Limits::default(), |_| {
            calls += 1;
            Ok(recognition(&["kept only on complete success"], vec![0.8]))
        }),
        Err(Error::Recognition)
    );
    assert_eq!(calls, 1);
}

#[test]
fn no_detections_succeeds_empty_without_requesting_recognition() {
    let detected = DBModelOutput { boxes: vec![vec![]], scores: vec![vec![]] };
    let page = stages::recognize_regions(&image(), detected, &Limits::default(), |_| {
        panic!("an empty detection set needs no recognition")
    })
    .unwrap();
    assert_eq!(page.width, 10);
    assert_eq!(page.height, 10);
    assert_eq!(page.lines, []);
}

#[test]
fn an_empty_recognition_cannot_prove_a_detection_was_a_false_positive() {
    for text in ["", " ", "\t\n"] {
        let result = stages::recognize_regions(&image(), detection(), &Limits::default(), |_| {
            Ok(recognition(&[text], vec![0.0]))
        });
        assert_eq!(result, Err(Error::EmptyRecognition));
    }
    let page = stages::recognize_regions(&image(), detection(), &Limits::default(), |_| {
        Ok(recognition(&["read at zero confidence"], vec![0.0]))
    })
    .unwrap();
    assert_eq!(page.lines.len(), 2);
}

#[test]
fn invalid_detection_or_recognition_confidence_refuses_instead_of_filtering() {
    for score in [f32::NAN, f32::INFINITY, -0.1, 1.1] {
        let mut detected = detection();
        detected.scores[0][0] = score;
        assert_eq!(
            stages::recognize_regions(&image(), detected, &Limits::default(), |_| {
                panic!("invalid detection must stop before recognition")
            }),
            Err(Error::InvalidOutput)
        );
        assert_eq!(
            stages::recognize_regions(&image(), detection(), &Limits::default(), |_| {
                Ok(recognition(&["text"], vec![score]))
            }),
            Err(Error::InvalidOutput)
        );
    }
}

#[test]
fn line_admission_precedes_crop_work_and_text_admission_stops_later_recognition() {
    let bounds = Limits { max_lines: 1, ..Limits::default() };
    assert_eq!(
        stages::recognize_regions(&image(), detection(), &bounds, |_| {
            panic!("oversized detection set must stop before recognition")
        }),
        Err(Error::OutputLimit)
    );
    let bounds = Limits { max_text_bytes: 1, ..Limits::default() };
    let mut calls = 0;
    assert_eq!(
        stages::recognize_regions(&image(), detection(), &bounds, |_| {
            calls += 1;
            Ok(recognition(&["ç"], vec![0.9]))
        }),
        Err(Error::OutputLimit)
    );
    assert_eq!(calls, 1);
}

#[test]
fn heatmap_shape_and_values_are_validated_before_postprocessing_can_hide_omissions() {
    for shape in [
        vec![0, 1, 2, 2],
        vec![2, 1, 2, 2],
        vec![1, 0, 2, 2],
        vec![1, 2, 2, 2],
        vec![1, 1, 0, 2],
        vec![1, 1, 2, 0],
        vec![1, 2, 2],
    ] {
        let values = vec![0.5; shape.iter().product()];
        assert_eq!(stages::validate_heatmap(&shape, values), Err(Error::InvalidOutput));
    }
    for values in [
        vec![0.5; 3],
        vec![0.5; 5],
        vec![f32::NAN; 4],
        vec![f32::INFINITY; 4],
        vec![-0.1; 4],
        vec![1.1; 4],
    ] {
        assert_eq!(stages::validate_heatmap(&[1, 1, 2, 2], values), Err(Error::InvalidOutput));
    }
    assert_eq!(stages::validate_heatmap(&[1, 1, 2, 2], [0.0, 0.2, 0.9, 1.0]), Ok(()));
    assert_eq!(stages::validate_batch_shape(&[1, 3, 2, 2], 3), Ok(()));
    assert_eq!(stages::validate_batch_shape(&[0, 3, 2, 2], 3), Err(Error::InvalidOutput));
    assert_eq!(stages::validate_heatmap(&[1, 1, usize::MAX, 2], []), Err(Error::InvalidOutput));
}

#[test]
fn cancellation_after_one_native_recognition_discards_the_page_and_skips_later_crops() {
    let cancelled = std::sync::atomic::AtomicBool::new(false);
    let mut calls = 0;
    let result = stages::recognize_regions_cancelled(
        &image(),
        detection(),
        &Limits::default(),
        |_| {
            calls += 1;
            cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(recognition(&["must not escape"], vec![0.5]))
        },
        &|| cancelled.load(std::sync::atomic::Ordering::Relaxed),
    );
    assert_eq!(result, Err(Error::Cancelled));
    assert_eq!(calls, 1);
}

#[test]
fn recognition_vocabulary_includes_the_pinned_decoders_special_tokens() {
    let dictionary = vec!["a".into(), "ç".into(), " ".into()];
    assert_eq!(crate::inference::recognition_classes(&dictionary), 5);
    let decoder =
        oar_ocr_core::processors::CTCLabelDecode::from_string_list(Some(&dictionary), true, false);
    assert_eq!(decoder.get_character_list(), ['\0', 'a', 'ç', ' ', ' ']);
}

#[test]
fn recognition_tensors_refuse_bad_vocabulary_shape_counts_and_nonfinite_values() {
    for shape in [
        vec![],
        vec![1, 2],
        vec![1, 2, 3, 1],
        vec![0, 2, 3],
        vec![2, 2, 3],
        vec![1, 0, 3],
        vec![1, 2, 2],
        vec![1, usize::MAX, 3],
    ] {
        assert_eq!(stages::validate_recognition(&shape, [0.0; 6], 3), Err(Error::InvalidOutput));
    }
    assert_eq!(stages::validate_recognition(&[1, 2, 2], [0.0; 4], 3), Err(Error::InvalidOutput));
    assert_eq!(stages::validate_recognition(&[1, 1, 0], [], 0), Err(Error::InvalidOutput));
    assert_eq!(stages::validate_recognition(&[1, 2, 3], [0.0; 5], 3), Err(Error::InvalidOutput));
    assert_eq!(stages::validate_recognition(&[1, 2, 3], [0.0; 7], 3), Err(Error::InvalidOutput));
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(
            stages::validate_recognition(&[1, 1, 3], [0.9, value, 0.1], 3),
            Err(Error::InvalidOutput)
        );
    }
    assert_eq!(stages::validate_recognition(&[1, 2, 3], [0.0; 6], 3), Ok(()));
}

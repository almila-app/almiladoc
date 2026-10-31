use std::sync::Arc;

use image::RgbImage;
use oar_ocr_core::core::config::{OrtExecutionProvider, OrtSessionConfig};
use oar_ocr_core::models::detection::db::{
    DBModel, DBModelBuilder, DBPostprocessConfig, DBPreprocessConfig,
};
use oar_ocr_core::models::recognition::crnn::{CRNNModel, CRNNModelBuilder, CRNNModelOutput};
use oar_ocr_core::processors::{CTCLabelDecode, LimitType};

use crate::{Error, Limits, Page, stages};

pub(crate) struct Pipeline {
    detector: DBModel,
    recognizer: CRNNModel,
    detection: DBPostprocessConfig,
    vocabulary: usize,
}

impl Pipeline {
    pub(crate) fn new(
        detector: Vec<u8>,
        recognizer: Vec<u8>,
        dictionary: Vec<String>,
        limits: &Limits,
    ) -> Result<Self, Error> {
        let sessions = OrtSessionConfig::new()
            .with_execution_providers(vec![OrtExecutionProvider::CPU])
            .with_intra_threads(1)
            .with_inter_threads(1)
            .with_parallel_execution(false);
        let detection = DBPostprocessConfig {
            max_candidates: usize::MAX,
            box_threshold: 0.6,
            ..DBPostprocessConfig::default()
        };
        let detector = DBModelBuilder::new()
            .preprocess_config(DBPreprocessConfig {
                limit_side_len: Some(960),
                limit_type: Some(LimitType::Max),
                max_side_limit: Some(limits.max_dimension),
                ..DBPreprocessConfig::default()
            })
            .postprocess_config(detection.clone())
            .with_ort_config(sessions.clone())
            .build(detector)
            .map_err(|_| Error::Model)?;
        let vocabulary = recognition_classes(&dictionary);
        let recognizer = CRNNModelBuilder::new()
            .character_dict(dictionary)
            .with_ort_config(sessions)
            .build(recognizer)
            .map_err(|_| Error::Model)?;
        Ok(Self { detector, recognizer, detection, vocabulary })
    }

    pub(crate) fn recognize(
        &self,
        image: RgbImage,
        limits: &Limits,
        cancelled: &(impl Fn() -> bool + Sync),
    ) -> Result<Page, Error> {
        crate::checkpoint(cancelled)?;
        let image = Arc::new(image);
        let (input, shapes) =
            self.detector.preprocess(vec![(*image).clone()]).map_err(|_| Error::Recognition)?;
        stages::validate_batch_shape(input.shape(), 3)?;
        if shapes.len() != 1 {
            return Err(Error::InvalidOutput);
        }
        crate::checkpoint(cancelled)?;
        let predictions = self.detector.infer(&input).map_err(|_| Error::Recognition)?;
        drop(input);
        crate::checkpoint(cancelled)?;
        stages::validate_heatmap(predictions.shape(), predictions.iter().copied())?;
        let detected = self.detector.postprocess(
            &predictions,
            shapes,
            self.detection.score_threshold,
            self.detection.box_threshold,
            self.detection.unclip_ratio,
        );
        drop(predictions);
        crate::checkpoint(cancelled)?;
        stages::recognize_regions_cancelled(
            &image,
            detected,
            limits,
            |crop| self.recognize_crop(crop, cancelled),
            cancelled,
        )
    }

    fn recognize_crop(
        &self,
        crop: &RgbImage,
        cancelled: &(impl Fn() -> bool + Sync),
    ) -> Result<CRNNModelOutput, Error> {
        crate::checkpoint(cancelled)?;
        let input = self.recognizer.preprocess_refs(&[crop]).map_err(|_| Error::Recognition)?;
        stages::validate_batch_shape(input.shape(), 3)?;
        crate::checkpoint(cancelled)?;
        let prediction = self.recognizer.infer(&input).map_err(|_| Error::Recognition)?;
        drop(input);
        crate::checkpoint(cancelled)?;
        stages::validate_recognition(
            prediction.shape(),
            prediction.iter().copied(),
            self.vocabulary,
        )?;
        Ok(self.recognizer.postprocess(&prediction, false))
    }
}

pub(crate) fn recognition_classes(dictionary: &[String]) -> usize {
    // CRNNModelBuilder::build in oar-ocr-core 0.10.2 fixes these space/blank options.
    CTCLabelDecode::from_string_list(Some(dictionary), true, false).get_character_count()
}

use crate::error::ConvertError;
use crate::ledger::CancelToken;
use crate::model::{Gap, GapId};
use crate::ocr::{
    GapError, GapFill, OcrEngine, OcrOutcome, OcrRefusal, OcrRequest, OcrSummary, RasterSpec,
};

/// The parsed document an OCR call asks requests of and settles gaps in.
pub(crate) trait Target {
    /// The request or refusal of each of `gaps` that asks for an image, in order, its image
    /// charged and a page painted under `spec`.
    fn requests(
        &mut self,
        gaps: &[Gap],
        spec: &RasterSpec,
    ) -> Result<Vec<Result<OcrRequest, OcrRefusal>>, ConvertError>;
    fn fill(&mut self, id: GapId, fill: GapFill) -> Result<(), GapError>;
    fn fail(&mut self, id: GapId) -> Result<(), GapError>;
}

/// Run `engine` over `gaps`, each asking for an image, in order and in batches of its size:
/// each outcome fills or fails its gap, a refused fill fails it, a batch answered by the wrong
/// number of outcomes fails all of its gaps, a gap with no image is counted as refused and left
/// unfilled, and a batch with an unattempted gap is the last.
/// A cancel seen once a batch returns applies none of its outcomes.
pub(crate) fn run(
    target: &mut impl Target,
    gaps: &[Gap],
    engine: &dyn OcrEngine,
    cancel: &CancelToken,
) -> Result<OcrSummary, ConvertError> {
    let (size, spec) = (engine.batch_size().max(1), engine.raster());
    spec.validate().map_err(ConvertError::InvalidConfig)?;
    let mut summary = OcrSummary::default();
    for (done, batch) in gaps.chunks(size).enumerate() {
        let mut requests = Vec::with_capacity(batch.len());
        for made in target.requests(batch, &spec)? {
            match made {
                Ok(request) => requests.push(request),
                Err(_) => summary.refused += 1,
            }
        }
        if requests.is_empty() {
            continue;
        }
        refuse_cancelled(cancel)?;
        let outcomes = engine.recognize(&requests, cancel);
        refuse_cancelled(cancel)?;
        if outcomes.len() != requests.len() {
            for request in &requests {
                settled(target.fail(request.gap))?;
            }
            summary.failed += requests.len();
            continue;
        }
        let mut stopped = false;
        for (request, outcome) in requests.iter().zip(outcomes) {
            match outcome {
                OcrOutcome::Fill(fill) => match target.fill(request.gap, fill) {
                    Ok(()) => summary.filled += 1,
                    Err(GapError::Refused { .. } | GapError::InvalidTable) => {
                        settled(target.fail(request.gap))?;
                        summary.failed += 1;
                    }
                    Err(error) => return Err(converted(error)),
                },
                OcrOutcome::Failed(_) => {
                    settled(target.fail(request.gap))?;
                    summary.failed += 1;
                }
                OcrOutcome::Unattempted(_) => {
                    summary.unattempted += 1;
                    stopped = true;
                }
            }
        }
        if stopped {
            summary.unattempted += gaps.len().saturating_sub(done.saturating_add(1) * size);
            break;
        }
    }
    Ok(summary)
}

fn refuse_cancelled(cancel: &CancelToken) -> Result<(), ConvertError> {
    if cancel.is_cancelled() { Err(ConvertError::Cancelled) } else { Ok(()) }
}

fn settled(result: Result<(), GapError>) -> Result<(), ConvertError> {
    result.map_err(converted)
}

/// A gap the call requested is neither unknown nor settled, so only a conversion error is.
fn converted(error: GapError) -> ConvertError {
    match error {
        GapError::Convert(error) => error,
        _ => ConvertError::Internal,
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;
    use std::sync::Mutex;

    use super::*;
    use crate::limits::Limit;
    use crate::model::{AssetId, OcrOrigin};
    use crate::ocr::{OcrFailure, OcrFailureKind, OcrImage, RefusalKind};

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Settled {
        Filled(String),
        Failed,
    }

    #[derive(Default)]
    struct Fake {
        settled: Vec<(u32, Settled)>,
        refuse: Option<u32>,
        fail_fill: Option<(u32, ConvertError)>,
        unasked: Vec<u32>,
        unpaintable: Vec<u32>,
    }

    fn gap(n: u32) -> Gap {
        let id = GapId::new(None, NonZeroU32::new(n).unwrap());
        Gap { id, origin: OcrOrigin::Image(AssetId(0)), reasons: Vec::new() }
    }

    impl Target for Fake {
        fn requests(
            &mut self,
            gaps: &[Gap],
            _: &RasterSpec,
        ) -> Result<Vec<Result<OcrRequest, OcrRefusal>>, ConvertError> {
            let image = OcrImage {
                bytes: Vec::new().into(),
                media_type: "image/png",
                dpi: None,
                width: None,
                height: None,
                fidelity: None,
            };
            let request = |gap: &Gap| {
                if self.unpaintable.contains(&gap.id.ordinal().get()) {
                    let kind = RefusalKind::Unpaintable;
                    return Err(OcrRefusal {
                        gap: gap.id,
                        origin: gap.origin,
                        reasons: Vec::new(),
                        kind,
                    });
                }
                Ok(OcrRequest {
                    gap: gap.id,
                    origin: gap.origin,
                    reasons: Vec::new(),
                    image: image.clone(),
                    max_text_bytes: 1,
                })
            };
            let asked = gaps.iter().filter(|gap| !self.unasked.contains(&gap.id.ordinal().get()));
            Ok(asked.map(request).collect())
        }

        fn fill(&mut self, id: GapId, fill: GapFill) -> Result<(), GapError> {
            let n = id.ordinal().get();
            if self.refuse == Some(n) {
                return Err(GapError::Refused { limit: Limit::OcrTextBytes });
            }
            if let Some((_, error)) = self.fail_fill.take_if(|(at, _)| *at == n) {
                return Err(GapError::Convert(error));
            }
            let GapFill::Markdown(text) = fill else {
                return Err(GapError::Convert(ConvertError::Internal));
            };
            self.settled.push((n, Settled::Filled(text)));
            Ok(())
        }

        fn fail(&mut self, id: GapId) -> Result<(), GapError> {
            self.settled.push((id.ordinal().get(), Settled::Failed));
            Ok(())
        }
    }

    /// Answers each call with its script's next row, one letter per outcome: `F`ill, fai`L`,
    /// `U`nattempted; a missing row fills every request.
    struct Scripted {
        size: usize,
        script: Mutex<Vec<&'static str>>,
        calls: Mutex<Vec<Vec<u32>>>,
        cancel_on: Option<usize>,
    }

    impl Scripted {
        fn new(size: usize, script: &[&'static str]) -> Self {
            let script = script.iter().rev().copied().collect();
            Self { size, script: Mutex::new(script), calls: Mutex::default(), cancel_on: None }
        }

        fn calls(&self) -> Vec<Vec<u32>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl OcrEngine for Scripted {
        fn batch_size(&self) -> usize {
            self.size
        }

        fn recognize(&self, requests: &[OcrRequest], cancel: &CancelToken) -> Vec<OcrOutcome> {
            let ordinals: Vec<u32> = requests.iter().map(|r| r.gap.ordinal().get()).collect();
            let mut calls = self.calls.lock().unwrap();
            calls.push(ordinals.clone());
            if self.cancel_on == Some(calls.len()) {
                cancel.cancel();
            }
            let row = self.script.lock().unwrap().pop();
            let letters = row.map_or_else(|| "F".repeat(ordinals.len()), str::to_owned);
            letters
                .chars()
                .zip(ordinals.iter().chain(std::iter::repeat(&0)))
                .map(|(letter, n)| match letter {
                    'F' => OcrOutcome::Fill(GapFill::Markdown(format!("text {n}"))),
                    'L' => OcrOutcome::Failed(OcrFailure::new(OcrFailureKind::Busy, Some(500))),
                    _ => {
                        OcrOutcome::Unattempted(OcrFailure::new(OcrFailureKind::Unreachable, None))
                    }
                })
                .collect()
        }
    }

    fn filled(n: u32) -> (u32, Settled) {
        (n, Settled::Filled(format!("text {n}")))
    }

    fn summary(filled: usize, failed: usize, unattempted: usize) -> OcrSummary {
        OcrSummary { filled, failed, unattempted, refused: 0 }
    }

    fn seven() -> Vec<Gap> {
        (1..=7).map(gap).collect()
    }

    #[test]
    fn batches_of_the_engine_s_size_run_in_order() {
        for (size, calls) in [
            (3, vec![vec![1, 2, 3], vec![4, 5, 6], vec![7]]),
            (0, (1..=7).map(|n| vec![n]).collect()),
            (9, vec![(1..=7).collect()]),
        ] {
            let (mut target, engine) = (Fake::default(), Scripted::new(size, &[]));
            let done = run(&mut target, &seven(), &engine, &CancelToken::new());
            assert_eq!((done, engine.calls()), (Ok(summary(7, 0, 0)), calls), "{size}");
            assert_eq!(target.settled, (1..=7).map(filled).collect::<Vec<_>>());
        }
    }

    #[test]
    fn a_batch_that_asks_for_no_image_reaches_no_engine() {
        let mut target = Fake { unasked: vec![3, 4], ..Fake::default() };
        let engine = Scripted::new(2, &[]);
        let done = run(&mut target, &seven(), &engine, &CancelToken::new());
        assert_eq!(done, Ok(summary(5, 0, 0)));
        assert_eq!(engine.calls(), [vec![1, 2], vec![5, 6], vec![7]]);
    }

    #[test]
    fn a_refused_gap_reaches_no_engine_stays_unsettled_and_is_counted_beside_the_rest() {
        let mut target = Fake { unpaintable: vec![2, 5], ..Fake::default() };
        let engine = Scripted::new(3, &["FF", "FU"]);
        let done = run(&mut target, &seven(), &engine, &CancelToken::new());
        let expected = OcrSummary { filled: 3, failed: 0, unattempted: 2, refused: 2 };
        assert_eq!(done, Ok(expected));
        assert_eq!(engine.calls(), [vec![1, 3], vec![4, 6]]);
        assert_eq!(target.settled, [1, 3, 4].map(filled));
    }

    #[test]
    fn a_batch_of_refused_gaps_is_counted_and_asks_no_engine() {
        let mut target = Fake { unpaintable: vec![1, 2, 3], ..Fake::default() };
        let engine = Scripted::new(3, &[]);
        let done = run(&mut target, &seven(), &engine, &CancelToken::new());
        assert_eq!(done, Ok(OcrSummary { filled: 4, failed: 0, unattempted: 0, refused: 3 }));
        assert_eq!(engine.calls(), [vec![4, 5, 6], vec![7]]);
        assert_eq!(target.settled, [4, 5, 6, 7].map(filled));
    }

    #[test]
    fn an_unattempted_gap_ends_the_call_after_its_batch() {
        let (mut target, engine) = (Fake::default(), Scripted::new(3, &["FFF", "FUF"]));
        let done = run(&mut target, &seven(), &engine, &CancelToken::new());
        assert_eq!(done, Ok(summary(5, 0, 2)));
        assert_eq!(engine.calls(), [vec![1, 2, 3], vec![4, 5, 6]]);
        assert_eq!(target.settled, [1, 2, 3, 4, 6].map(filled));
        let engine = Scripted::new(3, &[]);
        let done = run(&mut target, &[gap(5), gap(7)], &engine, &CancelToken::new());
        assert_eq!((done, engine.calls()), (Ok(summary(2, 0, 0)), vec![vec![5, 7]]));
    }

    #[test]
    fn a_wrong_outcome_count_fails_its_batch_and_the_call_goes_on() {
        for script in ["FF", "FFFF"] {
            let (mut target, engine) = (Fake::default(), Scripted::new(3, &[script]));
            let done = run(
                &mut target,
                &(1..=4).map(gap).collect::<Vec<_>>(),
                &engine,
                &CancelToken::new(),
            );
            assert_eq!(done, Ok(summary(1, 3, 0)), "{script}");
            let failed = [1, 2, 3].map(|n| (n, Settled::Failed));
            assert_eq!(target.settled, [failed.as_slice(), &[filled(4)]].concat());
        }
    }

    #[test]
    fn a_failure_or_a_refused_fill_fails_its_gap_alone() {
        let mut target = Fake { refuse: Some(2), ..Fake::default() };
        let engine = Scripted::new(3, &["FFL"]);
        let done = run(&mut target, &[gap(1), gap(2), gap(3)], &engine, &CancelToken::new());
        assert_eq!(done, Ok(summary(1, 2, 0)));
        assert_eq!(target.settled, [filled(1), (2, Settled::Failed), (3, Settled::Failed)]);
    }

    #[test]
    fn a_cancel_in_recognize_keeps_earlier_batches_and_applies_none_of_its_own() {
        let (mut target, mut engine) = (Fake::default(), Scripted::new(3, &[]));
        engine.cancel_on = Some(2);
        let done = run(&mut target, &seven(), &engine, &CancelToken::new());
        assert_eq!(done, Err(ConvertError::Cancelled));
        assert_eq!(engine.calls().len(), 2);
        assert_eq!(target.settled, [1, 2, 3].map(filled));
        let cancelled = CancelToken::new();
        cancelled.cancel();
        let engine = Scripted::new(3, &[]);
        let done = run(&mut Fake::default(), &seven(), &engine, &cancelled);
        assert_eq!((done, engine.calls().len()), (Err(ConvertError::Cancelled), 0));
    }

    #[test]
    fn a_conversion_error_in_a_fill_ends_the_call() {
        let error = || ConvertError::limit(Limit::Work, 1);
        let mut target = Fake { fail_fill: Some((2, error())), ..Fake::default() };
        let engine = Scripted::new(3, &[]);
        let done = run(&mut target, &[gap(1), gap(2), gap(3)], &engine, &CancelToken::new());
        assert_eq!((done, target.settled), (Err(error()), vec![filled(1)]));
    }
}

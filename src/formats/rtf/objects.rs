use super::lexer::hex_digit;
use super::{Capture, Parser};
use crate::diagnostics::{DiagnosticCode, Region};
use crate::error::ConvertError;
use crate::limits::Limit;
use crate::model::Inline;
use crate::shared::math::Equation;
use crate::shared::ole_equation::{is_equation, ole_equation, ole1_object};

const CLASS_BYTES: usize = 64;

#[derive(Default)]
pub(super) struct Payload {
    depth: usize,
    bytes: Vec<u8>,
    high: Option<u8>,
}

impl Payload {
    fn push(&mut self, byte: u8, max: u64) -> Result<(), ConvertError> {
        let Some(digit) = hex_digit(byte) else {
            return Ok(());
        };
        let Some(high) = self.high.take() else {
            self.high = Some(digit);
            return Ok(());
        };
        if self.bytes.len() as u64 >= max {
            return Err(ConvertError::limit(Limit::EntryBytes, max));
        }
        self.bytes.push((high << 4) | digit);
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Omission {
    Revision,
    Hidden,
}

pub(super) struct ObjectState {
    depth: usize,
    region: Region,
    class: String,
    payload: Option<Payload>,
    payload_seen: bool,
    result_seen: bool,
    omission: Option<Omission>,
    equation: Option<Equation>,
}

impl ObjectState {
    pub(super) fn new(depth: usize, region: Region) -> Self {
        Self {
            depth,
            region,
            class: String::new(),
            payload: None,
            payload_seen: false,
            result_seen: false,
            omission: None,
            equation: None,
        }
    }

    pub(super) fn closes_at(&self, depth: usize) -> bool {
        depth < self.depth
    }

    pub(super) fn push_class(&mut self, text: &str) {
        if self.class.len() + text.len() <= CLASS_BYTES {
            self.class.push_str(text);
        }
    }
}

impl Parser<'_> {
    pub(super) fn open_object(&mut self) {
        if self.stack.last().is_none_or(|holder| holder.suppress) {
            return;
        }
        let region = self.region();
        if self.dest.object.is_some() {
            self.parse_ctx.emit(DiagnosticCode::EmbeddedObject, region);
        } else {
            self.dest.object = Some(ObjectState::new(self.stack.len(), region));
        }
    }

    pub(super) fn object_part(&mut self, word: &str) -> bool {
        if word == "result" {
            self.object_result();
            return true;
        }
        let depth = self.stack.len();
        let Some(object) = self.dest.object.as_mut().filter(|object| depth == object.depth + 1)
        else {
            return false;
        };
        let capture = match word {
            "objclass" => Capture::ObjectClass,
            "objdata"
                if is_equation(object.class.trim())
                    && object.omission.is_none()
                    && !object.payload_seen
                    && !object.result_seen =>
            {
                object.payload_seen = true;
                object.payload = Some(Payload { depth, ..Payload::default() });
                Capture::ObjectData
            }
            _ => return false,
        };
        self.flush_pending();
        self.state.suppress = true;
        self.state.capture = capture;
        true
    }

    fn object_result(&mut self) {
        let depth = self.stack.len();
        if let Some(object) = self.dest.object.as_mut().filter(|object| depth == object.depth + 1) {
            object.result_seen = true;
            self.state.omit |= object.equation.is_some();
        }
        self.state.suppress = self.state.omit;
    }

    pub(super) fn object_payload(&mut self) -> Option<&mut Payload> {
        let depth = self.stack.len();
        let capturing = self.state.capture == Capture::ObjectData;
        let object = self.dest.object.as_mut()?;
        object.payload.as_mut().filter(|payload| capturing && payload.depth == depth)
    }

    pub(super) fn push_object_payload(&mut self, byte: u8) -> Result<bool, ConvertError> {
        let max = self.parse_ctx.limits().max_entry_bytes;
        match self.object_payload() {
            Some(payload) => payload.push(byte, max).map(|()| true),
            None => Ok(false),
        }
    }

    pub(super) fn close_payload(&mut self, depth: usize) -> Result<(), ConvertError> {
        let Some(object) = &mut self.dest.object else {
            return Ok(());
        };
        if object.payload.as_ref().is_none_or(|payload| depth >= payload.depth) {
            return Ok(());
        }
        let bytes = object.payload.take().map(|payload| payload.bytes).unwrap_or_default();
        self.parse_ctx.charge(bytes.len() as u64)?;
        object.equation = match ole1_object(&bytes) {
            Some(embedded) => ole_equation(self.parse_ctx, Some(embedded.class), embedded.native)?,
            None => None,
        };
        Ok(())
    }

    pub(super) fn note_object_omission(&mut self) {
        let omission = if self.revision_drops() {
            Some(Omission::Revision)
        } else if self.hides() {
            Some(Omission::Hidden)
        } else {
            None
        };
        let depth = self.stack.len();
        if let Some(object) = self.dest.object.as_mut().filter(|object| object.depth == depth) {
            object.omission = object.omission.or(omission);
        }
    }

    pub(super) fn finish_object(&mut self) {
        let Some(ObjectState { region, omission, equation, .. }) = self.dest.object.take() else {
            return;
        };
        match (omission, equation) {
            (Some(Omission::Revision), _) => self.drop_deleted(),
            (Some(Omission::Hidden), _) => self.drop_hidden(),
            (None, None) => self.parse_ctx.emit(DiagnosticCode::EmbeddedObject, region),
            (None, Some(Equation { tex, inline, unmapped })) => {
                self.parse_ctx.emit_count(DiagnosticCode::UnmappedSymbol, region, unmapped);
                self.inlines.push(Inline::Math(tex));
                self.dest.math_display |= !inline;
            }
        }
    }
}

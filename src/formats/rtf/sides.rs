use super::CharState;
use crate::model::{HeaderFooterRegion as Side, HeaderFooterVariant as Variant};
use std::collections::HashMap;
use std::ops::Range;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Pages {
    All,
    Left,
    Right,
    First,
}

pub(super) fn destination(word: &str) -> Option<(Side, Pages)> {
    let side = if word.starts_with("header") { Side::Header } else { Side::Footer };
    let pages = match word {
        "header" | "footer" => Pages::All,
        "headerl" | "footerl" => Pages::Left,
        "headerr" | "footerr" => Pages::Right,
        "headerf" | "footerf" => Pages::First,
        _ => return None,
    };
    Some((side, pages))
}

#[derive(Clone, Copy)]
pub(super) struct Definition {
    pub(super) range: (usize, usize),
    pub(super) state: CharState,
}

impl Definition {
    pub(super) fn bytes(&self) -> Range<usize> {
        self.range.0..self.range.1
    }
}

#[derive(Clone)]
pub(super) struct Section {
    pub(super) begin: usize,
    pub(super) title_page: bool,
    pub(super) defined: HashMap<(Side, Pages), usize>,
}

pub(super) type Shown = Option<Option<usize>>;

impl Section {
    pub(super) fn shown(&self, side: Side, variant: Variant, facing: bool) -> Shown {
        let get = |pages| self.defined.get(&(side, pages)).copied();
        let all = get(Pages::All);
        let right = get(Pages::Right);
        let default = if facing { right.or(all) } else { all.or(right) };
        match variant {
            Variant::Default => Some(default),
            Variant::First if self.title_page => match get(Pages::First) {
                Some(first) => Some(Some(first)),
                None if default.is_none() => None,
                None => Some(None),
            },
            Variant::Even if facing => match (get(Pages::Left), right) {
                (Some(left), _) => Some(Some(left)),
                (None, Some(_)) => Some(all),
                (None, None) => None,
            },
            _ => None,
        }
    }
}

/// The sections read so far and the group being skipped.
pub(super) struct Sections {
    pub(super) facing: bool,
    pub(super) open: Option<(Side, Pages, usize, usize, CharState)>,
    pub(super) current: Section,
    pub(super) done: Vec<Section>,
    pub(super) definitions: Vec<Definition>,
}

impl Sections {
    pub(super) fn new() -> Self {
        Self {
            facing: false,
            open: None,
            current: Section { begin: 0, title_page: false, defined: HashMap::new() },
            done: Vec::new(),
            definitions: Vec::new(),
        }
    }

    pub(super) fn close(&mut self, depth: usize, end: usize) {
        if let Some((side, pages, opened, start, state)) = self.open
            && depth < opened
        {
            self.open = None;
            self.current.defined.insert((side, pages), self.definitions.len());
            self.definitions.push(Definition { range: (start, end.max(start)), state });
        }
    }

    pub(super) fn break_at(&mut self, begin: usize) {
        let next = Section { begin, title_page: false, defined: self.current.defined.clone() };
        self.done.push(std::mem::replace(&mut self.current, next));
    }
}

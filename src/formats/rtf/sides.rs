//! Section headers and footers (RTF 1.9.1, "Headers and Footers"): each `\header`-family group
//! is skipped where it stands and remembered by its byte range, and each section shows those it
//! or an earlier section defines, `\titlepg` and `\facingp` choosing the first and left pages
//! wherever they appear in the section.

use super::CharState;
use crate::model::{HeaderFooterRegion as Side, HeaderFooterVariant as Variant};
use std::collections::HashMap;
use std::ops::Range;

/// Which pages a `\header`-family group names.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Pages {
    All,
    Left,
    Right,
    First,
}

/// The side and pages a destination control word names.
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

/// One group's content, read only where a section shows it.
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

/// A section: where its blocks start, whether it shows a first-page variant, and the groups in
/// force for each side and pages, its own over those it inherits.
#[derive(Clone)]
pub(super) struct Section {
    pub(super) begin: usize,
    pub(super) title_page: bool,
    pub(super) defined: HashMap<(Side, Pages), usize>,
}

/// What each variant of a section shows: the definition it reads, `Some(None)` for nothing,
/// `None` for the default variant.
pub(super) type Shown = Option<Option<usize>>;

impl Section {
    /// The definition each variant of `side` shows on these pages.
    /// Facing pages show `\headerr` on right pages and `\headerl` on left ones, `\header` on
    /// those that lack their own; other pages show `\header`, else `\headerr`.
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
    /// The skipped group's side and pages, the depth it opened at, where its content starts and
    /// the state it opens in.
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

    /// End the open group where its closing brace stands.
    pub(super) fn close(&mut self, depth: usize, end: usize) {
        if let Some((side, pages, opened, start, state)) = self.open
            && depth < opened
        {
            self.open = None;
            self.current.defined.insert((side, pages), self.definitions.len());
            self.definitions.push(Definition { range: (start, end.max(start)), state });
        }
    }

    /// Start the next section at block `begin`, inheriting the groups in force.
    pub(super) fn break_at(&mut self, begin: usize) {
        let next = Section { begin, title_page: false, defined: self.current.defined.clone() };
        self.done.push(std::mem::replace(&mut self.current, next));
    }
}

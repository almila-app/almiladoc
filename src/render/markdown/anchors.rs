//! Anchor resolution: maps every internal anchor id to the fragment it will
//! have in the rendered Markdown. Heading-coincident anchors reuse the
//! heading's GFM auto-generated slug; an anchor a link targets gets a
//! sanitized, stable HTML id rendered as `<a id="..."></a>` at its position.
//!
//! Anchors nothing links to render nothing: producers mark up far more
//! positions than they reference (a bookmark per paragraph, an id per EPUB
//! element), and an unreachable target is only noise in the output.

use crate::ImagePolicy;
use crate::model::{Block, Document, Inline, LinkTarget, Note, Unit};
use crate::ocr::GapFills;
use crate::render::markdown::escape::controls_to_spaces;
use crate::render::markdown::image;
use crate::render::markdown::inline::url_label;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub(crate) struct AnchorMap {
    resolved: HashMap<String, Resolved>,
    units: HashMap<Unit, String>,
}

struct Resolved {
    fragment: String,
    /// True when the anchor needs an explicit `<a id>` emitted at its
    /// position (false for heading-coincident anchors - the heading's own
    /// slug carries them).
    emit_html: bool,
}

impl AnchorMap {
    pub(crate) fn unit_fragment(&self, unit: Unit) -> Option<&str> {
        self.units.get(&unit).map(String::as_str)
    }

    /// The `#fragment` a link to `id` should use, if the target exists.
    pub(crate) fn fragment(&self, id: &str) -> Option<&str> {
        self.resolved.get(id).map(|r| r.fragment.as_str())
    }

    /// The HTML id to emit for an `Inline::Anchor` node, when one is needed.
    pub(crate) fn html_id(&self, id: &str) -> Option<&str> {
        self.resolved.get(id).filter(|r| r.emit_html).map(|r| r.fragment.as_str())
    }
}

pub(crate) fn resolve_anchors(
    doc: &Document,
    fills: &GapFills,
    notes: &[&Note],
    images: ImagePolicy,
    note_numbers: &HashMap<String, usize>,
) -> AnchorMap {
    let mut ids = UniqueIds::default();
    let mut resolved: HashMap<String, Resolved> = HashMap::new();

    // Anchor ids some link in the document targets, notes included.
    let mut linked: HashSet<&str> = HashSet::new();
    let mut linked_units = HashSet::new();
    walk_all(doc, fills, notes, &mut |block| {
        if let Some(content) = inlines(block) {
            for_each_link_target(content, &mut linked, &mut linked_units);
        }
    });

    // Pass 1: headings claim their GFM slugs in render order, binding any
    // heading-coincident anchor ids (the `anchor` field and anchor nodes
    // inside the heading content) to those slugs.
    let mut heading = |block: &Block| {
        if let Block::Heading { anchor, content, .. } = block {
            let label = heading_label(content, images, note_numbers);
            if !label.shown {
                return;
            }
            let Some(slug) = ids.claim(gfm_slug(&label.text)) else {
                return;
            };
            let mut bind = |id: &str| {
                resolved
                    .entry(id.to_string())
                    .or_insert_with(|| Resolved { fragment: slug.clone(), emit_html: false });
            };
            if let Some(id) = anchor {
                bind(id);
            }
            for_each_anchor(content, &mut bind);
        }
    };
    walk_all(doc, fills, notes, &mut heading);

    // Pass 2: every remaining anchor a link targets gets a sanitized HTML id.
    let mut assign = |id: &str| {
        if linked.contains(id)
            && !resolved.contains_key(id)
            && let Some(html) = ids.claim(sanitize_id(id))
        {
            resolved.insert(id.to_string(), Resolved { fragment: html, emit_html: true });
        }
    };
    walk_all(doc, fills, notes, &mut |block| {
        if let Some(content) = inlines(block) {
            for_each_anchor(content, &mut assign);
        }
    });

    let mut units = HashMap::new();
    for block in &doc.blocks {
        if let Block::UnitStart(unit) = block
            && linked_units.contains(unit)
            && !units.contains_key(unit)
            && let Some(id) =
                ids.claim(format!("{}-{}", unit.kind.as_str().replace('_', "-"), unit.ordinal))
        {
            units.insert(*unit, id);
        }
    }
    AnchorMap { resolved, units }
}

#[derive(Default)]
struct HeadingLabel {
    text: String,
    shown: bool,
}

fn heading_label(
    inlines: &[Inline],
    images: ImagePolicy,
    note_numbers: &HashMap<String, usize>,
) -> HeadingLabel {
    let mut label = HeadingLabel::default();
    for inline in inlines {
        let (text, shown) = match inline {
            Inline::Text { text, .. } | Inline::Math(text) => {
                (Cow::Borrowed(text.as_str()), !text.trim().is_empty())
            }
            Inline::Link { content, target } => {
                let content = heading_label(content, images, note_numbers);
                match url_label(target).filter(|_| !content.shown) {
                    Some(url) => (controls_to_spaces(url), true),
                    None => (Cow::Owned(content.text), content.shown),
                }
            }
            Inline::Image { alt, .. } if images != ImagePolicy::Omit => (image::label(alt), true),
            Inline::Checkbox(value) => (Cow::Borrowed(if *value { "[x]" } else { "[ ]" }), true),
            Inline::LineBreak => (Cow::Borrowed(" "), false),
            Inline::NoteRef(id) => (Cow::Borrowed(""), note_numbers.contains_key(id)),
            _ => continue,
        };
        label.text.push_str(&text);
        label.shown |= shown;
    }
    label
}

fn for_each_link_target<'a>(
    inlines: &'a [Inline],
    out: &mut HashSet<&'a str>,
    units: &mut HashSet<Unit>,
) {
    for inline in inlines {
        if let Inline::Link { content, target } = inline {
            match target {
                LinkTarget::Anchor { id, .. } => {
                    out.insert(id.as_str());
                }
                LinkTarget::Unit(unit) => {
                    units.insert(*unit);
                }
                _ => {}
            }
            for_each_link_target(content, out, units);
        }
    }
}

fn inlines(block: &Block) -> Option<&[Inline]> {
    match block {
        Block::Heading { content, .. } | Block::Paragraph(content) => Some(content),
        _ => None,
    }
}

fn walk_all<'a>(
    doc: &'a Document,
    fills: &'a GapFills,
    notes: &[&'a Note],
    f: &mut impl FnMut(&'a Block),
) {
    walk_blocks(&doc.blocks, fills, f);
    for note in notes {
        walk_blocks(&note.blocks, fills, f);
    }
}

fn walk_blocks<'a>(blocks: &'a [Block], fills: &'a GapFills, f: &mut impl FnMut(&'a Block)) {
    let mut stack: Vec<&Block> = blocks.iter().rev().collect();
    while let Some(block) = stack.pop() {
        f(block);
        match block {
            Block::List(list) => {
                for item in list.items.iter().rev() {
                    stack.extend(item.blocks.iter().rev());
                }
            }
            Block::Table(table) => {
                for cell in table.origins().rev() {
                    stack.extend(cell.blocks.iter().rev());
                }
            }
            Block::BlockQuote(inner) => stack.extend(inner.iter().rev()),
            Block::Gap(gap) => stack.extend(fills.filled(gap).into_iter().flatten().rev()),
            _ => {}
        }
    }
}

fn for_each_anchor(inlines: &[Inline], f: &mut impl FnMut(&str)) {
    for inline in inlines {
        match inline {
            Inline::Anchor(id) => f(id),
            Inline::Link { content, .. } => for_each_anchor(content, f),
            _ => {}
        }
    }
}

/// Every id handed out, mapped to the numeric suffix to try next for it as a base.
#[derive(Default)]
struct UniqueIds {
    next_suffix: HashMap<String, usize>,
}

impl UniqueIds {
    fn claim(&mut self, base: String) -> Option<String> {
        let Some(&start) = self.next_suffix.get(&base) else {
            self.next_suffix.insert(base.clone(), 1);
            return Some(base);
        };
        let mut n = start;
        loop {
            let candidate = format!("{base}-{n}");
            n = n.checked_add(1)?;
            if !self.next_suffix.contains_key(&candidate) {
                self.next_suffix.insert(base, n);
                self.next_suffix.insert(candidate.clone(), 1);
                return Some(candidate);
            }
        }
    }
}

/// GFM-style heading slugs: full-Unicode lowercase, spaces and line breaks become hyphens,
/// and everything except word-forming characters (letters, numbers, marks,
/// connector punctuation) and hyphens drops. An empty result becomes
/// `section` so the anchor stays linkable.
fn gfm_slug(text: &str) -> String {
    let slug: String = text
        .trim()
        .chars()
        .flat_map(char::to_lowercase)
        .filter_map(|c| match c {
            ' ' | '\n' => Some('-'),
            '-' => Some(c),
            c if c.is_alphanumeric() || is_combining_mark(c) || is_connector_punctuation(c) => {
                Some(c)
            }
            _ => None,
        })
        .collect();
    if slug.is_empty() { "section".to_string() } else { slug }
}

/// Combining-mark blocks `is_alphanumeric` misses (marks outside
/// `Other_Alphabetic`, such as viramas and cantillation/stress signs).
fn is_combining_mark(c: char) -> bool {
    matches!(
        u32::from(c),
        0x0300..=0x036F
            | 0x0483..=0x0489
            | 0x0900..=0x0903
            | 0x093A..=0x094F
            | 0x0951..=0x0957
            | 0x0962..=0x0963
            | 0x1AB0..=0x1AFF
            | 0x1DC0..=0x1DFF
            | 0x20D0..=0x20FF
            | 0xFE20..=0xFE2F
    )
}

/// Connector punctuation (category Pc): joins words, kept like `_`.
fn is_connector_punctuation(c: char) -> bool {
    matches!(
        u32::from(c),
        0x005F | 0x203F | 0x2040 | 0x2054 | 0xFE33 | 0xFE34 | 0xFE4D..=0xFE4F | 0xFF3F
    )
}

/// Sanitize a source anchor id into a stable `[a-z0-9-_]` HTML id.
fn sanitize_id(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    let mut prev_dash = false;
    for c in id.chars() {
        let mapped = match c.to_ascii_lowercase() {
            c @ ('a'..='z' | '0'..='9' | '_' | '-') => c,
            _ => '-',
        };
        if mapped == '-' && prev_dash {
            continue;
        }
        prev_dash = mapped == '-';
        out.push(mapped);
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() { "anchor".to_string() } else { trimmed.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NoteKind;

    #[test]
    fn slugs_keep_word_forming_characters_hyphenate_spaces_and_default_to_section() {
        for (text, slug) in [
            ("Hello World!", "hello-world"),
            ("  leading and trailing  ", "leading-and-trailing"),
            ("under_score and hy-phen", "under_score-and-hy-phen"),
            ("C++ & Rust", "c--rust"),
            ("étude précomposée", "étude-précomposée"),
            ("combining n\u{0303} tilde", "combining-n\u{0303}-tilde"),
            ("देवनागरी क्षि", "देवनागरी-क्षि"),
            ("日本語の見出し", "日本語の見出し"),
            ("a‿b undertie", "a‿b-undertie"),
            ("a⁀b tie", "a⁀b-tie"),
            ("a＿b fullwidth", "a＿b-fullwidth"),
            ("quotes 'single' \"double\"", "quotes-single-double"),
            ("copyright © and € and ∑", "copyright--and--and-"),
            ("parens (and) [brackets]", "parens-and-brackets"),
            ("Part One\nOverview\n", "part-one-overview"),
            ("!!!", "section"),
            ("", "section"),
        ] {
            assert_eq!(gfm_slug(text), slug, "{text:?}");
        }
    }

    #[test]
    fn heading_line_breaks_slug_as_spaces() {
        let doc = Document {
            blocks: vec![Block::Heading {
                level: 1,
                anchor: Some("toc".to_string()),
                content: vec![
                    Inline::plain("Part One"),
                    Inline::LineBreak,
                    Inline::plain("Overview"),
                ],
            }],
            notes: Vec::new(),
            assets: Vec::new(),
        };
        assert_eq!(
            resolve_anchors(
                &doc,
                &GapFills::default(),
                &[],
                ImagePolicy::Placeholder,
                &HashMap::new()
            )
            .fragment("toc"),
            Some("part-one-overview")
        );
    }

    #[test]
    fn anchors_and_links_in_notes_resolve_like_those_in_the_body() {
        let anchor = |id: &str| Inline::Anchor(id.into());
        let link =
            |id: &str| Inline::Link { content: Vec::new(), target: LinkTarget::anchor(id.into()) };
        let heading = Block::Heading {
            level: 2,
            anchor: Some("in-note".into()),
            content: vec![Inline::plain("Note Title")],
        };
        let note = Note {
            id: "n".into(),
            kind: NoteKind::Footnote,
            blocks: vec![heading, Block::Paragraph(vec![anchor("target"), link("body")])],
        };
        let doc = Document {
            blocks: vec![Block::Paragraph(vec![anchor("body"), link("target")])],
            ..Document::default()
        };
        let fills = GapFills::default();
        let anchors =
            resolve_anchors(&doc, &fills, &[&note], ImagePolicy::Placeholder, &HashMap::new());
        assert_eq!(anchors.fragment("in-note"), Some("note-title"));
        assert_eq!(anchors.html_id("body"), Some("body"));
        assert_eq!(anchors.html_id("target"), Some("target"));
    }

    #[test]
    fn duplicate_slugs_get_numeric_suffixes() {
        let mut ids = UniqueIds::default();
        assert_eq!(ids.claim(gfm_slug("dup")).as_deref(), Some("dup"));
        assert_eq!(ids.claim(gfm_slug("dup")).as_deref(), Some("dup-1"));
        assert_eq!(ids.claim(gfm_slug("dup")).as_deref(), Some("dup-2"));
    }

    #[test]
    fn a_base_equal_to_an_earlier_suffixed_id_gets_a_suffix_of_its_own() {
        let mut ids = UniqueIds::default();
        let claimed = ["a", "a", "a-1", "a"].map(|base| ids.claim(base.into()));
        assert_eq!(claimed, ["a", "a-1", "a-1-1", "a-2"].map(|id| Some(id.to_string())));
    }

    #[test]
    fn duplicate_slug_suffixes_advance_without_reprobing() {
        let mut ids = UniqueIds::default();
        for n in 0..10_000 {
            let expected = if n == 0 { "same".to_string() } else { format!("same-{n}") };
            assert_eq!(ids.claim("same".to_string()), Some(expected));
        }
        assert_eq!(ids.next_suffix.get("same"), Some(&10_000));
    }
}

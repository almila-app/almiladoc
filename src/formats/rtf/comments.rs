use super::{Capture, CharState, Parser};
use crate::ctx::ParseCtx;
use crate::diagnostics::Region;
use crate::error::ConvertError;
use crate::model::{Block, Inline};
use crate::shared::assets::AssetSink;
use crate::shared::comments::{self, CommentThreads, ReferenceUnits, SourceComment};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub(super) struct Comments {
    definitions: Vec<Definition>,
    open: Option<(usize, Definition)>,
    metadata: Option<(usize, Metadata, String)>,
    author_key: Option<String>,
    skipped_side: Option<usize>,
}

#[derive(Clone, Copy)]
enum Metadata {
    AuthorKey,
    Parent,
}

struct Definition {
    start: usize,
    end: usize,
    id: String,
    author_key: Option<String>,
    parent_key: Option<String>,
    anchored: bool,
    state: CharState,
}

impl Comments {
    pub(super) fn close(
        &mut self,
        ctx: &ParseCtx<'_>,
        depth: usize,
        end: usize,
    ) -> Result<(), ConvertError> {
        if let Some((_, kind, text)) = self.metadata.take_if(|(opened, _, _)| depth < *opened) {
            let text = text.trim();
            match kind {
                Metadata::AuthorKey => {
                    ctx.charge(text.len() as u64)?;
                    self.author_key = Some(text.to_owned());
                }
                Metadata::Parent => {
                    if let Some((_, definition)) = &mut self.open {
                        ctx.charge(text.len() as u64)?;
                        definition.parent_key = Some(text.to_owned());
                    }
                }
            }
        }
        if let Some((_, mut definition)) = self.open.take_if(|(opened, _)| depth < *opened) {
            definition.end = end.max(definition.start);
            self.definitions.push(definition);
        }
        self.skipped_side.take_if(|opened| depth < *opened);
        Ok(())
    }

    pub(super) fn push_text(&mut self, text: &str) {
        if let Some((_, _, buffer)) = &mut self.metadata {
            buffer.push_str(text);
        }
    }

    pub(super) fn capturing(&self) -> bool {
        self.open.is_some()
    }
}

impl Parser<'_> {
    pub(super) fn comment_control(
        &mut self,
        word: &str,
        param: Option<i32>,
    ) -> Result<bool, ConvertError> {
        if !self.parse_ctx.comments_requested() {
            return Ok(false);
        }
        if !self.comment_story
            && self.state.side.is_none()
            && !self.comments.capturing()
            && super::sides::destination(word).is_some()
            && self.stack.last().is_some_and(|holder| !holder.suppress && !holder.omit)
        {
            self.comments.skipped_side = Some(self.stack.len());
        }
        let parent = self
            .comments
            .open
            .as_ref()
            .is_some_and(|(depth, _)| self.stack.len() == depth.saturating_add(1));
        let nested_parent_key = self.comments.metadata.as_ref().is_some_and(|(depth, kind, _)| {
            matches!(kind, Metadata::Parent) && self.stack.len() == depth.saturating_add(1)
        });
        if word == "atnid" && nested_parent_key {
            self.flush_pending();
            self.state.capture = Capture::CommentMetadata;
            self.state.suppress = true;
            self.state.omit = false;
            return Ok(true);
        }
        let skipped_side = self.comments.skipped_side.is_some();
        let captures_id = self.stack.last().is_some_and(|holder| !holder.suppress && !holder.omit);
        let kind = match word {
            "atnparent" if parent || self.comment_story && !self.state.omit => {
                Some(Metadata::Parent)
            }
            "atnid" if captures_id || skipped_side && !self.comments.capturing() => {
                Some(Metadata::AuthorKey)
            }
            "annotation" => return self.begin_comment(skipped_side),
            _ => return Ok(false),
        };
        self.flush_pending();
        if let Some(kind) = kind {
            self.comments.metadata = Some((
                self.stack.len(),
                kind,
                param.map_or_else(String::new, |value| value.to_string()),
            ));
            self.state.capture = Capture::CommentMetadata;
            self.state.suppress = true;
            self.state.omit = false;
        }
        Ok(true)
    }

    pub(super) fn comment_anchor_selected(&self) -> bool {
        let holder = self.stack.last().copied().unwrap_or(self.state);
        let dropped =
            if self.parse_ctx.reject_revisions() { holder.inserted } else { holder.deleted };
        !holder.suppress && !holder.omit && !dropped
    }

    fn begin_comment(&mut self, skipped_side: bool) -> Result<bool, ConvertError> {
        self.flush_pending();
        let holder = self.stack.last().copied().unwrap_or(self.state);
        let anchored = self.comment_anchor_selected();
        if !self.comments.capturing()
            && (anchored || skipped_side || !holder.suppress && !holder.omit)
        {
            let start = self.source_offset.saturating_add(self.lexer.pos());
            let id = format!("rtf:c:s:{start}");
            self.parse_ctx.charge(id.len() as u64)?;
            if anchored {
                self.parse_ctx.charge(id.len() as u64)?;
                self.inlines.push(Inline::CommentRef(id.clone()));
            }
            let author_key = self.comments.author_key.take().filter(|key| !key.is_empty());
            if !self.comment_story {
                self.comments.open = Some((
                    self.stack.len(),
                    Definition {
                        start,
                        end: start,
                        id,
                        author_key,
                        parent_key: None,
                        anchored,
                        state: self.state,
                    },
                ));
            }
        }

        self.state.suppress = true;
        self.state.omit = true;
        self.state.capture = Capture::None;
        Ok(true)
    }

    pub(super) fn read_comments(&mut self, blocks: &mut [Block]) -> Result<(), ConvertError> {
        if !self.parse_ctx.comments_requested() {
            return Ok(());
        }
        let mut definitions = std::mem::take(&mut self.comments.definitions);
        self.parse_ctx.charge(definitions.len() as u64)?;
        definitions
            .sort_by_key(|definition| (definition.start, definition.end, !definition.anchored));
        definitions.dedup_by_key(|definition| (definition.start, definition.end));
        let selected = self.comment_units(blocks)?.into_keys().collect::<BTreeSet<_>>();
        if selected.is_empty() {
            return Ok(());
        }
        let mut sources = metadata(self.parse_ctx, &definitions)?;
        let threads = CommentThreads::new(self.parse_ctx, &sources, Some(&selected))?;
        threads.admit(self.parse_ctx)?;
        comments::remap_references(self.parse_ctx, blocks, threads.aliases())?;
        for note in &mut self.dest.notes {
            comments::remap_references(self.parse_ctx, &mut note.blocks, threads.aliases())?;
        }
        let roots = definitions
            .iter()
            .map(|definition| {
                let root = threads.aliases().get(&definition.id).ok_or(ConvertError::Internal)?;
                self.parse_ctx.charge(root.len() as u64)?;
                Ok(root.clone())
            })
            .collect::<Result<Vec<_>, ConvertError>>()?;
        let units = self.comment_units(blocks)?;
        self.read_comment_bodies(&definitions, &mut sources, &threads, &units)?;
        let (notes, _) = threads.finish_with(self.parse_ctx, sources, |index, code| {
            let root = roots.get(index).ok_or(ConvertError::Internal)?;
            let units = units.get(root).ok_or(ConvertError::Internal)?;
            for unit in &units.all {
                let _reports = self.parse_ctx.scope(*unit);
                self.parse_ctx.emit(code, Region::Comment);
            }
            Ok(())
        })?;
        self.dest.notes.extend(notes);
        Ok(())
    }

    fn read_comment_bodies(
        &mut self,
        definitions: &[Definition],
        sources: &mut [SourceComment],
        threads: &CommentThreads,
        units: &BTreeMap<String, ReferenceUnits>,
    ) -> Result<(), ConvertError> {
        for (index, (definition, source)) in definitions.iter().zip(sources).enumerate() {
            if !threads.includes(index) {
                continue;
            }
            let root = threads.aliases().get(&source.id).ok_or(ConvertError::Internal)?;
            let units = units.get(root).ok_or(ConvertError::Internal)?;
            let _reports = self.parse_ctx.scope(units.first);
            let (blocks, held) =
                self.parse_ctx.holding_reports(|| self.read_comment(definition))?;
            source.blocks = blocks;
            for unit in &units.all {
                let _reports = self.parse_ctx.scope(*unit);
                self.parse_ctx.report_held(&held)?;
            }
        }
        Ok(())
    }

    fn read_comment(&mut self, definition: &Definition) -> Result<Vec<Block>, ConvertError> {
        let bytes = self.bytes.get(definition.start..definition.end).unwrap_or_default();
        let mut story = Parser::new(self.parse_ctx, bytes);
        story.sides = None;
        story.comment_story = true;
        story.source_offset = definition.start;
        story.prelude = std::mem::take(&mut self.prelude);
        story.assets = std::mem::replace(&mut self.assets, AssetSink::new(self.parse_ctx));
        story.decoder.default_encoding = self.decoder.default_encoding;
        (story.codepage_declared, story.assumed_reported) = (true, self.assumed_reported);
        (story.deff, story.last_symbol) = (self.deff, self.last_symbol);
        story.state = CharState {
            side: None,
            hidden: false,
            deleted: false,
            inserted: false,
            suppress: false,
            omit: false,
            capture: Capture::None,
            in_note: false,
            ..definition.state
        };
        let read = story.run().and_then(|()| story.finish_blocks());
        self.prelude = std::mem::take(&mut story.prelude);
        self.assets = std::mem::replace(&mut story.assets, AssetSink::new(self.parse_ctx));
        read
    }

    pub(super) fn import_comments(&mut self, story: &mut Self) {
        self.comments.definitions.append(&mut story.comments.definitions);
    }

    fn comment_units(
        &self,
        blocks: &[Block],
    ) -> Result<BTreeMap<String, ReferenceUnits>, ConvertError> {
        comments::reference_units(
            blocks,
            &self.dest.notes,
            |_| None,
            |work| self.parse_ctx.charge(work),
        )?
        .into_iter()
        .map(|(id, units)| {
            self.parse_ctx.charge(id.len() as u64)?;
            Ok((id.to_owned(), units))
        })
        .collect()
    }

    pub(super) fn side_note_reference(&mut self) -> Result<bool, ConvertError> {
        if !self.comment_story && self.state.side.is_none() {
            return Ok(false);
        }
        self.flush_pending();
        self.omit_deleted();
        self.omit_hidden();
        if self.renders() && !self.state.omit {
            let position = self.source_offset.saturating_add(self.lexer.pos());
            let id = format!("rtf:side-note:{position}");
            self.parse_ctx.charge(id.len() as u64)?;
            self.inlines.push(Inline::NoteRef(id));
        }
        self.state.suppress = true;
        self.state.omit = true;
        self.state.capture = Capture::None;
        Ok(true)
    }
}

fn metadata(
    ctx: &ParseCtx<'_>,
    definitions: &[Definition],
) -> Result<Vec<SourceComment>, ConvertError> {
    let mut authors: BTreeMap<&str, Option<&str>> = BTreeMap::new();
    for definition in definitions {
        if let Some(author) = &definition.author_key {
            ctx.charge(author.len() as u64 + 1)?;
            authors
                .entry(author)
                .and_modify(|parent| *parent = None)
                .or_insert(Some(&definition.id));
        }
    }
    definitions
        .iter()
        .map(|definition| {
            ctx.charge(definition.id.len() as u64)?;
            let parent = definition
                .parent_key
                .as_ref()
                .map(|key| -> Result<String, ConvertError> {
                    ctx.charge(key.len() as u64)?;
                    if let Some(id) = authors.get(key.as_str()).copied().flatten() {
                        ctx.charge(id.len() as u64)?;
                        Ok(id.to_owned())
                    } else {
                        let id = format!("rtf:unresolved-parent:{}", definition.start);
                        ctx.charge(id.len() as u64)?;
                        Ok(id)
                    }
                })
                .transpose()?;
            Ok(SourceComment { id: definition.id.clone(), parent, blocks: Vec::new() })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::ctx::ParseCtx;
    use crate::diagnostics::{DiagnosticCode, Region};
    use crate::formats::rtf::{Parser, parse};
    use crate::model::{Block, Inline};
    use crate::shared::test_support::diagnostic_rows;
    use crate::{CommentPolicy, HeaderFooterPolicy, Limits};
    use std::fmt::Write as _;

    type Reported = (DiagnosticCode, Region, Option<u32>, u64);

    #[test]
    fn same_author_comments_preserve_each_distinct_body() {
        let source =
            br"{\rtf1 {\*\atnid JD}{\*\annotation First}{\*\atnid JD}{\*\annotation Second}}";
        let ctx = ParseCtx::default().with_comments(CommentPolicy::Emit);
        let document = parse(&ctx, source).unwrap();
        assert_eq!(document.notes.len(), 2);
        assert_ne!(document.notes[0].id, document.notes[1].id);
        assert_eq!(document.notes[0].blocks, [Block::Paragraph(vec![Inline::plain("First")])]);
        assert_eq!(document.notes[1].blocks, [Block::Paragraph(vec![Inline::plain("Second")])]);
        assert_eq!(diagnostic_rows::<Reported>(&ctx.into_diagnostics()), []);
    }

    #[test]
    fn same_author_comments_keep_each_bodys_damage_and_images() {
        let source = br"{\rtf1 {\*\atnid JD}{\*\annotation First}{\*\atnid JD}{\*\annotation \u56320?{\pict\pngblip 89504e47}Second}}";
        let ctx = ParseCtx::default().with_comments(CommentPolicy::Emit);
        let document = parse(&ctx, source).unwrap();
        assert_eq!(document.notes.len(), 2);
        assert_eq!(document.assets.len(), 1);
        assert_eq!(document.assets[0].bytes, [0x89, 0x50, 0x4e, 0x47]);
        assert!(format!("{:?}", document.notes[1].blocks).contains("Second"));
        assert_eq!(
            diagnostic_rows::<Reported>(&ctx.into_diagnostics()),
            [(DiagnosticCode::InvalidEncoding, Region::Comment, None, 1)]
        );
    }

    #[test]
    fn same_author_comments_preserve_body_and_header_sources() {
        for (source, expected) in [
            (
                r"{\rtf1 {\header {\*\atnid JD}{\*\annotation Header}}Body{\*\atnid JD}{\*\annotation Main}}",
                ["Header", "Main"],
            ),
            (
                r"{\rtf1 Body{\*\atnid JD}{\*\annotation Main}{\header {\*\atnid JD}{\*\annotation Header}}}",
                ["Main", "Header"],
            ),
        ] {
            let ctx = ParseCtx::default()
                .with_comments(CommentPolicy::Emit)
                .with_headers_footers(HeaderFooterPolicy::Emit);
            let document = parse(&ctx, source.as_bytes()).unwrap();
            assert_eq!(document.notes.len(), 2);
            for (note, expected) in document.notes.iter().zip(expected) {
                assert_eq!(note.blocks, [Block::Paragraph(vec![Inline::plain(expected)])]);
            }
            assert_eq!(diagnostic_rows::<Reported>(&ctx.into_diagnostics()), []);
        }
    }

    #[test]
    fn unpublished_captures_do_not_admit_their_reply_graph() {
        let mut source = String::from("{\\rtf1 ");
        for index in 0..12 {
            let parent = if index == 0 {
                String::new()
            } else {
                format!(r"{{\*\atnparent c{}}}", index - 1)
            };
            write!(source, r"{{\*\atnid c{index}}}{{\*\annotation {parent}body}}").unwrap();
        }
        source.push('}');
        let limits = Limits { max_nesting_depth: 5, ..Limits::DEFAULT };
        let ctx = ParseCtx::new(&limits).with_comments(CommentPolicy::Emit);
        let mut parser = Parser::new(&ctx, source.as_bytes());
        parser.run().unwrap();
        parser.read_comments(&mut []).unwrap();
        assert_eq!(parser.dest.notes, []);
        assert_eq!(diagnostic_rows::<Reported>(&ctx.into_diagnostics()), []);
    }

    #[test]
    fn unpublished_header_author_reuse_does_not_read_its_body() {
        let source = br"{\rtf1 {\headerf {\*\atnid same}{\*\annotation \u56320?{\pict\pngblip 89504e47}}}Body{\*\atnid same}{\*\annotation Main}}";
        let limits = Limits { max_asset_total_bytes: 0, ..Limits::DEFAULT };
        let ctx = ParseCtx::new(&limits)
            .with_comments(CommentPolicy::Emit)
            .with_headers_footers(HeaderFooterPolicy::Emit);
        let document = parse(&ctx, source).unwrap();
        assert_eq!(document.notes.len(), 1);
        assert_eq!(document.notes[0].blocks, [Block::Paragraph(vec![Inline::plain("Main")])]);
        assert_eq!(document.assets, []);
        assert_eq!(diagnostic_rows::<Reported>(&ctx.into_diagnostics()), []);
    }

    #[test]
    fn visible_replies_can_select_an_unpublished_header_parent() {
        let source = br"{\rtf1 {\headerf Unpublished{\*\atnid root}{\*\annotation Parent}}Body{\*\atnid reply}{\*\annotation {\*\atnparent {\*\atnid root}}Reply}}";
        for policy in [HeaderFooterPolicy::Omit, HeaderFooterPolicy::Emit] {
            let ctx =
                ParseCtx::default().with_comments(CommentPolicy::Emit).with_headers_footers(policy);
            let document = parse(&ctx, source).unwrap();
            assert_eq!(document.notes.len(), 1);
            assert_eq!(
                document.notes[0].blocks,
                [
                    Block::Paragraph(vec![Inline::plain("Parent")]),
                    Block::BlockQuote(vec![Block::Paragraph(vec![Inline::plain("Reply")])])
                ]
            );
            let expected = if policy == HeaderFooterPolicy::Omit {
                vec![(DiagnosticCode::HeaderOmitted, Region::Header, None, 1)]
            } else {
                vec![]
            };
            assert_eq!(diagnostic_rows::<Reported>(&ctx.into_diagnostics()), expected);
            assert!(!format!("{:?}", document.blocks).contains("Unpublished"));
        }
    }

    #[test]
    fn cached_headers_reuse_one_source_definition_asset_and_loss() {
        let source = br"{\rtf1 {\header Head{\*\annotation \u56320?{\pict\pngblip 89504e47}Comment}}First\par\sect Second\par\sect Third\par}";
        let ctx = ParseCtx::default()
            .with_comments(CommentPolicy::Emit)
            .with_headers_footers(HeaderFooterPolicy::Emit);
        let document = parse(&ctx, source).unwrap();
        assert_eq!(document.notes.len(), 1);
        assert_eq!(document.assets.len(), 1);
        assert_eq!(document.assets[0].bytes, [0x89, 0x50, 0x4e, 0x47]);
        assert_eq!(
            diagnostic_rows::<Reported>(&ctx.into_diagnostics()),
            [(DiagnosticCode::InvalidEncoding, Region::Comment, None, 1)]
        );
    }

    #[test]
    fn isolated_side_references_never_read_nested_note_or_comment_bodies() {
        let source = br"{\rtf1 {\header Head{\footnote {\pict\pngblip 89504e47}HeaderNote}}Body{\footnote Main}{\*\atnid root}{\*\annotation Outer{\footnote {\pict\pngblip 89504e47}CommentNote}{\*\atnid nested}{\*\annotation {\pict\pngblip 89504e47}Nested}}}";
        let limits = Limits { max_asset_total_bytes: 0, ..Limits::DEFAULT };
        let ctx = ParseCtx::new(&limits)
            .with_comments(CommentPolicy::Emit)
            .with_headers_footers(HeaderFooterPolicy::Emit);
        let document = parse(&ctx, source).unwrap();
        assert_eq!(document.notes.len(), 2);
        assert_eq!(document.assets, []);
        assert!(!format!("{:?}", document.notes).contains("CommentNote"));
        let diagnostics =
            crate::converter::diagnostics_of(ctx, &document, crate::ImagePolicy::Placeholder)
                .unwrap();
        assert_eq!(
            diagnostic_rows::<Reported>(&diagnostics),
            [
                (DiagnosticCode::DanglingReference, Region::Header, None, 1),
                (DiagnosticCode::DanglingReference, Region::Comment, None, 2)
            ]
        );
    }

    #[test]
    fn selected_comment_asset_limits_and_cancellation_propagate() {
        let source = br"{\rtf1 Body{\*\annotation {\pict\pngblip 89504e47}}}";
        let limits = Limits { max_asset_total_bytes: 0, ..Limits::DEFAULT };
        let ctx = ParseCtx::new(&limits).with_comments(CommentPolicy::Emit);
        assert!(matches!(
            parse(&ctx, source),
            Err(crate::ConvertError::ResourceLimit {
                limit: crate::Limit::AssetTotalBytes,
                max: 0
            })
        ));
        let cancel = crate::CancelToken::default();
        cancel.cancel();
        let ctx = ParseCtx::default().with_comments(CommentPolicy::Emit).with_cancel(Some(&cancel));
        assert!(matches!(parse(&ctx, source), Err(crate::ConvertError::Cancelled)));
    }

    #[test]
    fn ambiguous_author_parent_keys_preserve_all_bodies_without_guessing() {
        let source = br"{\rtf1 {\*\atnid JD}{\*\annotation First}{\*\atnid JD}{\*\annotation Second}{\*\atnid RS}{\*\annotation {\*\atnparent {\*\atnid JD}}Reply}}";
        let ctx = ParseCtx::default().with_comments(CommentPolicy::Emit);
        let document = parse(&ctx, source).unwrap();
        assert_eq!(document.notes.len(), 3);
        for (note, text) in document.notes.iter().zip(["First", "Second", "Reply"]) {
            assert_eq!(note.blocks, [Block::Paragraph(vec![Inline::plain(text)])]);
        }
        assert_eq!(
            diagnostic_rows::<Reported>(&ctx.into_diagnostics()),
            [(DiagnosticCode::DanglingReference, Region::Comment, None, 1)]
        );
    }

    #[test]
    fn unrelated_unpublished_header_threads_do_not_admit_graph_depth() {
        let mut source = String::from("{\\rtf1 {\\headerf ");
        for index in 0..12 {
            let parent = if index == 0 {
                String::new()
            } else {
                format!(r"{{\*\atnparent h{}}}", index - 1)
            };
            write!(source, r"{{\*\atnid h{index}}}{{\*\annotation {parent}body}}").unwrap();
        }
        source.push_str(r"}Visible{\*\atnid selected}{\*\annotation kept}}");
        let limits = Limits { max_nesting_depth: 5, ..Limits::DEFAULT };
        let ctx = ParseCtx::new(&limits)
            .with_comments(CommentPolicy::Emit)
            .with_headers_footers(HeaderFooterPolicy::Emit);
        let document = parse(&ctx, source.as_bytes()).unwrap();
        assert_eq!(document.notes.len(), 1);
        assert_eq!(document.notes[0].blocks, [Block::Paragraph(vec![Inline::plain("kept")])]);
        assert_eq!(diagnostic_rows::<Reported>(&ctx.into_diagnostics()), []);
    }
}

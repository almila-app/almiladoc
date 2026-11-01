use super::styles::OdfStyles;
use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::limits::{Limit, Limits};
use crate::package::xml::{Attr, ContentSelection, DeferredFault, Element, Node, ns};
use crate::shared::delta::StyleDelta;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;

const MAX_CHANGE_REFERENCES: usize = 64;

type Scope = *const Element;
type Key<'a> = (Scope, &'a str);
type Damage = [(usize, Range<usize>)];
type FieldCache = Rc<RefCell<HashMap<Scope, super::text::Fields>>>;

#[derive(Clone, Copy)]
enum Token<'a> {
    Open(&'a Element),
    Close(&'a Element),
    Text(&'a str, &'a Damage, &'a Element),
    Omitted,
    Hidden { count: u64, block: bool },
    Fault(DeferredFault),
    Pending(Request<'a>),
    Reference(Reference<'a>),
}

#[derive(Clone, Copy)]
enum ReferenceKind {
    Story,
    Deletion,
    InlineDeletion,
}

impl ReferenceKind {
    fn change(self) -> bool {
        !matches!(self, Self::Story)
    }
    fn inline(self) -> bool {
        matches!(self, Self::InlineDeletion)
    }
}

#[derive(Clone, Copy)]
struct Reference<'a> {
    plan: usize,
    source: &'a Element,
    kind: ReferenceKind,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum PlanKey<'a> {
    Change(Key<'a>),
    Story(Scope),
}

#[derive(Clone, Copy)]
struct Request<'a> {
    source: &'a Element,
    scope: Option<Scope>,
    id: Option<&'a str>,
    inline: bool,
}

impl<'a> Request<'a> {
    fn key(self) -> Result<PlanKey<'a>, ConvertError> {
        match self.id {
            Some(id) => Ok(PlanKey::Change((self.scope.ok_or(ConvertError::Internal)?, id))),
            None => Ok(PlanKey::Story(std::ptr::from_ref(self.source))),
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Metrics<'a> {
    len: u64,
    nodes: u64,
    baseline: u64,
    references: u64,
    first: Option<(u64, &'a Element)>,
    last: Option<(u64, &'a Element)>,
    overflow: bool,
    fields: bool,
}

impl Metrics<'_> {
    fn add(&mut self, other: Self) {
        if self.first.is_none() {
            self.first = other.first.map(|(at, e)| (self.len.saturating_add(at), e));
        }
        if let Some((at, elem)) = other.last {
            self.last = Some((self.len.saturating_add(at), elem));
        }
        self.overflow |= other.overflow
            || self.len.checked_add(other.len).is_none()
            || self.nodes.checked_add(other.nodes).is_none();
        self.len = self.len.saturating_add(other.len);
        self.nodes = self.nodes.saturating_add(other.nodes);
        self.baseline = self.baseline.saturating_add(other.baseline);
        self.references = self.references.max(other.references);
        self.fields |= other.fields;
    }
}

#[derive(Clone, Copy)]
struct Window<'a> {
    start: u64,
    end: u64,
    metrics: Metrics<'a>,
}

struct Plan<'a> {
    tokens: Vec<Token<'a>>,
    full: Window<'a>,
    inline: Window<'a>,
}

impl<'a> Plan<'a> {
    fn window(&self, inline: bool) -> Window<'a> {
        if inline { self.inline } else { self.full }
    }
}

enum Cached {
    Building,
    Ready(usize),
}

struct Draft<'a> {
    request: Request<'a>,
    tokens: Vec<Token<'a>>,
    next: usize,
}

#[derive(Clone, Copy)]
enum Change<'a> {
    Insertion,
    Deletion(&'a Element),
    Formatting,
    Invalid,
}

#[derive(Default)]
struct State<'a> {
    active: HashSet<&'a str>,
    omitted: bool,
    selected: u64,
    omissions: u64,
}

impl State<'_> {
    fn finish(&self) -> Result<(), ConvertError> {
        if self.active.is_empty() { Ok(()) } else { Err(malformed("unclosed insertion range")) }
    }
}

#[derive(Default)]
struct InsertionRanges<'a> {
    active: HashMap<&'a str, usize>,
    starts: Vec<(&'a Element, bool)>,
}

impl<'a> InsertionRanges<'a> {
    fn marker(&mut self, elem: &'a Element, id: &'a str) -> Option<DeferredFault> {
        if elem.local == "change-start" {
            if self.active.contains_key(id) {
                return Some(DeferredFault::Malformed("duplicate insertion start"));
            }
            self.active.insert(id, self.starts.len());
            self.starts.push((elem, false));
        } else if elem.local == "change-end" {
            let Some(at) = self.active.remove(id) else {
                return Some(DeferredFault::Malformed("insertion end has no start"));
            };
            if let Some((_, closed)) = self.starts.get_mut(at) {
                *closed = true;
            }
        }
        None
    }
}

struct Selector<'a, 'c> {
    ctx: &'c ParseCtx<'c>,
    scopes: HashMap<Scope, HashMap<&'a str, Change<'a>>>,
    nearest: HashMap<Scope, Option<Scope>>,
    parents: HashMap<Scope, &'a Element>,
    range_faults: HashMap<Scope, DeferredFault>,
    cache: HashMap<PlanKey<'a>, Cached>,
    plans: Vec<Plan<'a>>,
    compiled_changes: usize,
    draft_root: Option<Scope>,
}

pub(super) struct StoryViews<'a, 'c> {
    selector: Selector<'a, 'c>,
    styles: Rc<OdfStyles<'a>>,
}

impl<'a, 'c> StoryViews<'a, 'c> {
    #[cfg(test)]
    pub(super) fn new(
        ctx: &'c ParseCtx<'c>,
        trees: impl Iterator<Item = &'a Element>,
    ) -> Result<Self, ConvertError> {
        let trees: Vec<_> = trees.collect();
        ctx.charge(trees.len() as u64)?;
        let mut styles = OdfStyles::default();
        if ctx.reject_revisions() {
            for tree in
                trees.iter().filter(|tree| tree.find(ns::OFFICE, "document-styles").is_some())
            {
                styles.collect(tree);
            }
            for tree in
                trees.iter().filter(|tree| tree.find(ns::OFFICE, "document-styles").is_none())
            {
                styles.collect(tree);
            }
        }
        Self::with_styles(ctx, trees.into_iter(), Rc::new(styles))
    }

    pub(super) fn with_styles(
        ctx: &'c ParseCtx<'c>,
        trees: impl Iterator<Item = &'a Element>,
        styles: Rc<OdfStyles<'a>>,
    ) -> Result<Self, ConvertError> {
        let mut selector = Selector::new(ctx);
        if ctx.reject_revisions() {
            for tree in trees {
                selector.index(tree)?;
            }
        }
        Ok(Self { selector, styles })
    }

    pub(super) fn selected(&mut self, story: &'a Element) -> Result<Option<Element>, ConvertError> {
        if !self.selector.ctx.reject_revisions() || self.selector.scopes.is_empty() {
            return Ok(None);
        }
        let scope = self.selector.nearest.get(&std::ptr::from_ref(story)).copied().flatten();
        let request = Request { source: story, scope, id: None, inline: false };
        let plan = self.selector.compile(request)?;
        let tokens = emit_view(
            self.selector.ctx,
            &self.styles,
            &self.selector.plans,
            &self.selector.parents,
            story,
            plan,
        )?;
        rebuild(self.selector.ctx, &tokens).map(Some)
    }
}

#[cfg(test)]
pub(super) fn selected(
    ctx: &ParseCtx<'_>,
    tree: &Element,
) -> Result<Option<Element>, ConvertError> {
    if !ctx.reject_revisions() {
        return Ok(None);
    }
    StoryViews::new(ctx, std::iter::once(tree))?.selected(tree)
}

#[cfg(test)]
pub(super) fn select(ctx: &ParseCtx<'_>, tree: Element) -> Result<Element, ConvertError> {
    Ok(selected(ctx, &tree)?.unwrap_or(tree))
}

pub(super) fn accepted<'a>(
    ctx: &ParseCtx<'_>,
    trees: impl Iterator<Item = &'a Element>,
) -> Result<HashSet<*const Element>, ConvertError> {
    let mut accepted = HashSet::new();
    if ctx.reject_revisions() {
        return Ok(accepted);
    }
    for tree in trees {
        let mut selector = Selector::new(ctx);
        selector.index(tree)?;
        if selector.scopes.is_empty() {
            continue;
        }
        let mut eligible = HashMap::new();
        let mut pending = vec![(tree, None)];
        while let Some((elem, inherited)) = pending.pop() {
            ctx.charge(1)?;
            if elem.is(ns::TEXT, "tracked-changes") {
                continue;
            }
            let scope = if selector.scopes.contains_key(&std::ptr::from_ref(elem)) {
                Some(std::ptr::from_ref(elem))
            } else {
                inherited
            };
            if elem.is(ns::TEXT, "change")
                && let (Some(scope), Some(id)) = (scope, elem.attr(ns::TEXT, "change-id"))
            {
                ctx.charge(id.len() as u64 + 1)?;
                let key = (scope, id);
                let holds = if let Some(holds) = eligible.get(&key) {
                    *holds
                } else {
                    let holds = match selector.scopes.get(&scope).and_then(|m| m.get(id)) {
                        Some(Change::Deletion(deletion)) => deletion_content(ctx, deletion)?,
                        _ => false,
                    };
                    eligible.insert(key, holds);
                    holds
                };
                if holds {
                    accepted.insert(std::ptr::from_ref(elem));
                }
            }
            pending.extend(elem.child_elems().map(|child| (child, scope)));
        }
    }
    Ok(accepted)
}

fn deletion_content(ctx: &ParseCtx<'_>, deletion: &Element) -> Result<bool, ConvertError> {
    let mut pending = vec![deletion];
    let mut paragraphs = 0;
    while let Some(elem) = pending.pop() {
        ctx.charge(1)?;
        if elem.is(ns::OFFICE, "change-info")
            || (elem.is(ns::OFFICE, "annotation") || elem.is(super::OFFICEOOO, "annotation"))
            || elem.is(ns::TEXT, "tracked-changes")
        {
            continue;
        }
        if paragraph_elem(elem) {
            paragraphs += 1;
            if paragraphs > 1 {
                return Ok(true);
            }
        }
        if elem.is(ns::DRAW, "image") || elem.is(ns::TEXT, "note") {
            return Ok(true);
        }
        for node in &elem.children {
            ctx.charge(1)?;
            match node {
                Node::Elem(child) => pending.push(child),
                Node::Text(text) => {
                    ctx.charge(text.len() as u64)?;
                    if !crate::shared::text::clean_text(text).trim().is_empty() {
                        return Ok(true);
                    }
                }
            }
        }
    }
    Ok(false)
}

pub(super) fn omitted(elem: &Element) -> bool {
    matches!(elem.selection.as_deref(), Some(ContentSelection::Omitted))
}

pub(super) fn fault(elem: &Element) -> Option<DeferredFault> {
    match elem.selection.as_deref() {
        Some(ContentSelection::Fault(fault)) => Some(*fault),
        _ => None,
    }
}

impl<'a, 'c> Selector<'a, 'c> {
    fn new(ctx: &'c ParseCtx<'c>) -> Self {
        Self {
            ctx,
            scopes: HashMap::new(),
            nearest: HashMap::new(),
            parents: HashMap::new(),
            range_faults: HashMap::new(),
            cache: HashMap::new(),
            plans: Vec::new(),
            compiled_changes: 0,
            draft_root: None,
        }
    }

    fn index(&mut self, tree: &'a Element) -> Result<(), ConvertError> {
        for scope in std::iter::once(tree).chain(tree.descendant_elems()) {
            self.ctx.charge(1)?;
            let mut changes = HashMap::new();
            let mut declared = false;
            for tracked in scope.find_all(ns::TEXT, "tracked-changes") {
                declared = true;
                for region in tracked.find_all(ns::TEXT, "changed-region") {
                    self.ctx.charge(1)?;
                    let Some(id) = region
                        .attr_qualified(ns::XML, "id")
                        .or_else(|| region.attr(ns::TEXT, "id"))
                    else {
                        continue;
                    };
                    self.ctx.charge(id.len() as u64)?;
                    let change = match region.child_elems().next() {
                        Some(e) if e.is(ns::TEXT, "insertion") => Change::Insertion,
                        Some(e) if e.is(ns::TEXT, "deletion") => Change::Deletion(e),
                        Some(e) if e.is(ns::TEXT, "format-change") => Change::Formatting,
                        _ => Change::Invalid,
                    };
                    changes.entry(id).and_modify(|e| *e = Change::Invalid).or_insert(change);
                }
            }
            if declared {
                self.scopes.insert(std::ptr::from_ref(scope), changes);
            }
        }
        if self.ctx.reject_revisions() {
            self.index_paths(tree)?;
        }
        Ok(())
    }

    fn index_paths(&mut self, tree: &'a Element) -> Result<(), ConvertError> {
        let mut ranges = vec![InsertionRanges::default()];
        let mut pending = vec![(tree, None, None, 0)];
        while let Some((elem, parent, inherited, outer_story)) = pending.pop() {
            self.ctx.charge(1)?;
            let key = std::ptr::from_ref(elem);
            if let Some(parent) = parent {
                self.parents.insert(key, parent);
            }
            let own_scope = self.scopes.contains_key(&key);
            let scope = if own_scope { Some(key) } else { inherited };
            self.nearest.insert(key, scope);
            let story = if own_scope || independent_story(elem) || elem.is(ns::TEXT, "deletion") {
                ranges.push(InsertionRanges::default());
                ranges.len() - 1
            } else {
                outer_story
            };
            self.index_marker(elem, scope, ranges.get_mut(story).ok_or(ConvertError::Internal)?)?;
            pending.extend(elem.child_elems().rev().map(|child| (child, Some(elem), scope, story)));
        }
        for range in ranges {
            for (start, closed) in range.starts {
                self.ctx.charge(1)?;
                if !closed {
                    self.range_faults.insert(
                        std::ptr::from_ref(start),
                        DeferredFault::Malformed("unclosed insertion range"),
                    );
                }
            }
        }
        Ok(())
    }

    fn index_marker(
        &mut self,
        elem: &'a Element,
        scope: Option<Scope>,
        ranges: &mut InsertionRanges<'a>,
    ) -> Result<(), ConvertError> {
        let (Some(scope), Some(id)) = (scope, elem.attr(ns::TEXT, "change-id")) else {
            return Ok(());
        };
        if !elem.is(ns::TEXT, "change-start") && !elem.is(ns::TEXT, "change-end") {
            return Ok(());
        }
        self.ctx.charge(id.len() as u64 + 1)?;
        if matches!(
            self.scopes.get(&scope).and_then(|changes| changes.get(id)),
            Some(Change::Insertion)
        ) && let Some(fault) = ranges.marker(elem, id)
        {
            self.range_faults.insert(std::ptr::from_ref(elem), fault);
        }
        Ok(())
    }

    fn push(&self, out: &mut Vec<Token<'a>>, token: Token<'a>) -> Result<(), ConvertError> {
        self.ctx.charge(1)?;
        out.push(token);
        Ok(())
    }

    fn omit(
        &mut self,
        state: &mut State<'a>,
        out: &mut Vec<Token<'a>>,
    ) -> Result<(), ConvertError> {
        if !state.omitted {
            self.push(out, Token::Omitted)?;
            state.omitted = true;
            state.omissions += 1;
        }
        Ok(())
    }

    fn walk(
        &mut self,
        elem: &'a Element,
        inherited: Option<Scope>,
        state: &mut State<'a>,
        depth: usize,
        paragraph: bool,
        out: &mut Vec<Token<'a>>,
    ) -> Result<(), ConvertError> {
        self.ctx.charge(1)?;
        if depth as u64 > Limits::MAX_NESTING_CEILING {
            return self.push(
                out,
                Token::Fault(DeferredFault::Refused {
                    limit: Limit::NestingDepth,
                    max: Limits::MAX_NESTING_CEILING,
                }),
            );
        }
        if elem.is(ns::TEXT, "tracked-changes") || elem.is(ns::OFFICE, "change-info") {
            return Ok(());
        }
        let own_scope = self.scopes.contains_key(&std::ptr::from_ref(elem));
        let scope = if own_scope { Some(std::ptr::from_ref(elem)) } else { inherited };
        if elem.ns.as_deref() == Some(ns::TEXT)
            && matches!(elem.local.as_str(), "change" | "change-start" | "change-end")
        {
            return self.marker(elem, scope, state, paragraph, out);
        }
        if independent_story(elem) && self.draft_root != Some(std::ptr::from_ref(elem)) {
            if state.active.is_empty() {
                self.push(
                    out,
                    Token::Pending(Request { source: elem, scope, id: None, inline: false }),
                )?;
                state.selected += 1;
            } else {
                self.omit(state, out)?;
            }
            return Ok(());
        }
        if own_scope || independent_story(elem) {
            if !state.active.is_empty() {
                if super::text::holds_content(elem) {
                    self.omit(state, out)?;
                }
                return Ok(());
            }
            let mut local = State::default();
            self.element(elem, scope, &mut local, depth, false, out)?;
            return local.finish();
        }
        self.element(elem, scope, state, depth, paragraph, out)
    }

    fn element(
        &mut self,
        elem: &'a Element,
        scope: Option<Scope>,
        state: &mut State<'a>,
        depth: usize,
        paragraph: bool,
        out: &mut Vec<Token<'a>>,
    ) -> Result<(), ConvertError> {
        let active_before = !state.active.is_empty();
        if active_before && atomic(elem) {
            if super::text::holds_content(elem) {
                self.omit(state, out)?;
            }
            return Ok(());
        }
        let start = out.len();
        let selected = state.selected;
        let omissions = state.omissions;
        let is_paragraph = paragraph_elem(elem);
        let boundary = is_paragraph || structural_boundary(elem);
        if !active_before {
            state.selected += 1;
        }
        if !boundary || !active_before {
            self.push(out, Token::Open(elem))?;
        }
        self.children(elem, scope, state, depth, paragraph || is_paragraph, out)?;
        if !boundary || state.active.is_empty() {
            self.push(out, Token::Close(elem))?;
        }
        if is_paragraph && !state.active.is_empty() {
            self.omit(state, out)?;
        }
        if active_before && !state.active.is_empty() && state.selected == selected {
            out.truncate(start);
            if state.omissions != omissions {
                self.push(out, Token::Omitted)?;
            }
        }
        Ok(())
    }

    fn children(
        &mut self,
        elem: &'a Element,
        scope: Option<Scope>,
        state: &mut State<'a>,
        depth: usize,
        paragraph: bool,
        out: &mut Vec<Token<'a>>,
    ) -> Result<(), ConvertError> {
        elem.validate_text_damage(self.ctx)?;
        let damage = elem.text_damage();
        let mut next_damage = 0;
        for (at, node) in elem.children.iter().enumerate() {
            self.ctx.charge(1)?;
            match node {
                Node::Elem(child) => {
                    self.walk(child, scope, state, depth + 1, paragraph, out)?;
                }
                Node::Text(text) => {
                    let first = next_damage;
                    while damage.get(next_damage).is_some_and(|(child, _)| *child == at) {
                        self.ctx.charge(1)?;
                        next_damage += 1;
                    }
                    if state.active.is_empty() {
                        state.selected += 1;
                        self.push(out, Token::Text(text, &damage[first..next_damage], elem))?;
                    } else {
                        self.ctx.charge(text.len() as u64)?;
                        if !text.trim().is_empty() {
                            self.omit(state, out)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn change(&self, scope: Scope, id: &str) -> Result<Change<'a>, ConvertError> {
        self.ctx.charge(id.len() as u64 + 1)?;
        match self.scopes.get(&scope).and_then(|m| m.get(id)).copied() {
            Some(Change::Invalid) | None => Err(malformed("unknown or ambiguous tracked change")),
            Some(change) => Ok(change),
        }
    }

    fn marker(
        &mut self,
        elem: &'a Element,
        scope: Option<Scope>,
        state: &mut State<'a>,
        paragraph: bool,
        out: &mut Vec<Token<'a>>,
    ) -> Result<(), ConvertError> {
        if let Some(fault) = self.range_faults.get(&std::ptr::from_ref(elem)).copied() {
            if state.active.is_empty() {
                self.push(out, Token::Fault(fault))?;
            }
            return Ok(());
        }
        let Some(scope) = scope else { return Ok(()) };
        let Some(id) = elem.attr(ns::TEXT, "change-id") else {
            return self
                .push(out, Token::Fault(DeferredFault::Malformed("tracked change has no id")));
        };
        if elem.local == "change" && !state.active.is_empty() {
            return Ok(());
        }
        let change = match self.change(scope, id) {
            Ok(change) => change,
            Err(ConvertError::Malformed { reason, .. }) if state.active.is_empty() => {
                return self.push(out, Token::Fault(DeferredFault::Malformed(reason)));
            }
            Err(error) if error.is_fatal() => return Err(error),
            Err(_) => return Ok(()),
        };
        match (elem.local.as_str(), change) {
            ("change-start", Change::Insertion) => {
                self.ctx.charge(id.len() as u64 + 1)?;
                if !state.active.insert(id) {
                    return Err(malformed("duplicate insertion start"));
                }
            }
            ("change-end", Change::Insertion) => {
                self.ctx.charge(id.len() as u64 + 1)?;
                if !state.active.remove(id) {
                    return Err(malformed("insertion end has no start"));
                }
                if state.active.is_empty() {
                    state.omitted = false;
                }
            }
            ("change", Change::Deletion(deletion)) => {
                self.push(
                    out,
                    Token::Pending(Request {
                        source: deletion,
                        scope: Some(scope),
                        id: Some(id),
                        inline: paragraph,
                    }),
                )?;
                state.selected += 1;
            }
            (_, Change::Formatting) => {}
            _ if !state.active.is_empty() => {}
            _ => self.push(
                out,
                Token::Fault(DeferredFault::Malformed("tracked change marker has the wrong kind")),
            )?,
        }
        Ok(())
    }

    fn draft(&mut self, request: Request<'a>) -> Result<Draft<'a>, ConvertError> {
        let saved = self.draft_root.replace(std::ptr::from_ref(request.source));
        let mut tokens = Vec::new();
        let mut state = State::default();
        let result = if request.id.is_some() {
            self.children(request.source, request.scope, &mut state, 0, false, &mut tokens)
        } else {
            self.walk(request.source, request.scope, &mut state, 0, false, &mut tokens)
        };
        self.draft_root = saved;
        result?;
        state.finish()?;
        Ok(Draft { request, tokens, next: 0 })
    }

    fn compile(&mut self, request: Request<'a>) -> Result<usize, ConvertError> {
        let key = request.key()?;
        if let Some(Cached::Ready(plan)) = self.cache.get(&key) {
            return Ok(*plan);
        }
        self.cache.insert(key, Cached::Building);
        let mut pending = vec![self.draft(request)?];
        while let Some(frame) = pending.last_mut() {
            self.ctx.charge(1)?;
            let Some(token) = frame.tokens.get(frame.next).copied() else {
                let frame = pending.pop().ok_or(ConvertError::Internal)?;
                let key = frame.request.key()?;
                let plan = self.finish_plan(frame.tokens)?;
                if frame.request.id.is_some() {
                    self.compiled_changes += 1;
                }
                self.cache.insert(key, Cached::Ready(plan));
                if pending.is_empty() {
                    return Ok(plan);
                }
                continue;
            };
            let Token::Pending(request) = token else {
                frame.next += 1;
                continue;
            };
            self.ctx.charge(request.id.map_or(1, |id| id.len() as u64 + 1))?;
            let key = request.key()?;
            let replacement = match self.cache.get(&key) {
                Some(Cached::Ready(plan)) => self.reference(request, *plan)?,
                Some(Cached::Building) => {
                    Token::Fault(DeferredFault::Malformed("cyclic tracked deletion"))
                }
                None => {
                    self.cache.insert(key, Cached::Building);
                    pending.push(self.draft(request)?);
                    continue;
                }
            };
            *frame.tokens.get_mut(frame.next).ok_or(ConvertError::Internal)? = replacement;
            frame.next += 1;
        }
        Err(ConvertError::Internal)
    }

    fn reference(&self, request: Request<'a>, plan: usize) -> Result<Token<'a>, ConvertError> {
        let window = self.plans.get(plan).ok_or(ConvertError::Internal)?.window(request.inline);
        let max = self.ctx.limits().max_xml_nodes;
        if request.id.is_some() && (window.metrics.overflow || window.metrics.nodes > max) {
            return Ok(Token::Fault(DeferredFault::Refused { limit: Limit::XmlNodes, max }));
        }
        if request.id.is_some() && window.metrics.references >= MAX_CHANGE_REFERENCES as u64 {
            return Ok(Token::Fault(DeferredFault::Refused {
                limit: Limit::NestingDepth,
                max: MAX_CHANGE_REFERENCES as u64,
            }));
        }
        Ok(Token::Reference(Reference {
            plan,
            source: request.source,
            kind: if request.id.is_none() {
                ReferenceKind::Story
            } else if request.inline {
                ReferenceKind::InlineDeletion
            } else {
                ReferenceKind::Deletion
            },
        }))
    }

    fn token_metrics(&self, token: Token<'a>) -> Result<Metrics<'a>, ConvertError> {
        Ok(match token {
            Token::Open(elem) => Metrics {
                len: 1,
                nodes: u64::from(!elem.local.is_empty()),
                baseline: u64::from(!elem.local.is_empty()),
                first: paragraph_elem(elem).then_some((0, elem)),
                fields: elem.is(ns::TEXT, "hidden-paragraph"),
                ..Metrics::default()
            },
            Token::Close(elem) => Metrics {
                len: 1,
                last: paragraph_elem(elem).then_some((0, elem)),
                ..Metrics::default()
            },
            Token::Text(..) | Token::Omitted | Token::Hidden { .. } | Token::Fault(_) => {
                Metrics { len: 1, nodes: 1, baseline: 1, ..Metrics::default() }
            }
            Token::Reference(Reference { plan, kind, .. }) => {
                let mut metrics = self
                    .plans
                    .get(plan)
                    .ok_or(ConvertError::Internal)?
                    .window(kind.inline())
                    .metrics;
                metrics.references = metrics.references.saturating_add(u64::from(kind.change()));
                if kind.change() {
                    metrics.baseline = 1;
                } else {
                    metrics.fields = false;
                }
                metrics
            }
            Token::Pending(_) => return Err(ConvertError::Internal),
        })
    }

    fn finish_plan(&mut self, tokens: Vec<Token<'a>>) -> Result<usize, ConvertError> {
        let mut full = Metrics::default();
        for token in &tokens {
            self.ctx.charge(1)?;
            full.add(self.token_metrics(*token)?);
        }
        let (start, end) = match (full.first, full.last) {
            (Some((first, _)), Some((last, _))) if first < last => (first + 1, last),
            _ => (0, full.len),
        };
        let metrics = if full.overflow { full } else { self.window_metrics(&tokens, start, end)? };
        let plan = self.plans.len();
        self.plans.push(Plan {
            tokens,
            full: Window { start: 0, end: full.len, metrics: full },
            inline: Window { start, end, metrics },
        });
        Ok(plan)
    }

    fn window_metrics(
        &self,
        tokens: &[Token<'a>],
        start: u64,
        end: u64,
    ) -> Result<Metrics<'a>, ConvertError> {
        let mut total = Metrics::default();
        let mut pending = vec![(tokens, start, end, 0, 0u64, 0u64)];
        while let Some((tokens, start, end, next, offset, depth)) = pending.last_mut() {
            let Some(token) = tokens.get(*next).copied() else {
                pending.pop();
                continue;
            };
            self.ctx.charge(1)?;
            let metrics = self.token_metrics(token)?;
            let token_start = *offset;
            *offset = offset.saturating_add(metrics.len);
            *next += 1;
            let from = (*start).max(token_start);
            let to = (*end).min(*offset);
            if from >= to {
                continue;
            }
            if from == token_start && to == *offset {
                let mut metrics = metrics;
                metrics.references = metrics.references.saturating_add(*depth);
                total.add(metrics);
            } else if let Token::Reference(Reference { plan, kind, .. }) = token {
                let child = self.plans.get(plan).ok_or(ConvertError::Internal)?;
                let window = child.window(kind.inline());
                let depth = depth.saturating_add(u64::from(kind.change()));
                pending.push((
                    &child.tokens,
                    window.start + from - token_start,
                    window.start + to - token_start,
                    0,
                    0,
                    depth,
                ));
            } else {
                return Err(ConvertError::Internal);
            }
        }
        Ok(total)
    }
}

fn paragraph_elem(elem: &Element) -> bool {
    elem.ns.as_deref() == Some(ns::TEXT) && matches!(elem.local.as_str(), "p" | "h")
}

fn structural_boundary(elem: &Element) -> bool {
    elem.ns.as_deref() == Some(ns::TEXT)
        && matches!(elem.local.as_str(), "list" | "list-item" | "list-header" | "section")
}

fn independent_story(elem: &Element) -> bool {
    elem.is(ns::TEXT, "note-body")
        || elem.is(ns::DRAW, "text-box")
        || elem.is(ns::OFFICE, "annotation")
        || elem.is(super::OFFICEOOO, "annotation")
        || elem.is(ns::TABLE, "table-cell")
        || elem.is(ns::TABLE, "covered-table-cell")
        || elem.is(ns::OFFICE, "text")
        || elem.ns.as_deref() == Some(ns::STYLE)
            && matches!(
                elem.local.as_str(),
                "header"
                    | "header-left"
                    | "header-first"
                    | "footer"
                    | "footer-left"
                    | "footer-first"
            )
}

fn atomic(elem: &Element) -> bool {
    elem.is(ns::DRAW, "frame")
        || elem.is(ns::TEXT, "note")
        || (elem.ns.as_deref() == Some(ns::TEXT)
            && matches!(elem.local.as_str(), "s" | "tab" | "line-break"))
}

fn copied(ctx: &ParseCtx<'_>, value: &str) -> Result<String, ConvertError> {
    ctx.charge(value.len() as u64)?;
    Ok(value.to_string())
}

fn metadata(ctx: &ParseCtx<'_>, elem: &Element) -> Result<Element, ConvertError> {
    let mut attrs = Vec::with_capacity(elem.attrs.len());
    for attr in &elem.attrs {
        ctx.charge(1)?;
        attrs.push(Attr {
            ns: attr.ns.clone(),
            local: copied(ctx, &attr.local)?,
            value: copied(ctx, &attr.value)?,
            unresolved: attr.unresolved,
            value_damage: attr
                .value_damage
                .as_deref()
                .map(|spans| {
                    ctx.charge(spans.len() as u64)?;
                    Ok::<_, ConvertError>(Box::new(spans.clone()))
                })
                .transpose()?,
        });
    }
    Ok(Element {
        ns: elem.ns.clone(),
        local: copied(ctx, &elem.local)?,
        attrs,
        selection: elem
            .selection
            .as_deref()
            .map(|selection| {
                ctx.charge(1)?;
                Ok::<_, ConvertError>(Box::new(*selection))
            })
            .transpose()?,
        ..Element::default()
    })
    .and_then(|mut copied| {
        if copied.selection.is_none()
            && (elem.is(ns::OFFICE, "annotation") || elem.is(super::OFFICEOOO, "annotation"))
        {
            ctx.charge(1)?;
            copied.selection =
                Some(Box::new(ContentSelection::Source(std::ptr::from_ref(elem) as usize)));
        }
        Ok(copied)
    })
}

#[derive(Clone)]
struct Binding<'a> {
    source: &'a Element,
    base: usize,
    include: bool,
    aliases: Vec<(Scope, &'a Element)>,
}

#[derive(Clone, Copy, Default)]
struct Visibility {
    delta: StyleDelta,
    locked: bool,
    emitted: bool,
    original: bool,
    hidden_any: bool,
    hidden_holds: bool,
    position: u64,
    fields: super::text::Fields,
}

struct Checkpoint<'a> {
    stack: Vec<&'a Element>,
    visibility: Vec<Visibility>,
    suspended: Vec<&'a Element>,
    stories: Vec<(Scope, Vec<&'a Element>)>,
    bindings: Vec<Binding<'a>>,
    out: usize,
    nodes: u64,
    remaining: u64,
}

enum FlowError {
    Parse(ConvertError),
    Refused(DeferredFault),
}

impl From<ConvertError> for FlowError {
    fn from(error: ConvertError) -> Self {
        Self::Parse(error)
    }
}

type FlowResult<T> = Result<T, FlowError>;

struct Flow<'a, 'b> {
    ctx: &'b ParseCtx<'b>,
    styles: &'b OdfStyles<'a>,
    plans: &'b [Plan<'a>],
    parents: &'b HashMap<Scope, &'a Element>,
    root: &'a Element,
    stack: Vec<&'a Element>,
    visibility: Vec<Visibility>,
    suspended: Vec<&'a Element>,
    stories: Vec<(Scope, Vec<&'a Element>)>,
    bindings: Vec<Binding<'a>>,
    out: Vec<Token<'a>>,
    nodes: u64,
    remaining: u64,
    measuring: bool,
    baseline: u64,
    field_scan: bool,
    position: u64,
    profile: Option<Box<ProfileCursor<'a, 'b>>>,
    closed_profile: Option<(Scope, u64, bool)>,
    field_cache: FieldCache,
}

impl<'a, 'b> Flow<'a, 'b> {
    fn new(
        ctx: &'b ParseCtx<'b>,
        styles: &'b OdfStyles<'a>,
        plans: &'b [Plan<'a>],
        parents: &'b HashMap<Scope, &'a Element>,
        root: &'a Element,
        field_cache: Option<&FieldCache>,
    ) -> Self {
        Self {
            ctx,
            styles,
            plans,
            parents,
            root,
            stack: Vec::new(),
            visibility: Vec::new(),
            suspended: Vec::new(),
            stories: Vec::new(),
            bindings: Vec::new(),
            out: Vec::new(),
            nodes: 0,
            remaining: 0,
            measuring: true,
            baseline: 0,
            field_scan: false,
            position: 0,
            profile: None,
            closed_profile: None,
            field_cache: field_cache.map_or_else(FieldCache::default, Rc::clone),
        }
    }

    fn profile(
        &self,
        reference: Reference<'a>,
        range: Range<u64>,
        position: u64,
    ) -> FlowResult<Option<Box<ProfileCursor<'a, 'b>>>> {
        let plan = self.plans.get(reference.plan).ok_or(ConvertError::Internal)?;
        if self.ctx.hidden_requested() || !plan.full.metrics.fields {
            Ok(None)
        } else {
            self.ctx.charge(1)?;
            ProfileCursor::new(self, reference, range, position).map(Box::new).map(Some)
        }
    }

    fn story_base(&self) -> usize {
        let projected = self
            .bindings
            .iter()
            .rev()
            .find(|binding| binding.include)
            .map_or(0, |binding| binding.base);
        projected.max(self.stack.iter().rposition(|elem| independent_story(elem)).unwrap_or(0))
    }

    fn paragraph(&self) -> Option<usize> {
        let base = self.story_base();
        self.stack
            .iter()
            .enumerate()
            .rev()
            .find_map(|(at, elem)| (at >= base && paragraph_elem(elem)).then_some(at))
    }

    fn record(&mut self, token: Token<'a>, original: bool) -> FlowResult<()> {
        self.ctx.charge(1)?;
        let node = match token {
            Token::Close(_) | Token::Hidden { .. } => false,
            Token::Open(e) => !e.local.is_empty(),
            _ => true,
        };
        if node {
            if original {
                self.remaining = self.remaining.saturating_sub(1);
            }
            if original {
                self.baseline += 1;
            }
            let max = self.ctx.limits().max_xml_nodes;
            if !self.measuring && self.nodes.saturating_add(self.remaining) >= max {
                return Err(FlowError::Refused(DeferredFault::Refused {
                    limit: Limit::XmlNodes,
                    max,
                }));
            }
            self.nodes += 1;
        }
        if !self.measuring {
            self.out.push(token);
        }
        Ok(())
    }

    fn hidden(&mut self, holds: bool) -> FlowResult<()> {
        self.ctx.charge(1)?;
        if let Some(at) =
            self.visibility.iter().position(|frame| frame.locked && !frame.emitted).or_else(|| {
                self.stack.iter().enumerate().rev().find_map(|(at, source)| {
                    (paragraph_elem(source) && !self.visibility[at].emitted).then_some(at)
                })
            })
        {
            let frame = self.visibility.get_mut(at).ok_or(ConvertError::Internal)?;
            frame.hidden_any = true;
            frame.hidden_holds |= holds;
            return Ok(());
        }
        self.hidden_marker(u64::from(holds), false)
    }

    fn hidden_marker(&mut self, count: u64, block: bool) -> FlowResult<()> {
        if self.measuring {
            return Ok(());
        }
        if let Some(Token::Hidden { count: prior, block: prior_block }) = self.out.last_mut()
            && *prior_block == block
        {
            *prior = if block { prior.saturating_add(count) } else { (*prior).max(count) };
            return Ok(());
        }
        self.record(Token::Hidden { count, block }, false)
    }

    fn flush(&mut self) -> FlowResult<()> {
        for at in 0..self.stack.len() {
            if !self.visibility[at].emitted {
                let source = self.stack[at];
                let frame = self.visibility[at];
                let max = self.ctx.limits().max_nesting_depth;
                if !self.field_scan && at as u64 > max {
                    return Err(FlowError::Refused(DeferredFault::Refused {
                        limit: Limit::NestingDepth,
                        max,
                    }));
                }
                self.record(Token::Open(source), frame.original)?;
                self.visibility[at].emitted = true;
                if frame.hidden_any {
                    self.hidden_marker(u64::from(frame.hidden_holds), false)?;
                    self.visibility[at].hidden_any = false;
                    self.visibility[at].hidden_holds = false;
                }
            }
        }
        Ok(())
    }

    fn field(&mut self, source: &'a Element) -> FlowResult<()> {
        if !self.field_scan || !source.is(ns::TEXT, "hidden-paragraph") {
            return Ok(());
        }
        let Some(at) = self.paragraph() else {
            return Ok(());
        };
        if self.stack[at + 1..].iter().any(|element| !inline_container(element)) {
            return Ok(());
        }
        self.ctx.charge(1)?;
        let cached = self.field_cache.borrow().get(&std::ptr::from_ref(source)).copied();
        let field = if let Some(field) = cached {
            field
        } else {
            let mut field = super::text::Fields::default();
            field.include(source);
            self.ctx.charge(1)?;
            self.field_cache.borrow_mut().insert(std::ptr::from_ref(source), field);
            field
        };
        self.visibility.get_mut(at).ok_or(ConvertError::Internal)?.fields.merge(field);
        Ok(())
    }

    fn open_direct(&mut self, source: &'a Element, original: bool) -> FlowResult<()> {
        self.field(source)?;
        let inherited = self.visibility.last().copied().unwrap_or_default();
        let field_hidden = if !self.field_scan && !inherited.locked && paragraph_elem(source) {
            self.profile
                .as_mut()
                .map(|cursor| cursor.hidden(source, self.position))
                .transpose()?
                .unwrap_or(false)
        } else {
            false
        };
        let absolute = !self.field_scan
            && !self.ctx.hidden_requested()
            && (source.is(ns::TEXT, "section") && source.attr(ns::TEXT, "display") == Some("none")
                || source.is(ns::TEXT, "hidden-text")
                    && source.attr(ns::TEXT, "is-hidden") == Some("true")
                || (source.is(ns::DRAW, "frame") || source.is(ns::TEXT, "note"))
                    && inherited.delta.hidden == Some(true)
                || field_hidden);
        let locked = inherited.locked || absolute;
        let delta = if self.field_scan || locked || self.ctx.hidden_requested() {
            inherited.delta
        } else {
            super::text::element_delta(
                source,
                self.ctx,
                self.styles,
                if independent_story(source) { StyleDelta::default() } else { inherited.delta },
            )?
        };
        if independent_story(source) {
            self.stories.push((std::ptr::from_ref(source), std::mem::take(&mut self.suspended)));
        }
        self.stack.push(source);
        let hidden_any = !self.field_scan
            && !self.ctx.hidden_requested()
            && (absolute
                || (locked || delta.hidden == Some(true))
                    && (atomic(source)
                        || super::text::is_graphic(source)
                        || source.is(ns::TEXT, "bookmark")
                        || source.is(ns::TEXT, "bookmark-start")));
        let hidden_holds =
            hidden_any && (source.is(ns::TEXT, "note") || super::text::is_graphic(source));
        self.visibility.push(Visibility {
            delta,
            locked,
            original,
            hidden_any,
            hidden_holds,
            position: self.position,
            ..Visibility::default()
        });
        if !locked && (self.ctx.hidden_requested() || delta.hidden != Some(true)) {
            self.flush()?;
        }
        Ok(())
    }

    fn close_top(&mut self) -> FlowResult<&'a Element> {
        if self.stack.last().is_some_and(|source| paragraph_elem(source))
            && self
                .visibility
                .last()
                .is_some_and(|frame| !frame.emitted && !frame.hidden_any && !frame.locked)
        {
            self.flush()?;
        }
        let source = self.stack.pop().ok_or(ConvertError::Internal)?;
        let frame = self.visibility.pop().ok_or(ConvertError::Internal)?;
        if self.field_scan
            && paragraph_elem(source)
            && self
                .closed_profile
                .replace((std::ptr::from_ref(source), frame.position, frame.fields.hides()))
                .is_some()
        {
            return Err(ConvertError::Internal.into());
        }
        if frame.emitted {
            self.record(Token::Close(source), false)?;
        } else if frame.hidden_any {
            if let Some(parent) = self.visibility.last_mut().filter(|parent| parent.locked) {
                parent.hidden_any = true;
                parent.hidden_holds |= frame.hidden_holds;
            } else {
                self.hidden_marker(u64::from(frame.hidden_holds), block_boundary(source))?;
            }
        }
        if independent_story(source) {
            let (key, suspended) = self.stories.pop().ok_or(ConvertError::Internal)?;
            if key != std::ptr::from_ref(source) {
                return Err(ConvertError::Internal.into());
            }
            self.suspended = suspended;
        }
        for binding in &mut self.bindings {
            binding.base = binding.base.min(self.stack.len());
        }
        Ok(source)
    }

    fn open(&mut self, source: &'a Element, original: bool) -> FlowResult<()> {
        if paragraph_elem(source) && self.paragraph().is_some() {
            return Ok(());
        }
        if block_boundary(source) && !paragraph_elem(source) {
            while self.stack.last().is_some_and(|e| paragraph_elem(e) || inline_container(e)) {
                self.close_top()?;
            }
        }
        if self.paragraph().is_none()
            && !self
                .bindings
                .last()
                .is_some_and(|binding| binding.include && std::ptr::eq(binding.source, source))
        {
            self.repair_parents(source)?;
        }
        self.open_direct(source, original)?;
        if paragraph_elem(source) {
            for wrapper in std::mem::take(&mut self.suspended) {
                self.open_direct(wrapper, false)?;
            }
        }
        Ok(())
    }

    fn close(&mut self, source: &'a Element) -> FlowResult<()> {
        if paragraph_elem(source) {
            let mut wrappers = Vec::new();
            while self.stack.last().is_some_and(|e| inline_container(e)) {
                wrappers.push(self.close_top()?);
            }
            self.suspended.extend(wrappers.into_iter().rev());
        }
        let base = self.story_base();
        let exact = self
            .stack
            .iter()
            .enumerate()
            .rev()
            .find_map(|(at, e)| (at >= base && std::ptr::eq(*e, source)).then_some(at));
        let matching = exact.or_else(|| {
            self.stack
                .iter()
                .enumerate()
                .rev()
                .find_map(|(at, e)| (at >= base && same_kind(e, source)).then_some(at))
        });
        let Some(at) = matching else {
            if block_boundary(source) {
                return Ok(());
            }
            return Err(malformed("incompatible rejected revision boundaries").into());
        };
        while self.stack.len() > at {
            self.close_top()?;
        }
        Ok(())
    }

    fn repair_parents(&mut self, source: &'a Element) -> FlowResult<()> {
        if std::ptr::eq(source, self.root) {
            return Ok(());
        }
        let mut desired = Vec::new();
        let mut next = self.parents.get(&std::ptr::from_ref(source)).copied();
        let mut base = 0;
        while let Some(parent) = next {
            self.ctx.charge(1)?;
            let actual = self.alias(parent);
            if independent_story(actual)
                && let Some(at) = self.stack.iter().rposition(|elem| std::ptr::eq(*elem, actual))
            {
                base = at;
                desired.push(actual);
                break;
            }

            if let Some(binding) =
                self.bindings.iter().rev().find(|b| std::ptr::eq(b.source, parent))
            {
                base = binding.base;
                if binding.include {
                    desired.push(parent);
                }
                break;
            }
            desired.push(self.alias(parent));
            if std::ptr::eq(parent, self.root) {
                break;
            }
            next = self.parents.get(&std::ptr::from_ref(parent)).copied();
        }
        desired.reverse();
        let mut common = base.min(self.stack.len());
        for parent in &desired {
            if self.stack.get(common).is_some_and(|e| std::ptr::eq(*e, *parent)) {
                common += 1;
            } else {
                break;
            }
        }
        while self.stack.len() > common {
            self.close_top()?;
        }
        for parent in desired.into_iter().skip(common.saturating_sub(base)) {
            self.open_direct(parent, false)?;
        }
        Ok(())
    }

    fn alias(&self, source: &'a Element) -> &'a Element {
        for binding in self.bindings.iter().rev() {
            if let Some((_, alias)) =
                binding.aliases.iter().find(|(key, _)| *key == std::ptr::from_ref(source))
            {
                return alias;
            }
        }
        source
    }

    fn binding(
        &self,
        reference: Reference<'a>,
        clipped: bool,
        plan: &Plan<'a>,
    ) -> FlowResult<Binding<'a>> {
        let Reference { source, kind, .. } = reference;
        let change = kind.change();
        let inline = kind.inline() || clipped;
        let mut base = self.stack.len();
        let mut aliases = Vec::new();
        if change
            && inline
            && let Some((_, first)) = plan.full.metrics.first
            && let Some(at) = self.paragraph()
        {
            base = at;
            let mut actual = at + 1;
            let mut next = Some(first);
            while let Some(original) = next {
                self.ctx.charge(1)?;
                if std::ptr::eq(original, source) || actual == 0 {
                    break;
                }
                let Some(found) = self.stack.get(actual - 1) else {
                    break;
                };
                if !same_kind(found, original) {
                    break;
                }
                aliases.push((std::ptr::from_ref(original), *found));
                actual -= 1;
                base = actual;
                next = self.parents.get(&std::ptr::from_ref(original)).copied();
            }
        }
        Ok(Binding { source, base, include: !change, aliases })
    }

    fn checkpoint(&self) -> FlowResult<Checkpoint<'a>> {
        let aliases = self.bindings.iter().map(|b| b.aliases.len()).sum::<usize>();
        let stories = self.stories.iter().map(|(_, suspended)| suspended.len() + 1).sum::<usize>();
        self.ctx.charge(
            (2 * self.stack.len() + self.suspended.len() + self.bindings.len() + aliases + stories)
                as u64,
        )?;
        Ok(Checkpoint {
            stack: self.stack.clone(),
            visibility: self.visibility.clone(),
            suspended: self.suspended.clone(),
            stories: self.stories.clone(),
            bindings: self.bindings.clone(),
            out: self.out.len(),
            nodes: self.nodes,
            remaining: self.remaining,
        })
    }

    fn restore(&mut self, checkpoint: Checkpoint<'a>) {
        self.stack = checkpoint.stack;
        self.visibility = checkpoint.visibility;
        self.suspended = checkpoint.suspended;
        self.stories = checkpoint.stories;
        self.bindings = checkpoint.bindings;
        self.out.truncate(checkpoint.out);
        self.nodes = checkpoint.nodes;
        self.remaining = checkpoint.remaining;
    }

    fn reference(
        &mut self,
        reference: Reference<'a>,
        original: bool,
        depth: usize,
        start: u64,
        end: u64,
        position: u64,
    ) -> FlowResult<()> {
        let Reference { plan: plan_id, source, kind } = reference;
        let change = kind.change();
        let plan = self.plans.get(plan_id).ok_or(ConvertError::Internal)?;
        if !change
            && !std::ptr::eq(source, self.root)
            && (source.is(ns::OFFICE, "annotation") || source.is(super::OFFICEOOO, "annotation"))
        {
            if !self.is_hidden() {
                self.open_direct(source, false)?;
                self.close(source)?;
            }
            return Ok(());
        }
        if change && original && !self.is_hidden() {
            self.remaining = self.remaining.saturating_sub(1);
            if self.measuring {
                self.baseline += 1;
            }
        }
        let checkpoint = self.checkpoint()?;
        let binding = self.binding(reference, start > 0, plan)?;
        self.bindings.push(binding);

        let parent_profile = if change {
            None
        } else {
            let profile = self.profile(reference, start..end, position)?;
            Some(std::mem::replace(&mut self.profile, profile))
        };
        let result = self.emit(&plan.tokens, start, end, original && !change, depth + 1, position);
        if let Some(profile) = parent_profile {
            self.profile = profile;
        }
        match result {
            Ok(()) => {
                self.bindings.pop();
                Ok(())
            }
            Err(FlowError::Parse(error)) => Err(FlowError::Parse(error)),
            Err(FlowError::Refused(fault)) => {
                let was_hidden = checkpoint.visibility.last().is_some_and(|frame| {
                    frame.locked || !self.ctx.hidden_requested() && frame.delta.hidden == Some(true)
                });
                if was_hidden && (!self.is_hidden() || self.nodes > checkpoint.nodes) {
                    return Err(FlowError::Parse(fault.error()));
                }
                self.restore(checkpoint);
                if change && self.is_hidden() {
                    self.hidden(true)
                } else if change {
                    self.record(Token::Fault(fault), false)
                } else {
                    if original {
                        self.remaining = self.remaining.saturating_sub(plan.full.metrics.baseline);
                    }
                    self.open_direct(source, false)?;
                    self.record(Token::Fault(fault), false)?;
                    self.close(source)
                }
            }
        }
    }

    fn is_hidden(&self) -> bool {
        self.visibility.last().is_some_and(|frame| {
            frame.locked || !self.ctx.hidden_requested() && frame.delta.hidden == Some(true)
        })
    }

    fn text(
        &mut self,
        value: &'a str,
        damage: &'a Damage,
        owner: &'a Element,
        original: bool,
    ) -> FlowResult<()> {
        if self.paragraph().is_none() {
            self.ctx.charge(value.len() as u64)?;
            if !value.trim().is_empty() {
                let mut next = Some(owner);
                while let Some(parent) = next {
                    self.ctx.charge(1)?;
                    if paragraph_elem(parent) {
                        self.open(parent, false)?;
                        break;
                    }
                    if std::ptr::eq(parent, self.root) {
                        break;
                    }
                    next = self.parents.get(&std::ptr::from_ref(parent)).copied();
                }
            }
        }
        if self.is_hidden() {
            self.ctx.charge(value.len() as u64)?;
            self.hidden(!crate::shared::text::clean_text(value).trim().is_empty())
        } else {
            self.flush()?;
            self.record(Token::Text(value, damage, owner), original)
        }
    }

    fn selected_marker(&mut self, token: Token<'a>, original: bool) -> FlowResult<()> {
        if self.is_hidden() {
            self.hidden(matches!(token, Token::Fault(_)))
        } else {
            self.flush()?;
            self.record(token, original)
        }
    }

    fn emit(
        &mut self,
        tokens: &[Token<'a>],
        start: u64,
        end: u64,
        original: bool,
        depth: usize,
        position: u64,
    ) -> FlowResult<()> {
        if depth as u64 > Limits::MAX_NESTING_CEILING {
            return Err(FlowError::Refused(DeferredFault::Refused {
                limit: Limit::NestingDepth,
                max: Limits::MAX_NESTING_CEILING,
            }));
        }
        let mut offset = 0u64;
        for token in tokens {
            self.ctx.charge(1)?;
            let len = match token {
                Token::Reference(Reference { plan, kind, .. }) => {
                    self.plans
                        .get(*plan)
                        .ok_or(ConvertError::Internal)?
                        .window(kind.inline())
                        .metrics
                        .len
                }
                _ => 1,
            };
            let from = start.max(offset);
            let to = end.min(offset.saturating_add(len));
            if from < to {
                self.position = position.saturating_add(from - start);
                match *token {
                    Token::Open(e) => self.open(e, original)?,
                    Token::Close(e) => self.close(e)?,
                    Token::Text(value, damage, owner) => {
                        self.text(value, damage, owner, original)?;
                    }
                    Token::Omitted | Token::Fault(_) => self.selected_marker(*token, original)?,
                    Token::Reference(reference) => {
                        let window = self
                            .plans
                            .get(reference.plan)
                            .ok_or(ConvertError::Internal)?
                            .window(reference.kind.inline());
                        self.reference(
                            reference,
                            original,
                            depth,
                            window.start + from - offset,
                            window.start + to - offset,
                            self.position,
                        )?;
                    }
                    Token::Pending(_) | Token::Hidden { .. } => {
                        return Err(ConvertError::Internal.into());
                    }
                }
            }
            offset = offset.saturating_add(len);
            if offset >= end {
                break;
            }
        }
        Ok(())
    }
}

struct ScanFrame {
    plan: usize,
    next: usize,
    offset: u64,
    range: Range<u64>,
    position: u64,
    bound: bool,
}

struct ProfileCursor<'a, 'b> {
    flow: Flow<'a, 'b>,
    pending: Vec<ScanFrame>,
}

impl<'a, 'b> ProfileCursor<'a, 'b> {
    fn new(
        parent: &Flow<'a, 'b>,
        reference: Reference<'a>,
        range: Range<u64>,
        position: u64,
    ) -> FlowResult<Self> {
        let mut flow = Flow::new(
            parent.ctx,
            parent.styles,
            parent.plans,
            parent.parents,
            reference.source,
            Some(&parent.field_cache),
        );
        flow.field_scan = true;
        let plan = flow.plans.get(reference.plan).ok_or(ConvertError::Internal)?;
        let binding = flow.binding(reference, range.start > 0, plan)?;
        flow.bindings.push(binding);
        Ok(Self {
            flow,
            pending: vec![ScanFrame {
                plan: reference.plan,
                next: 0,
                offset: 0,
                range,
                position,
                bound: true,
            }],
        })
    }

    fn hidden(&mut self, source: &Element, position: u64) -> FlowResult<bool> {
        while let Some((owner, at, hidden)) = self.next()? {
            if at < position {
                continue;
            }
            if at == position && owner == std::ptr::from_ref(source) {
                return Ok(hidden);
            }
            return Err(ConvertError::Internal.into());
        }
        Err(ConvertError::Internal.into())
    }

    fn descend(
        &mut self,
        reference: Reference<'a>,
        selected: Range<u64>,
        position: u64,
    ) -> FlowResult<()> {
        let child = self.flow.plans.get(reference.plan).ok_or(ConvertError::Internal)?;
        let window = child.window(reference.kind.inline());
        let range = window.start + selected.start..window.start + selected.end;
        let binding = self.flow.binding(reference, range.start > 0, child)?;
        self.flow.bindings.push(binding);
        self.pending.push(ScanFrame {
            plan: reference.plan,
            next: 0,
            offset: 0,
            range,
            position,
            bound: true,
        });
        Ok(())
    }

    fn next(&mut self) -> FlowResult<Option<(Scope, u64, bool)>> {
        loop {
            if let Some(profile) = self.flow.closed_profile.take() {
                return Ok(Some(profile));
            }
            let Some(frame) = self.pending.last_mut() else {
                return Ok(None);
            };
            let plan = self.flow.plans.get(frame.plan).ok_or(ConvertError::Internal)?;
            let Some(token) = plan.tokens.get(frame.next).copied() else {
                let frame = self.pending.pop().ok_or(ConvertError::Internal)?;
                if frame.bound {
                    self.flow.bindings.pop().ok_or(ConvertError::Internal)?;
                }
                continue;
            };
            self.flow.ctx.charge(1)?;
            frame.next += 1;
            let len = match token {
                Token::Reference(reference) => {
                    self.flow
                        .plans
                        .get(reference.plan)
                        .ok_or(ConvertError::Internal)?
                        .window(reference.kind.inline())
                        .metrics
                        .len
                }
                _ => 1,
            };
            let offset = frame.offset;
            frame.offset = offset.saturating_add(len);
            let from = frame.range.start.max(offset);
            let to = frame.range.end.min(frame.offset);
            if from >= to {
                continue;
            }
            let position = frame.position.saturating_add(from - frame.range.start);
            self.flow.position = position;
            match token {
                Token::Open(source) => self.flow.open(source, false)?,
                Token::Close(source) => self.flow.close(source)?,
                Token::Text(value, damage, owner) => self.flow.text(value, damage, owner, false)?,
                Token::Reference(reference) if reference.kind.change() => {
                    self.descend(reference, from - offset..to - offset, position)?;
                }
                Token::Omitted | Token::Fault(_) | Token::Reference(_) => {}
                Token::Pending(_) | Token::Hidden { .. } => {
                    return Err(ConvertError::Internal.into());
                }
            }
        }
    }
}

fn same_kind(left: &Element, right: &Element) -> bool {
    left.ns == right.ns && left.local == right.local
        || paragraph_elem(left) && paragraph_elem(right)
}

fn inline_container(elem: &Element) -> bool {
    elem.ns.as_deref() == Some(ns::TEXT)
        && matches!(
            elem.local.as_str(),
            "span" | "a" | "meta" | "meta-field" | "ruby" | "ruby-base"
        )
}

fn block_boundary(elem: &Element) -> bool {
    paragraph_elem(elem)
        || structural_boundary(elem)
        || elem.is(ns::TABLE, "table")
        || elem.ns.as_deref() == Some(ns::TABLE)
            && matches!(elem.local.as_str(), "table-row" | "table-cell" | "covered-table-cell")
}

fn flow_error(error: FlowError) -> ConvertError {
    match error {
        FlowError::Parse(error) => error,
        FlowError::Refused(fault) => fault.error(),
    }
}

fn emit_view<'a>(
    ctx: &ParseCtx<'_>,
    styles: &OdfStyles<'a>,
    plans: &[Plan<'a>],
    parents: &HashMap<Scope, &'a Element>,
    root: &'a Element,
    plan_id: usize,
) -> Result<Vec<Token<'a>>, ConvertError> {
    let plan = plans.get(plan_id).ok_or(ConvertError::Internal)?;
    let mut flow = Flow::new(ctx, styles, plans, parents, root, None);
    let reference = Reference { plan: plan_id, source: root, kind: ReferenceKind::Story };
    flow.profile = flow.profile(reference, 0..plan.full.end, 0).map_err(flow_error)?;
    match flow.emit(&plan.tokens, 0, plan.full.end, true, 0, 0) {
        Ok(()) => {}
        Err(FlowError::Parse(error)) => return Err(error),
        Err(FlowError::Refused(fault)) => return Err(fault.error()),
    }
    flow.remaining = flow.baseline;
    flow.measuring = false;
    flow.baseline = 0;
    flow.nodes = 0;
    flow.profile = flow.profile(reference, 0..plan.full.end, 0).map_err(flow_error)?;
    match flow.emit(&plan.tokens, 0, plan.full.end, true, 0, 0) {
        Ok(()) if flow.stack.is_empty() && flow.suspended.is_empty() && flow.stories.is_empty() => {
            Ok(flow.out)
        }
        Ok(()) => Err(malformed("unbalanced rejected revisions")),
        Err(FlowError::Parse(error)) => Err(error),
        Err(FlowError::Refused(fault)) => Err(fault.error()),
    }
}

struct Rebuilder<'c> {
    ctx: &'c ParseCtx<'c>,
    stack: Vec<Element>,
    root: Option<Element>,
}

impl Rebuilder<'_> {
    fn close(&mut self, source: &Element) -> Result<(), ConvertError> {
        let elem = self.stack.pop().ok_or(ConvertError::Internal)?;
        if elem.ns != source.ns || elem.local != source.local {
            return Err(ConvertError::Internal);
        }
        if let Some(parent) = self.stack.last_mut() {
            parent.unresolved_text |= elem.unresolved_text;
            parent.children.push(Node::Elem(elem));
        } else if self.root.replace(elem).is_some() {
            return Err(ConvertError::Internal);
        }
        Ok(())
    }

    fn text(&mut self, value: &str, damage: &Damage) -> Result<(), ConvertError> {
        let parent = self.stack.last_mut().ok_or(ConvertError::Internal)?;
        let at = parent.children.len();
        let value = copied(self.ctx, value)?;
        for (_, span) in damage {
            self.ctx.charge(1)?;
            parent.text_damage.get_or_insert_with(Box::default).push((at, span.clone()));
            parent.unresolved_text = true;
        }
        parent.children.push(Node::Text(value));
        Ok(())
    }

    fn marker(&mut self, selection: ContentSelection) -> Result<(), ConvertError> {
        let parent = self.stack.last_mut().ok_or(ConvertError::Internal)?;
        self.ctx.charge(ns::TEXT.len() as u64 + 1)?;
        parent.children.push(Node::Elem(Element {
            ns: Some(Rc::from(ns::TEXT)),
            local: copied(self.ctx, "change")?,
            selection: Some(Box::new(selection)),
            ..Element::default()
        }));
        Ok(())
    }
}

fn rebuild(ctx: &ParseCtx<'_>, tokens: &[Token<'_>]) -> Result<Element, ConvertError> {
    let mut builder = Rebuilder { ctx, stack: Vec::new(), root: None };
    for token in tokens {
        ctx.charge(1)?;
        match token {
            Token::Open(elem) => builder.stack.push(metadata(ctx, elem)?),
            Token::Close(elem) => builder.close(elem)?,
            Token::Text(value, damage, _) => builder.text(value, damage)?,
            Token::Omitted => builder.marker(ContentSelection::Omitted)?,
            Token::Hidden { count, block } => {
                builder.marker(ContentSelection::Hidden(*count))?;
                if *block {
                    let parent = builder.stack.last_mut().ok_or(ConvertError::Internal)?;
                    let Node::Elem(marker) =
                        parent.children.last_mut().ok_or(ConvertError::Internal)?
                    else {
                        return Err(ConvertError::Internal);
                    };
                    marker.local = copied(ctx, "p")?;
                }
            }
            Token::Fault(fault) => builder.marker(ContentSelection::Fault(*fault))?,
            Token::Pending(_) | Token::Reference(_) => return Err(ConvertError::Internal),
        }
    }
    if !builder.stack.is_empty() {
        return Err(ConvertError::Internal);
    }
    builder.root.ok_or(ConvertError::Internal)
}

fn malformed(reason: &'static str) -> ConvertError {
    ConvertError::malformed_part("content.xml", reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RevisionPolicy;
    use crate::render::markdown::document_to_markdown;
    use crate::shared::test_support::zip_package;
    use std::assert_matches;
    use std::fmt::Write;

    const NS: &str = r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0""#;

    fn markdown(body: &str, policy: RevisionPolicy) -> String {
        let content = format!(
            "<office:document-content {NS}><office:body><office:text>{body}</office:text></office:body></office:document-content>"
        );
        let bytes = zip_package(&[("content.xml", &content)]);
        let ctx = ParseCtx::default().with_revisions(policy);
        document_to_markdown(&super::super::parse(&ctx, &bytes).unwrap())
    }

    fn tracked(changes: &str, body: &str) -> String {
        format!("<text:tracked-changes>{changes}</text:tracked-changes>{body}")
    }

    #[test]
    fn rejecting_insertions_restores_paragraph_boundaries() {
        let definition =
            r#"<text:changed-region text:id="i"><text:insertion/></text:changed-region>"#;
        let cases = [
            (
                r#"<text:p>a<text:change-start text:change-id="i"/>new<text:change-end text:change-id="i"/>b</text:p>"#,
                "ab\n",
            ),
            (
                r#"<text:p>a<text:change-start text:change-id="i"/>new</text:p><text:p>more<text:change-end text:change-id="i"/>b</text:p>"#,
                "ab\n",
            ),
            (
                r#"<text:p>a<text:change-start text:change-id="i"/></text:p><text:p><text:change-end text:change-id="i"/>b</text:p>"#,
                "ab\n",
            ),
            (
                r#"<text:p>a</text:p><text:change-start text:change-id="i"/><text:list><text:list-item><text:p>new</text:p></text:list-item></text:list><text:change-end text:change-id="i"/><text:p>b</text:p>"#,
                "a\n\nb\n",
            ),
        ];
        for (body, expected) in cases {
            assert_eq!(
                markdown(&tracked(definition, body), RevisionPolicy::Reject),
                expected,
                "{body}"
            );
        }
    }

    #[test]
    fn rejecting_deletions_merges_edges_and_keeps_middle_paragraphs() {
        let body = r#"<text:p>before<text:change text:change-id="d"/>after</text:p>"#;
        let cases = [
            ("<text:p>old</text:p>", "beforeoldafter\n"),
            (
                "<text:p>first</text:p><text:p>middle</text:p><text:p>last</text:p>",
                "beforefirst\n\nmiddle\n\nlastafter\n",
            ),
            ("<text:p/><text:p>whole</text:p><text:p/>", "before\n\nwhole\n\nafter\n"),
        ];
        for (deleted, expected) in cases {
            let definition = format!(
                r#"<text:changed-region text:id="decoy"><text:deletion><text:p>wrong</text:p></text:deletion></text:changed-region><text:changed-region xml:id="d" text:id="unused"><text:deletion>{deleted}</text:deletion></text:changed-region>"#
            );
            let input = tracked(&definition, body);
            assert_eq!(markdown(&input, RevisionPolicy::Reject), expected);
            assert_eq!(markdown(&input, RevisionPolicy::Accept), "beforeafter\n");
        }
    }

    #[test]
    fn revision_selection_applies_inside_list_items_and_table_cells() {
        let definition = r#"<text:changed-region text:id="i"><text:insertion/></text:changed-region><text:changed-region text:id="d"><text:deletion><text:p>old</text:p><text:p>last</text:p></text:deletion></text:changed-region>"#;
        let list = r#"<text:list><text:list-item><text:p>a<text:change-start text:change-id="i"/>new</text:p><text:p>more<text:change-end text:change-id="i"/>b<text:change text:change-id="d"/>c</text:p></text:list-item></text:list>"#;
        assert_eq!(
            markdown(&tracked(definition, list), RevisionPolicy::Reject),
            "- abold\n\n  lastc\n"
        );
        let cell = r#"<table:table><table:table-row><table:table-cell><text:p>a<text:change-start text:change-id="i"/>new</text:p><text:p>more<text:change-end text:change-id="i"/>b<text:change text:change-id="d"/>c</text:p></table:table-cell></table:table-row></table:table>"#;
        assert_eq!(
            markdown(&tracked(definition, cell), RevisionPolicy::Reject),
            "|  |\n| --- |\n| abold<br>lastc |\n"
        );
    }
    #[test]
    fn restored_paragraphs_split_surrounding_hyperlinks() {
        let definition = r#"<text:changed-region text:id="d"><text:deletion><text:p>first</text:p><text:p>last</text:p></text:deletion></text:changed-region>"#;
        let body = r#"<text:p><text:a xlink:href="https://example.org">before<text:change text:change-id="d"/>after</text:a></text:p>"#;
        assert_eq!(
            markdown(&tracked(definition, body), RevisionPolicy::Reject),
            "[beforefirst](https://example.org)\n\n[lastafter](https://example.org)\n"
        );
    }
    fn raw(body: &str) -> Element {
        let xml = format!("<office:text {NS}>{body}</office:text>");
        crate::package::xml::parse_xml(
            &ParseCtx::default(),
            xml.as_bytes(),
            crate::package::archive::Role::Probe,
        )
        .unwrap()
    }

    fn rejected(body: &str) -> Result<Element, ConvertError> {
        select(&ParseCtx::default().with_revisions(RevisionPolicy::Reject), raw(body))
    }

    #[test]
    fn scoped_changes_do_not_cross_text_stories() {
        let body = r#"<text:tracked-changes><text:changed-region text:id="d"><text:deletion><text:p>body</text:p></text:deletion></text:changed-region></text:tracked-changes><text:p><text:change text:change-id="d"/></text:p><style:header><text:tracked-changes><text:changed-region text:id="d"><text:deletion><text:p>header</text:p></text:deletion></text:changed-region></text:tracked-changes><text:p><text:change text:change-id="d"/></text:p></style:header>"#;
        let tree = rejected(body).unwrap();
        assert_eq!(tree.text(), "bodyheader");
        assert_eq!(tree.first_descendant(ns::STYLE, "header").unwrap().text(), "header");
        assert_eq!(
            rejected(r#"<text:p>a<text:change text:change-id="missing"/>b</text:p>"#)
                .unwrap()
                .text(),
            "ab"
        );
    }

    #[test]
    fn restored_text_preserves_damage_styles_links_and_images() {
        let input = tracked(
            r#"<text:changed-region text:id="i"><text:insertion/></text:changed-region><text:changed-region text:id="d"><text:deletion><office:change-info><text:p>metadata&meta;</text:p></office:change-info><text:p><text:span text:style-name="Bold">old&old;</text:span><text:a xlink:href="https://example.org">link</text:a><draw:frame><draw:image xlink:href="Pictures/photo.png"/></draw:frame></text:p></text:deletion></text:changed-region>"#,
            r#"<text:p>left&left;<text:change-start text:change-id="i"/>gone&gone;<text:change-end text:change-id="i"/><text:change text:change-id="d"/>right&right;</text:p>"#,
        );
        let tree = rejected(&input).unwrap();
        let paragraph = tree.first_descendant(ns::TEXT, "p").unwrap();
        assert_eq!(paragraph.text(), "left&left;old&old;linkright&right;");
        assert_eq!(paragraph.text_damage(), [(0, 4..10), (5, 5..12)]);
        let span = paragraph.find(ns::TEXT, "span").unwrap();
        assert_eq!(span.attr(ns::TEXT, "style-name"), Some("Bold"));
        assert_eq!(span.text_damage(), [(0, 3..8)]);
        assert_eq!(
            paragraph.find(ns::TEXT, "a").unwrap().attr(ns::XLINK, "href"),
            Some("https://example.org")
        );
        assert_eq!(
            paragraph.first_descendant(ns::DRAW, "image").unwrap().attr(ns::XLINK, "href"),
            Some("Pictures/photo.png")
        );
    }

    #[test]
    fn only_referenced_deletion_content_counts_under_accept() {
        let definitions = r#"<text:changed-region text:id="d"><text:deletion><office:change-info><text:p>metadata</text:p></office:change-info><text:p>deleted</text:p></text:deletion></text:changed-region><text:changed-region text:id="empty"><text:deletion><office:change-info><text:p>metadata only</text:p></office:change-info><text:p/></text:deletion></text:changed-region><text:changed-region text:id="unused"><text:deletion><text:p>unused</text:p></text:deletion></text:changed-region>"#;
        let input = tracked(
            definitions,
            r#"<text:p>a<text:change text:change-id="d"/><text:change text:change-id="empty"/>b<text:change text:change-id="d"/></text:p>"#,
        );
        let tree = raw(&input);
        let found = accepted(&ParseCtx::default(), std::iter::once(&tree)).unwrap();
        assert_eq!(found.len(), 2);
        let bytes = format!(
            "<office:document-content {NS}><office:body><office:text>{input}</office:text></office:body></office:document-content>"
        );
        let ctx = ParseCtx::default();
        let document = super::super::parse(&ctx, &zip_package(&[("content.xml", &bytes)])).unwrap();
        assert_eq!(document_to_markdown(&document), "ab\n");
        assert_eq!(
            crate::shared::test_support::diagnostic_rows::<(
                crate::diagnostics::DiagnosticCode,
                crate::diagnostics::Region,
                u64
            )>(&ctx.diagnostics()),
            [(
                crate::diagnostics::DiagnosticCode::RevisionOmitted,
                crate::diagnostics::Region::Body,
                2
            )]
        );
    }

    #[test]
    fn selected_cycles_and_missing_definitions_are_malformed() {
        let cases = [
            (
                r#"<text:changed-region text:id="d"><text:deletion><text:p><text:change text:change-id="d"/></text:p></text:deletion></text:changed-region>"#,
                r#"<text:p><text:change text:change-id="d"/></text:p>"#,
                "cyclic tracked deletion",
            ),
            (
                r#"<text:changed-region text:id="decoy"><text:insertion/></text:changed-region>"#,
                r#"<text:p><text:change text:change-id="missing"/></text:p>"#,
                "unknown or ambiguous tracked change",
            ),
            (
                r#"<text:changed-region text:id="i"><text:insertion/></text:changed-region>"#,
                r#"<text:p><text:change-start text:change-id="i"/>open</text:p>"#,
                "unclosed insertion range",
            ),
        ];
        for (definitions, body, expected) in cases {
            assert_matches!(reject_document(&tracked(definitions, body), "", &Limits::default()), Err(ConvertError::Malformed { reason, .. }) if reason == expected);
        }
    }

    #[test]
    fn unselected_cycles_and_metadata_are_never_resolved() {
        let definitions = r#"<text:changed-region text:id="i"><text:insertion/></text:changed-region><text:changed-region text:id="d"><text:deletion><text:p><text:change text:change-id="d"/></text:p></text:deletion></text:changed-region>"#;
        for body in [
            "<text:p>kept</text:p>",
            r#"<text:p>kept<text:change-start text:change-id="i"/><text:change-end text:change-id="unknown"/><text:change-start text:change-id="i"/>gone<text:change-end text:change-id="i"/></text:p>"#,
            r#"<text:p>kept<text:change-start text:change-id="i"/><text:change text:change-id="d"/>gone<text:change-end text:change-id="i"/></text:p>"#,
        ] {
            assert_eq!(rejected(&tracked(definitions, body)).unwrap().text(), "kept");
        }
    }

    fn reference_chain(count: usize, nested: bool) -> String {
        let mut definitions = String::new();
        for at in 0..count {
            let content = if at + 1 == count {
                "end".to_string()
            } else {
                format!(r#"<text:change text:change-id="d{}"/>"#, at + 1)
            };
            let content = if nested {
                format!("<text:list><text:list-item>{content}</text:list-item></text:list>")
            } else {
                format!("<text:p>{content}</text:p>")
            };
            write!(definitions, r#"<text:changed-region text:id="d{at}"><text:deletion>{content}</text:deletion></text:changed-region>"#).unwrap();
        }
        tracked(&definitions, r#"<text:change text:change-id="d0"/>"#)
    }

    #[test]
    fn change_reference_and_materialized_nesting_are_independently_bounded() {
        let limits = Limits { max_nesting_depth: 1024, ..Limits::default() };
        let ctx = ParseCtx::new(&limits).with_revisions(RevisionPolicy::Reject);
        let selected =
            select(&ctx, raw(&reference_chain(MAX_CHANGE_REFERENCES + 1, false))).unwrap();
        assert_matches!(
            selected.descendant_elems().find_map(fault).unwrap().error(),
            ConvertError::ResourceLimit { limit: Limit::NestingDepth, max: 64 }
        );
        assert_matches!(
            reject_document(&reference_chain(MAX_CHANGE_REFERENCES + 1, false), "", &limits),
            Err(ConvertError::ResourceLimit { limit: Limit::NestingDepth, max: 64 })
        );
        let limits = Limits { max_nesting_depth: 8, ..Limits::default() };
        let ctx = ParseCtx::new(&limits).with_revisions(RevisionPolicy::Reject);
        let selected = select(&ctx, raw(&reference_chain(10, true))).unwrap();
        assert_matches!(
            selected.descendant_elems().find_map(fault).unwrap().error(),
            ConvertError::ResourceLimit { limit: Limit::NestingDepth, max: 8 }
        );
    }

    #[test]
    fn expanded_copies_charge_attribute_values_and_reuse_cached_references() {
        let definition = format!(
            r#"<text:changed-region text:id="d"><text:deletion><text:p><text:a xlink:href="https://example.org/{}">label</text:a></text:p></text:deletion></text:changed-region>"#,
            "x".repeat(4096)
        );
        let body = tracked(
            &definition,
            &format!("<text:p>{}</text:p>", r#"<text:change text:change-id="d"/>"#.repeat(32)),
        );
        let tree = raw(&body);
        let ctx = ParseCtx::default().with_revisions(RevisionPolicy::Reject);
        let mut selector = Selector::new(&ctx);
        selector.index(&tree).unwrap();
        let scope = selector.nearest.get(&std::ptr::from_ref(&tree)).copied().flatten();
        let plan =
            selector.compile(Request { source: &tree, scope, id: None, inline: false }).unwrap();
        assert_eq!(selector.compiled_changes, 1);
        let tokens =
            emit_view(&ctx, &OdfStyles::default(), &selector.plans, &selector.parents, &tree, plan)
                .unwrap();
        let before = ctx.work_left();
        rebuild(&ctx, &tokens).unwrap();
        assert!(before - ctx.work_left() >= 131_072);
        eprintln!(
            "ODF repeated reference: {} target plans, {} physical plan events, {} copy work units",
            selector.compiled_changes,
            selector.plans.iter().map(|plan| plan.tokens.len()).sum::<usize>(),
            before - ctx.work_left()
        );
        let spent = ctx.limits().max_work - ctx.work_left();
        let limits = Limits { max_work: spent - 1, ..Limits::default() };
        let ctx = ParseCtx::new(&limits).with_revisions(RevisionPolicy::Reject);
        assert_matches!(select(&ctx, raw(&body)), Err(ConvertError::ResourceLimit { limit: Limit::Work, max }) if max == spent - 1);
    }
    #[test]
    fn inserted_list_item_boundaries_join_the_surviving_items() {
        let definitions =
            r#"<text:changed-region text:id="i"><text:insertion/></text:changed-region>"#;
        let body = r#"<text:list><text:list-item><text:p>a<text:change-start text:change-id="i"/>new</text:p></text:list-item><text:list-item><text:p>middle</text:p></text:list-item><text:list-item><text:p>more<text:change-end text:change-id="i"/>b</text:p></text:list-item></text:list>"#;
        assert_eq!(markdown(&tracked(definitions, body), RevisionPolicy::Reject), "- ab\n");
    }

    #[test]
    fn a_restored_table_remains_between_the_surrounding_paragraphs() {
        let definitions = r#"<text:changed-region text:id="d"><text:deletion><text:p/><table:table><table:table-row><table:table-cell><text:p>cell</text:p></table:table-cell></table:table-row></table:table><text:p/></text:deletion></text:changed-region>"#;
        let body = r#"<text:p>before<text:change text:change-id="d"/>after</text:p>"#;
        assert_eq!(
            markdown(&tracked(definitions, body), RevisionPolicy::Reject),
            "before\n\n|  |\n| --- |\n| cell |\n\nafter\n"
        );
    }

    #[test]
    fn default_acceptance_keeps_source_allocations_and_has_linear_index_cost() {
        let body = "<text:p>paragraph with ordinary text</text:p>".repeat(10_000);
        let start = std::time::Instant::now();
        let tree = raw(&body);
        let parse_us = start.elapsed().as_micros();
        let pointer = tree.children.as_ptr();
        let ctx = ParseCtx::default();
        let before = ctx.work_left();
        let start = std::time::Instant::now();
        assert_eq!(accepted(&ctx, std::iter::once(&tree)).unwrap().len(), 0);
        let index_us = start.elapsed().as_micros();
        let cost = before - ctx.work_left();
        assert_eq!(cost, 10_002);
        let selected = select(&ctx, tree).unwrap();
        assert_eq!(pointer, selected.children.as_ptr());
        eprintln!(
            "ODF 10000 paragraphs: XML parse {parse_us} us; default revision index {index_us} us; {cost} work units; no source copies"
        );
    }

    #[test]
    fn selected_expansion_and_cancellation_stop_before_materialization() {
        let definition = r#"<text:changed-region text:id="d"><text:deletion><text:p><text:span>one</text:span><text:span>two</text:span><text:span>three</text:span></text:p></text:deletion></text:changed-region>"#;
        let body = tracked(
            definition,
            &r#"<text:p><text:change text:change-id="d"/></text:p>"#.repeat(50),
        );
        let limits = Limits { max_xml_nodes: 256, ..Limits::default() };
        let ctx = ParseCtx::new(&limits).with_revisions(RevisionPolicy::Reject);
        let selected = select(&ctx, raw(&body)).unwrap();
        assert_matches!(
            selected.descendant_elems().find_map(fault).unwrap().error(),
            ConvertError::ResourceLimit { limit: Limit::XmlNodes, max: 256 }
        );
        assert_matches!(
            reject_document(&body, "", &limits),
            Err(ConvertError::ResourceLimit { limit: Limit::XmlNodes, max: 256 })
        );
        let cancel = crate::ledger::CancelToken::new();
        let ctx =
            crate::shared::test_support::cancelled(&cancel).with_revisions(RevisionPolicy::Reject);
        assert_matches!(select(&ctx, raw(&body)), Err(ConvertError::Cancelled));
    }
    fn reject_document(
        body: &str,
        head: &str,
        limits: &Limits,
    ) -> Result<crate::model::Document, ConvertError> {
        reject_document_hidden(body, head, limits, crate::HiddenPolicy::Omit)
    }

    fn reject_document_hidden(
        body: &str,
        head: &str,
        limits: &Limits,
        hidden: crate::HiddenPolicy,
    ) -> Result<crate::model::Document, ConvertError> {
        let content = format!(
            "<office:document-content {NS}>{head}<office:body><office:text>{body}</office:text></office:body></office:document-content>"
        );
        let bytes = zip_package(&[("content.xml", &content)]);
        super::super::parse(
            &ParseCtx::new(limits).with_revisions(RevisionPolicy::Reject).with_hidden(hidden),
            &bytes,
        )
    }

    const HIDDEN_STYLES: &str = r#"<office:automatic-styles><style:style style:name="Hide" style:family="text"><style:text-properties text:display="none"/></style:style><style:style style:name="Show" style:family="text"><style:text-properties text:display="true"/></style:style><style:style style:name="Gone" style:family="paragraph"><style:text-properties text:display="none"/></style:style></office:automatic-styles>"#;

    #[test]
    fn hidden_invalid_references_do_not_refuse_the_selected_view() {
        let definitions = r#"<text:changed-region text:id="d"><text:deletion><text:p><text:change text:change-id="d"/></text:p></text:deletion></text:changed-region>"#;
        let cases = [
            r#"<text:p><text:hidden-paragraph text:is-hidden="true"/><text:change text:change-id="d"/></text:p>"#,
            r#"<text:p><text:span text:style-name="Hide"><text:change text:change-id="d"/></text:span></text:p>"#,
            r#"<text:p text:style-name="Gone"><text:change text:change-id="missing"/></text:p>"#,
            r#"<text:section text:display="none"><text:p><text:change text:change-id="d"/></text:p></text:section>"#,
        ];
        for body in cases {
            let body = tracked(definitions, &format!("{body}<text:p>kept</text:p>"));
            assert_eq!(
                document_to_markdown(
                    &reject_document(&body, HIDDEN_STYLES, &Limits::default()).unwrap()
                ),
                "kept\n"
            );
        }
        let body = tracked(
            definitions,
            r#"<text:p text:style-name="Gone"><text:span text:style-name="Show"><text:change text:change-id="d"/></text:span></text:p>"#,
        );
        assert_matches!(
            reject_document(&body, HIDDEN_STYLES, &Limits::default()),
            Err(ConvertError::Malformed { reason: "cyclic tracked deletion", .. })
        );
    }

    #[test]
    fn hidden_unadmitted_reference_depth_is_skipped_without_expansion() {
        let input = reference_chain(MAX_CHANGE_REFERENCES + 1, false);
        let input = input.replace(r#"<text:change text:change-id="d0"/>"#, r#"<text:p><text:span text:style-name="Hide"><text:change text:change-id="d0"/></text:span></text:p><text:p>kept</text:p>"#);
        let limits = Limits { max_nesting_depth: 1024, ..Limits::default() };
        assert_eq!(
            document_to_markdown(&reject_document(&input, HIDDEN_STYLES, &limits).unwrap()),
            "kept\n"
        );
    }

    #[test]
    fn source_tags_cannot_manufacture_internal_omissions_or_faults() {
        let body = r#"<text:p>kept<text:change-start/><text:change error="cyclic tracked deletion" refused="nesting_depth" max="1"/></text:p>"#;
        let content = format!(
            "<office:document-content {NS}><office:body><office:text>{body}</office:text></office:body></office:document-content>"
        );
        let ctx = ParseCtx::default().with_revisions(RevisionPolicy::Reject);
        let document =
            super::super::parse(&ctx, &zip_package(&[("content.xml", &content)])).unwrap();
        assert_eq!(document_to_markdown(&document), "kept\n");
        assert_eq!(ctx.diagnostics().iter().count(), 0);
    }
    #[test]
    fn a_target_refused_deep_in_a_hidden_change_resolves_when_used_directly() {
        let input = reference_chain(MAX_CHANGE_REFERENCES + 1, false);
        let input = input.replace(r#"<text:change text:change-id="d0"/>"#, r#"<text:p><text:span text:style-name="Hide"><text:change text:change-id="d0"/></text:span>visible<text:change text:change-id="d62"/></text:p>"#);
        let limits = Limits { max_nesting_depth: 1024, ..Limits::default() };
        assert_eq!(
            document_to_markdown(&reject_document(&input, HIDDEN_STYLES, &limits).unwrap()),
            "visibleend\n"
        );
    }
    #[test]
    fn accepted_deletion_of_a_paragraph_mark_counts_as_omitted_text() {
        let cases = [
            ("<text:p/>", 0),
            ("<text:p/><text:p/>", 1),
            ("<office:change-info><text:p/><text:p/></office:change-info><text:p/>", 0),
        ];
        for (content, expected) in cases {
            let definition = format!(
                r#"<text:changed-region text:id="d"><text:deletion>{content}</text:deletion></text:changed-region>"#
            );
            let tree = raw(&tracked(
                &definition,
                r#"<text:p>before<text:change text:change-id="d"/>after</text:p>"#,
            ));
            assert_eq!(
                accepted(&ParseCtx::default(), std::iter::once(&tree)).unwrap().len(),
                expected
            );
        }
    }
    #[test]
    fn review_fix_hidden_insertion_validation() {
        let definitions =
            r#"<text:changed-region text:id="i"><text:insertion/></text:changed-region>"#;
        let cases = [
            (
                "hidden paragraph end",
                r#"<text:p text:style-name="Gone"><text:change-end text:change-id="i"/></text:p>"#,
            ),
            (
                "hidden span end",
                r#"<text:p><text:span text:style-name="Hide"><text:change-end text:change-id="i"/></text:span></text:p>"#,
            ),
            (
                "hidden section end",
                r#"<text:section text:display="none"><text:p><text:change-end text:change-id="i"/></text:p></text:section>"#,
            ),
            (
                "hidden section start",
                r#"<text:section text:display="none"><text:p><text:change-start text:change-id="i"/>gone</text:p></text:section>"#,
            ),
        ];
        for (name, body) in cases {
            let body = tracked(definitions, &format!("{body}<text:p>kept</text:p>"));
            let result = reject_document(&body, HIDDEN_STYLES, &Limits::default());
            assert_eq!(result.as_ref().map(document_to_markdown).unwrap(), "kept\n", "{name}");
            let expected = if name.ends_with("start") {
                "unclosed insertion range"
            } else {
                "insertion end has no start"
            };
            assert_matches!(reject_document_hidden(&body, HIDDEN_STYLES, &Limits::default(), crate::HiddenPolicy::Emit), Err(ConvertError::Malformed { reason, .. }) if reason == expected);
        }
    }

    #[test]
    fn review_fix_hidden_expansion_poisoning() {
        let mut definitions = String::new();
        for at in 0..10 {
            let child = if at == 9 {
                "end".to_string()
            } else {
                format!(
                    r#"<text:change text:change-id="d{}"/><text:change text:change-id="d{}"/>"#,
                    at + 1,
                    at + 1
                )
            };
            write!(definitions, r#"<text:changed-region text:id="d{at}"><text:deletion><text:p>{child}</text:p></text:deletion></text:changed-region>"#).unwrap();
        }
        definitions.push_str(r#"<text:changed-region text:id="small"><text:deletion><text:p>old</text:p></text:deletion></text:changed-region>"#);
        let limits = Limits { max_xml_nodes: 128, ..Limits::default() };
        for (name, body) in [
            ("only visible", r#"<text:p>visible<text:change text:change-id="small"/></text:p>"#),
            (
                "hidden first",
                r#"<text:p><text:span text:style-name="Hide"><text:change text:change-id="d0"/></text:span>visible<text:change text:change-id="small"/></text:p>"#,
            ),
            (
                "visible first",
                r#"<text:p>visible<text:change text:change-id="small"/><text:span text:style-name="Hide"><text:change text:change-id="d0"/></text:span></text:p>"#,
            ),
        ] {
            let result = reject_document(&tracked(&definitions, body), HIDDEN_STYLES, &limits);
            assert_eq!(
                result.as_ref().map(document_to_markdown).unwrap(),
                "visibleold\n",
                "{name}"
            );
            if name != "only visible" {
                assert_matches!(
                    reject_document_hidden(
                        &tracked(&definitions, body),
                        HIDDEN_STYLES,
                        &limits,
                        crate::HiddenPolicy::Emit
                    ),
                    Err(ConvertError::ResourceLimit { limit: Limit::XmlNodes, max: 128 })
                );
            }
            let source = raw(&tracked(&definitions, body));
            let ctx = ParseCtx::new(&limits).with_revisions(RevisionPolicy::Reject);
            let mut views = StoryViews::new(&ctx, std::iter::once(&source)).unwrap();
            views.selected(&source).unwrap();
            let retained = views.selector.plans.iter().map(|plan| plan.tokens.len()).sum::<usize>();
            let nodes = std::iter::once(&source)
                .chain(source.descendant_elems())
                .map(|elem| {
                    1 + elem.children.iter().filter(|node| matches!(node, Node::Text(_))).count()
                })
                .sum::<usize>();
            assert!(retained <= nodes * 2, "{name}: retained {retained}, source nodes {nodes}");
            eprintln!(
                "ODF hidden expansion {name}: {retained} physical plan events / {nodes} source nodes; {} work units",
                ctx.limits().max_work - ctx.work_left()
            );
        }
    }

    #[test]
    fn review_fix_cross_list_boundaries() {
        let definitions =
            r#"<text:changed-region text:id="i"><text:insertion/></text:changed-region>"#;
        for (name, body, expected) in [
            (
                "list to paragraph",
                r#"<text:list><text:list-item><text:p>a<text:change-start text:change-id="i"/>new</text:p></text:list-item></text:list><text:p>more<text:change-end text:change-id="i"/>b</text:p>"#,
                "- ab\n",
            ),
            (
                "paragraph to list",
                r#"<text:p>a<text:change-start text:change-id="i"/>new</text:p><text:list><text:list-item><text:p>more<text:change-end text:change-id="i"/>b</text:p></text:list-item></text:list>"#,
                "ab\n",
            ),
            (
                "list to paragraph with following paragraph",
                r#"<text:list><text:list-item><text:p>a<text:change-start text:change-id="i"/>new</text:p></text:list-item></text:list><text:p>more<text:change-end text:change-id="i"/>b</text:p><text:p>c</text:p>"#,
                "- ab\n\nc\n",
            ),
            (
                "paragraph to list with following item",
                r#"<text:p>a<text:change-start text:change-id="i"/>new</text:p><text:list><text:list-item><text:p>more<text:change-end text:change-id="i"/>b</text:p></text:list-item><text:list-item><text:p>c</text:p></text:list-item></text:list>"#,
                "ab\n\n- c\n",
            ),
        ] {
            let result = reject_document(&tracked(definitions, body), "", &Limits::default());
            assert_eq!(result.as_ref().map(document_to_markdown).unwrap(), expected, "{name}");
        }
    }
    #[test]
    fn review_fix_independent_text_stories_keep_their_paragraphs() {
        let definitions =
            r#"<text:changed-region text:id="i"><text:insertion/></text:changed-region>"#;
        let cases = [
            (
                r#"<text:p>body<text:note text:note-class="footnote"><text:note-citation>1</text:note-citation><text:note-body><text:p>old<text:change-start text:change-id="i"/>new<text:change-end text:change-id="i"/></text:p></text:note-body></text:note></text:p>"#,
                "body[^1]\n\n[^1]: old\n",
            ),
            (
                r#"<text:p>body<draw:frame><draw:text-box><text:p>old<text:change-start text:change-id="i"/>new<text:change-end text:change-id="i"/></text:p></draw:text-box></draw:frame></text:p>"#,
                "body\n\nold\n",
            ),
        ];
        for (body, expected) in cases {
            assert_eq!(markdown(&tracked(definitions, body), RevisionPolicy::Reject), expected);
        }
    }
    #[test]
    fn individually_fitting_hidden_expansions_do_not_consume_visible_admission() {
        let payload = "<text:span>x</text:span>".repeat(35);
        let limits = Limits { max_xml_nodes: 128, ..Limits::default() };
        for (tail, hidden_text) in
            [("", ""), (r#"<text:span text:style-name="Show">shown</text:span>"#, "shown")]
        {
            let definition = format!(
                r#"<text:changed-region text:id="d"><text:deletion><text:p>{payload}{tail}</text:p></text:deletion></text:changed-region>"#
            );
            for (body, expected) in [
                (
                    r#"<text:p><text:span text:style-name="Hide"><text:change text:change-id="d"/></text:span>visible<text:change text:change-id="d"/></text:p>"#,
                    format!("{hidden_text}visible{}{hidden_text}\n", "x".repeat(35)),
                ),
                (
                    r#"<text:p>visible<text:change text:change-id="d"/><text:span text:style-name="Hide"><text:change text:change-id="d"/></text:span></text:p>"#,
                    format!("visible{}{hidden_text}{hidden_text}\n", "x".repeat(35)),
                ),
            ] {
                let input = tracked(&definition, body);
                assert_eq!(
                    document_to_markdown(&reject_document(&input, HIDDEN_STYLES, &limits).unwrap()),
                    expected
                );
                assert_matches!(
                    reject_document_hidden(
                        &input,
                        HIDDEN_STYLES,
                        &limits,
                        crate::HiddenPolicy::Emit
                    ),
                    Err(ConvertError::ResourceLimit { limit: Limit::XmlNodes, max: 128 })
                );
            }
        }
    }

    #[test]
    fn omitted_comment_bodies_keep_style_resolution_lazy() {
        let head = r#"<office:automatic-styles><style:style style:name="Cycle" style:family="paragraph" style:parent-style-name="Cycle"/></office:automatic-styles>"#;
        let definition =
            r#"<text:changed-region text:id="unused"><text:insertion/></text:changed-region>"#;
        let body = tracked(
            definition,
            r#"<text:p>kept<office:annotation office:name="ignored"><text:p text:style-name="Cycle">unused</text:p></office:annotation></text:p>"#,
        );
        assert_eq!(
            document_to_markdown(&reject_document(&body, head, &Limits::default()).unwrap()),
            "kept\n"
        );
    }
    #[test]
    fn paragraph_fields_follow_selected_revision_content() {
        let inserted =
            r#"<text:changed-region text:id="i"><text:insertion/></text:changed-region>"#;
        let body = tracked(
            inserted,
            r#"<text:p><text:change-start text:change-id="i"/><text:hidden-paragraph text:is-hidden="true"/><text:change-end text:change-id="i"/>kept</text:p>"#,
        );
        assert_eq!(
            document_to_markdown(&reject_document(&body, "", &Limits::default()).unwrap()),
            "kept\n"
        );
        let payload = "<text:span>x</text:span>".repeat(35);
        let definitions = format!(
            r#"<text:changed-region text:id="d"><text:deletion><text:p><text:hidden-paragraph text:is-hidden="true"/>{payload}</text:p></text:deletion></text:changed-region><text:changed-region text:id="small"><text:deletion><text:p>old</text:p></text:deletion></text:changed-region>"#
        );
        for body in [
            r#"<text:p><text:change text:change-id="d"/></text:p><text:p>visible<text:change text:change-id="small"/></text:p>"#,
            r#"<text:p>visible<text:change text:change-id="small"/></text:p><text:p><text:change text:change-id="d"/></text:p>"#,
        ] {
            assert_eq!(
                document_to_markdown(
                    &reject_document(
                        &tracked(&definitions, body),
                        "",
                        &Limits { max_xml_nodes: 128, ..Limits::default() }
                    )
                    .unwrap()
                ),
                "visibleold\n"
            );
        }
        let definitions = r#"<text:changed-region text:id="d"><text:deletion><text:p><text:hidden-paragraph text:is-hidden="false"/>old</text:p></text:deletion></text:changed-region>"#;
        let body = tracked(
            definitions,
            r#"<text:p><text:hidden-paragraph text:is-hidden="true"/>before<text:change text:change-id="d"/>after</text:p>"#,
        );
        assert_eq!(
            document_to_markdown(&reject_document(&body, "", &Limits::default()).unwrap()),
            "beforeoldafter\n"
        );
    }

    #[test]
    fn external_styles_control_admission_before_visible_materialization() {
        let payload = "<text:span>x</text:span>".repeat(35);
        let definition = format!(
            r#"<text:changed-region text:id="d"><text:deletion><text:p>{payload}<text:span text:style-name="Show">shown</text:span></text:p></text:deletion></text:changed-region>"#
        );
        let body = tracked(
            &definition,
            r#"<text:p><text:span text:style-name="Hide"><text:change text:change-id="d"/></text:span>visible<text:change text:change-id="d"/></text:p>"#,
        );
        let content = format!(
            "<office:document-content {NS}><office:body><office:text>{body}</office:text></office:body></office:document-content>"
        );
        let styles =
            format!("<office:document-styles {NS}>{HIDDEN_STYLES}</office:document-styles>");
        let bytes = zip_package(&[("content.xml", &content), ("styles.xml", &styles)]);
        let limits = Limits { max_xml_nodes: 128, ..Limits::default() };
        let ctx = ParseCtx::new(&limits).with_revisions(RevisionPolicy::Reject);
        assert_eq!(
            document_to_markdown(&super::super::parse(&ctx, &bytes).unwrap()),
            format!("shownvisible{}shown\n", "x".repeat(35))
        );
    }

    #[test]
    fn join_field_profiles_stream_over_reused_deletion_plans() {
        let mut definitions = String::from(
            r#"<text:changed-region text:id="d0"><text:deletion><text:p>first</text:p><text:p>last</text:p></text:deletion></text:changed-region><text:changed-region text:id="d1"><text:deletion><text:p><text:hidden-paragraph text:is-hidden="true"/><text:change text:change-id="d0"/><text:hidden-paragraph text:is-hidden="true"/></text:p><text:p><text:hidden-paragraph text:is-hidden="true"/><text:change text:change-id="d0"/><text:hidden-paragraph text:is-hidden="false"/></text:p></text:deletion></text:changed-region>"#,
        );
        for depth in 2..=12 {
            write!(definitions, "<text:changed-region text:id=\"d{depth}\"><text:deletion><text:p><text:change text:change-id=\"d{}\"/></text:p><text:p><text:change text:change-id=\"d{}\"/></text:p></text:deletion></text:changed-region>", depth-1, depth-1).unwrap();
        }
        let tree =
            raw(&tracked(&definitions, r#"<text:p><text:change text:change-id="d12"/></text:p>"#));
        let ctx = ParseCtx::default().with_revisions(RevisionPolicy::Reject);
        let mut selector = Selector::new(&ctx);
        selector.index(&tree).unwrap();
        let source = tree.descendant_elems().find(|source| source.is(ns::OFFICE, "text")).unwrap();
        let scope = selector.nearest.get(&std::ptr::from_ref(source)).copied().flatten();
        let plan = selector.compile(Request { source, scope, id: None, inline: false }).unwrap();
        let styles = OdfStyles::default();
        let flow = Flow::new(&ctx, &styles, &selector.plans, &selector.parents, source, None);
        let mut cursor = ProfileCursor::new(
            &flow,
            Reference { plan, source, kind: ReferenceKind::Story },
            0..selector.plans[plan].full.end,
            0,
        )
        .ok()
        .unwrap();
        let mut profiles = 0;
        let mut hidden = 0;
        let mut frames = 0;
        while let Some((_, _, hides)) = cursor.next().ok().unwrap() {
            profiles += 1;
            hidden += usize::from(hides);
            frames = frames.max(cursor.pending.len());
            assert!(cursor.flow.out.is_empty());
            assert!(cursor.flow.field_cache.borrow().len() <= 4);
            assert!(cursor.flow.closed_profile.is_none());
        }
        assert_eq!(profiles, 8192);
        assert_eq!(hidden, 6144);
        assert!(frames <= 14);
        assert!(selector.plans.iter().map(|plan| plan.tokens.len()).sum::<usize>() < 200);
        eprintln!(
            "ODF field profile DAG: {profiles} paragraphs, {frames} active plan frames, {} source field folds",
            cursor.flow.field_cache.borrow().len()
        );
        let body = tracked(
            &definitions,
            r#"<text:section text:display="none"><text:p><text:change text:change-id="d12"/></text:p></text:section><text:p>kept</text:p>"#,
        );
        assert_eq!(
            document_to_markdown(&reject_document(&body, "", &Limits::default()).unwrap()),
            "kept\n"
        );
    }
}

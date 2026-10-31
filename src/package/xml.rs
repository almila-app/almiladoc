//! Namespace-aware DOM over quick-xml, its names resolved to interned namespace URIs.
//! [`parse`] lists what fails.

use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::limits::{Limit, Limits};
use crate::package::archive::{Role, report};
use quick_xml::errors::IllFormedError;
use quick_xml::events::attributes::Attribute;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::{QName, ResolveResult};
use quick_xml::{NsReader, XmlVersion};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// Well-known namespace URIs.
pub(crate) mod ns {
    pub(crate) const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
    pub(crate) const R: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    pub(crate) const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
    pub(crate) const PIC: &str = "http://schemas.openxmlformats.org/drawingml/2006/picture";
    pub(crate) const WP: &str =
        "http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing";
    pub(crate) const MC: &str = "http://schemas.openxmlformats.org/markup-compatibility/2006";
    pub(crate) const CHART: &str = "http://schemas.openxmlformats.org/drawingml/2006/chart";
    pub(crate) const DGM: &str = "http://schemas.openxmlformats.org/drawingml/2006/diagram";
    pub(crate) const P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
    pub(crate) const SML: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
    pub(crate) const XDR: &str =
        "http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing";
    pub(crate) const PKG_RELS: &str =
        "http://schemas.openxmlformats.org/package/2006/relationships";

    pub(crate) const OFFICE: &str = "urn:oasis:names:tc:opendocument:xmlns:office:1.0";
    pub(crate) const TEXT: &str = "urn:oasis:names:tc:opendocument:xmlns:text:1.0";
    pub(crate) const TABLE: &str = "urn:oasis:names:tc:opendocument:xmlns:table:1.0";
    pub(crate) const DRAW: &str = "urn:oasis:names:tc:opendocument:xmlns:drawing:1.0";
    pub(crate) const STYLE: &str = "urn:oasis:names:tc:opendocument:xmlns:style:1.0";
    pub(crate) const FO: &str = "urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0";
    pub(crate) const PRESENTATION: &str = "urn:oasis:names:tc:opendocument:xmlns:presentation:1.0";
    pub(crate) const MANIFEST: &str = "urn:oasis:names:tc:opendocument:xmlns:manifest:1.0";
    pub(crate) const XLINK: &str = "http://www.w3.org/1999/xlink";
    pub(crate) const XML: &str = "http://www.w3.org/XML/1998/namespace";
    pub(crate) const SVG_COMPAT: &str = "urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0";
    pub(crate) const FORM: &str = "urn:oasis:names:tc:opendocument:xmlns:form:1.0";
    pub(crate) const NUMBER: &str = "urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0";

    pub(crate) const VML: &str = "urn:schemas-microsoft-com:vml";
    pub(crate) const O_VML: &str = "urn:schemas-microsoft-com:office:office";
    pub(crate) const X_VML: &str = "urn:schemas-microsoft-com:office:excel";
    pub(crate) const WPS: &str =
        "http://schemas.microsoft.com/office/word/2010/wordprocessingShape";
    pub(crate) const WPG: &str =
        "http://schemas.microsoft.com/office/word/2010/wordprocessingGroup";
    pub(crate) const A14: &str = "http://schemas.microsoft.com/office/drawing/2010/main";

    pub(crate) const M: &str = "http://schemas.openxmlformats.org/officeDocument/2006/math";
    pub(crate) const MATHML: &str = "http://www.w3.org/1998/Math/MathML";
}

#[derive(Debug, PartialEq)]
pub(crate) enum Node {
    Elem(Element),
    Text(String),
}

impl Node {
    fn elem(&self) -> Option<&Element> {
        match self {
            Self::Elem(e) => Some(e),
            Self::Text(_) => None,
        }
    }
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Element {
    /// Resolved namespace URI (interned); `None` for unqualified names.
    pub ns: Option<Rc<str>>,
    pub local: String,
    pub attrs: Vec<Attr>,
    pub children: Vec<Node>,
    pub unresolved_text: bool,
}

#[derive(Debug, PartialEq)]
pub(crate) struct Attr {
    pub ns: Option<Rc<str>>,
    pub local: String,
    pub value: String,
}

impl Element {
    pub(crate) fn is(&self, ns: &str, local: &str) -> bool {
        self.local == local && self.ns.as_deref() == Some(ns)
    }

    /// Same-vocabulary attribute lookup: the qualified attribute wins, and
    /// an explicitly unqualified one with the same local name is accepted
    /// as a deliberate leniency - schemas often leave their own attributes
    /// unqualified (DrawingML `gridSpan`) and producers vary. Lookups in a
    /// *different* vocabulary than the element's (`r:id`, `xml:id`) must use
    /// [`Element::attr_qualified`], where that fallback would misattribute.
    pub(crate) fn attr(&self, ns: &str, local: &str) -> Option<&str> {
        self.attr_qualified(ns, local).or_else(|| self.attr_unqualified(local))
    }

    /// Strictly namespace-qualified attribute lookup, no fallback.
    pub(crate) fn attr_qualified(&self, ns: &str, local: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|a| a.local == local && a.ns.as_deref() == Some(ns))
            .map(|a| a.value.as_str())
    }

    /// Explicitly unqualified attribute lookup (no namespace), no fallback.
    pub(crate) fn attr_unqualified(&self, local: &str) -> Option<&str> {
        self.attrs.iter().find(|a| a.local == local && a.ns.is_none()).map(|a| a.value.as_str())
    }

    /// Attribute by local name regardless of namespace. For elements whose
    /// vocabulary is unambiguous in context (relationships, container files).
    pub(crate) fn attr_any(&self, local: &str) -> Option<&str> {
        self.attrs.iter().find(|a| a.local == local).map(|a| a.value.as_str())
    }

    pub(crate) fn child_elems(&self) -> impl DoubleEndedIterator<Item = &Self> {
        self.children.iter().filter_map(Node::elem)
    }

    pub(crate) fn find(&self, ns: &str, local: &str) -> Option<&Self> {
        self.child_elems().find(|e| e.is(ns, local))
    }

    pub(crate) fn find_all<'a>(
        &'a self,
        ns: &'a str,
        local: &'a str,
    ) -> impl Iterator<Item = &'a Self> {
        self.child_elems().filter(move |e| e.is(ns, local))
    }

    /// All descendant nodes (elements and text), depth-first in document
    /// order, iteratively (deep DOMs must not overflow the call stack).
    fn descendant_nodes(&self) -> impl Iterator<Item = &Node> {
        let mut stack: Vec<&Node> = self.children.iter().rev().collect();
        std::iter::from_fn(move || {
            let node = stack.pop()?;
            if let Node::Elem(e) = node {
                stack.extend(e.children.iter().rev());
            }
            Some(node)
        })
    }

    /// All descendant elements, depth-first in document order, lazily.
    pub(crate) fn descendant_elems(&self) -> impl Iterator<Item = &Self> {
        self.descendant_nodes().filter_map(Node::elem)
    }

    /// First descendant with the given name, depth-first.
    pub(crate) fn first_descendant(&self, ns: &str, local: &str) -> Option<&Self> {
        self.descendant_elems().find(|e| e.is(ns, local))
    }

    /// All descendants with the given name, in document order, lazily.
    pub(crate) fn descendants<'a>(
        &'a self,
        ns: &'a str,
        local: &'a str,
    ) -> impl Iterator<Item = &'a Self> {
        self.descendant_elems().filter(move |e| e.is(ns, local))
    }

    /// The descendants with the given name that no other such descendant
    /// holds, in document order: a match's own subtree is never walked.
    pub(crate) fn outermost<'a>(
        &'a self,
        ns: &'a str,
        local: &'a str,
    ) -> impl Iterator<Item = &'a Self> {
        let mut stack: Vec<&Self> = self.children.iter().rev().filter_map(Node::elem).collect();
        std::iter::from_fn(move || {
            while let Some(elem) = stack.pop() {
                if elem.is(ns, local) {
                    return Some(elem);
                }
                stack.extend(elem.children.iter().rev().filter_map(Node::elem));
            }
            None
        })
    }

    /// All descendants matching a local name regardless of namespace, in
    /// document order. For vocabularies where the namespace varies across
    /// producers (EPUB container/OPF metadata).
    pub(crate) fn descendants_any<'a>(&'a self, local: &'a str) -> impl Iterator<Item = &'a Self> {
        self.descendant_elems().filter(move |e| e.local == local)
    }

    pub(crate) fn checked_text(&self, parse_ctx: &ParseCtx<'_>) -> Result<String, ConvertError> {
        let mut text = String::new();
        for node in self.descendant_nodes() {
            parse_ctx.charge(1)?;
            if let Node::Text(value) = node {
                parse_ctx.charge(value.len() as u64)?;
                text.push_str(value);
            }
        }
        Ok(text)
    }

    /// Concatenated text of all descendant text nodes.
    pub(crate) fn text(&self) -> String {
        self.descendant_nodes()
            .filter_map(|node| match node {
                Node::Text(text) => Some(text.as_str()),
                Node::Elem(_) => None,
            })
            .collect()
    }
}

/// Which named references a part's markup may use: declared (DTD) entities
/// never expand in either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dialect {
    /// XML's five predefined entities alone: OOXML and OpenDocument parts.
    Xml,
    /// HTML5's named references as well: EPUB's XHTML and package documents.
    Xhtml,
}

impl Dialect {
    fn named(self, name: &str) -> Option<&'static str> {
        match self {
            Self::Xml => quick_xml::escape::resolve_xml_entity(name),
            // quick-xml 0.42's table cuts each five-digit `*fr`, `*scr` and `*opf`
            // code point to its first four digits, all in U+1D00..=U+1DFF, a
            // block no HTML5 reference names.
            Self::Xhtml => quick_xml::escape::resolve_html5_entity(name).filter(|value| {
                let mut chars = value.chars();
                !matches!((chars.next(), chars.next()), (Some('\u{1D00}'..='\u{1DFF}'), None))
            }),
        }
    }

    fn reference(self, name: &str) -> Reference {
        if let Some(number) = name.strip_prefix('#') {
            return char_reference(number).map_or(Reference::Unresolved, Reference::Char);
        }
        if !is_name(name) {
            return Reference::Literal;
        }
        self.named(name).map_or(Reference::Unresolved, Reference::Named)
    }
}

/// A reference's reading: a character, a named replacement, a name the
/// dialect does not know (kept literally, a loss), or an ampersand that
/// begins no reference (kept literally, as written).
enum Reference {
    Char(char),
    Named(&'static str),
    Unresolved,
    Literal,
}

/// A character reference's number as XML reads it: decimal, or hexadecimal
/// after a lowercase `x`, unsigned, naming a scalar value other than NUL.
fn char_reference(number: &str) -> Option<char> {
    let (digits, radix) = number.strip_prefix('x').map_or((number, 10), |hex| (hex, 16));
    if digits.starts_with(['+', '-']) {
        return None;
    }
    let code = u32::from_str_radix(digits, radix).ok().filter(|&code| code != 0)?;
    char::from_u32(code)
}

/// Whether `name` is an XML 1.0 (fifth edition) `Name`, so `&name;` is a
/// reference rather than an ampersand followed by text.
pub(crate) fn is_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(is_name_start_char) && chars.all(is_name_char)
}

/// XML 1.0 (fifth edition) `NameStartChar`.
fn is_name_start_char(c: char) -> bool {
    matches!(c,
        ':' | 'A'..='Z' | '_' | 'a'..='z'
        | '\u{C0}'..='\u{D6}' | '\u{D8}'..='\u{F6}' | '\u{F8}'..='\u{2FF}'
        | '\u{370}'..='\u{37D}' | '\u{37F}'..='\u{1FFF}' | '\u{200C}'..='\u{200D}'
        | '\u{2070}'..='\u{218F}' | '\u{2C00}'..='\u{2FEF}' | '\u{3001}'..='\u{D7FF}'
        | '\u{F900}'..='\u{FDCF}' | '\u{FDF0}'..='\u{FFFD}' | '\u{10000}'..='\u{EFFFF}')
}

/// XML 1.0 (fifth edition) `NameChar`.
fn is_name_char(c: char) -> bool {
    is_name_start_char(c)
        || matches!(c,
            '-' | '.' | '0'..='9' | '\u{B7}' | '\u{300}'..='\u{36F}' | '\u{203F}'..='\u{2040}')
}

/// A parsed part, what building it saw that the tree cannot show, and the
/// nodes it holds.
pub(crate) struct Parsed {
    pub root: Element,
    pub marks: Marks,
    pub nodes: u64,
}

/// Where nodes go as they complete instead of joining the tree: a reader
/// streams a part's repeated records through it, so the parse holds one
/// record's nodes at a time.
pub(crate) trait NodeSink {
    /// Whether a node completing under `ancestors`, outermost first, leaves
    /// the tree for the sink.
    fn takes(&self, ancestors: &[Element]) -> bool;

    /// Receive a closed element the sink takes under `ancestors`; text it
    /// takes is dropped, and an error ends the parse.
    fn take(&mut self, ancestors: &[Element], elem: Element) -> Result<(), ConvertError>;
}

/// The sink that takes nothing: the whole part stays in the tree.
pub(crate) struct WholeTree;

impl NodeSink for WholeTree {
    fn takes(&self, _: &[Element]) -> bool {
        false
    }

    fn take(&mut self, _: &[Element], _: Element) -> Result<(), ConvertError> {
        Ok(())
    }
}

/// [`parse`] in the XML dialect, with its marks reported by `role`.
#[cfg(test)]
pub(crate) fn parse_xml(
    parse_ctx: &ParseCtx<'_>,
    bytes: &[u8],
    role: Role,
) -> Result<Element, ConvertError> {
    let parsed = parse(parse_ctx, bytes, Dialect::Xml, &mut WholeTree)?;
    parsed.marks.report(parse_ctx, role);
    Ok(parsed.root)
}

/// Parse an XML part into a synthetic root element containing the top-level
/// nodes, and return what the tree cannot show as [`Marks`] for each
/// reading call to report by its role. Encoding comes from the BOM or XML
/// declaration; the part is transcoded to UTF-8 before parsing so namespace
/// resolution sees one consistent encoding. `Malformed`: bytes that do not
/// decode or an encoding label no decoder names, a mismatched or unclosed
/// element, an ill-formed or repeated (qualified or expanded) attribute, an
/// unbound prefix, a `<` that opens no well-formed tag, a processing
/// instruction without a target, a declaration or document type inside the
/// content, and in [`Dialect::Xml`] a document type with an internal subset.
/// `ResourceLimit`: `max_work` spent, nesting past `max_nesting_depth`, or
/// more than `max_xml_nodes` nodes held; an error `sink` returns passes
/// through. Every node `sink` takes goes to
/// it as it completes and is released, so `max_xml_nodes` bounds the nodes
/// held at once.
pub(crate) fn parse(
    parse_ctx: &ParseCtx<'_>,
    bytes: &[u8],
    dialect: Dialect,
    sink: &mut dyn NodeSink,
) -> Result<Parsed, ConvertError> {
    let limits = parse_ctx.limits();
    let (utf8, fault) = to_utf8(bytes);
    if let Some(reason) = fault {
        return Err(ConvertError::malformed(reason));
    }
    let mut reader = NsReader::from_reader(utf8.as_ref());
    reader.config_mut().allow_dangling_amp = true;

    let mut interner: HashMap<String, Namespace> = HashMap::new();
    let mut tree = Tree::default();
    let mut marks = Marks::default();

    loop {
        let event = reader.read_event().map_err(|e| ConvertError::malformed(unparseable(&e)))?;
        parse_ctx.charge(1)?;
        reject_misplaced(&event, dialect, &tree)?;
        match event {
            Event::Start(e) => {
                check_start_tag(&e)?;
                tree.open(limits)?;
                tree.stack.push(start_to_element(&e, &reader, &mut interner, dialect, &mut marks)?);
            }
            Event::Empty(e) => {
                check_start_tag(&e)?;
                tree.open(limits)?;
                let elem = start_to_element(&e, &reader, &mut interner, dialect, &mut marks)?;
                tree.close(elem, sink)?;
            }
            Event::End(_) => {
                let elem = tree
                    .stack
                    .pop()
                    .ok_or_else(|| ConvertError::malformed("mismatched end tag"))?;
                tree.close(elem, sink)?;
            }
            Event::Text(e) => tree.text(e.into_inner(), sink, limits.max_xml_nodes)?,
            Event::GeneralRef(e) => {
                let mut reference = Marks::default();
                let text = reference_text(dialect, &e, &mut reference);
                marks.unresolved |= reference.unresolved;
                tree.stack.last_mut().unwrap_or(&mut tree.root).unresolved_text |=
                    reference.unresolved;
                tree.text(text, sink, limits.max_xml_nodes)?;
            }
            Event::CData(e) => tree.text(e.into_inner(), sink, limits.max_xml_nodes)?,
            Event::Eof => break,
            Event::PI(_) | Event::Decl(_) | Event::DocType(_) | Event::Comment(_) => {}
        }
    }
    if !tree.stack.is_empty() {
        return Err(ConvertError::malformed("unclosed element"));
    }
    Ok(Parsed { root: tree.root, marks, nodes: tree.nodes })
}

#[derive(Default)]
struct Tree {
    root: Element,
    stack: Vec<Element>,
    held_before: Vec<u64>,
    nodes: u64,
}

impl Tree {
    fn open(&mut self, limits: &Limits) -> Result<(), ConvertError> {
        let max = limits.max_nesting_depth;
        if self.stack.len() as u64 >= max {
            return Err(ConvertError::limit(Limit::NestingDepth, max));
        }
        self.held_before.push(self.nodes);
        self.bump(limits.max_xml_nodes)
    }

    fn bump(&mut self, max: u64) -> Result<(), ConvertError> {
        self.nodes += 1;
        if self.nodes > max {
            return Err(ConvertError::limit(Limit::XmlNodes, max));
        }
        Ok(())
    }

    fn close(&mut self, elem: Element, sink: &mut dyn NodeSink) -> Result<(), ConvertError> {
        let before = self.held_before.pop().unwrap_or_default();
        if sink.takes(&self.stack) {
            self.nodes = before;
            return sink.take(&self.stack, elem);
        }
        self.stack.last_mut().unwrap_or(&mut self.root).unresolved_text |= elem.unresolved_text;
        self.siblings().push(Node::Elem(elem));
        Ok(())
    }

    fn text(
        &mut self,
        text: Cow<'_, str>,
        sink: &dyn NodeSink,
        max: u64,
    ) -> Result<(), ConvertError> {
        if text.is_empty() || sink.takes(&self.stack) {
            return Ok(());
        }
        self.bump(max)?;
        let siblings = self.siblings();
        match siblings.last_mut() {
            Some(Node::Text(prev)) => prev.push_str(&text),
            _ => siblings.push(Node::Text(text.into_owned())),
        }
        Ok(())
    }

    fn siblings(&mut self) -> &mut Vec<Node> {
        &mut self.stack.last_mut().unwrap_or(&mut self.root).children
    }

    fn began(&self) -> bool {
        !self.stack.is_empty() || self.root.child_elems().next().is_some()
    }
}

fn reference_text(dialect: Dialect, name: &str, marks: &mut Marks) -> Cow<'static, str> {
    match dialect.reference(name) {
        Reference::Char(c) => Cow::Owned(c.to_string()),
        Reference::Named(replacement) => Cow::Borrowed(replacement),
        reference => {
            marks.unresolved |= matches!(reference, Reference::Unresolved);
            Cow::Owned(format!("&{name};"))
        }
    }
}

fn reject_misplaced(event: &Event<'_>, dialect: Dialect, tree: &Tree) -> Result<(), ConvertError> {
    let reason = match event {
        Event::PI(e) if !is_name(e.target()) => "processing instruction without a target",
        Event::Decl(_) | Event::DocType(_) if tree.began() => "declaration inside the content",
        Event::DocType(e) if dialect == Dialect::Xml && has_internal_subset(e) => {
            "document type with an internal subset"
        }
        _ => return Ok(()),
    };
    Err(ConvertError::malformed(reason))
}

/// What a parse saw that its tree cannot show.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Marks {
    /// A reference the dialect does not resolve stayed literal.
    pub unresolved: bool,
}

impl Marks {
    pub(crate) fn report(self, parse_ctx: &ParseCtx<'_>, role: Role) {
        if self.unresolved {
            report(parse_ctx, role.unresolved());
        }
    }
}

/// The namespace of the first start tag in `head`, the leading bytes of a
/// part, normalized from Strict to Transitional; markup past that tag is
/// never read, so a head cut short anywhere after it, even inside a
/// character, still names it.
pub(crate) fn root_namespace(head: &[u8]) -> Option<String> {
    let (utf8, _) = to_utf8(head);
    let mut reader = NsReader::from_reader(utf8.as_ref());
    loop {
        match reader.read_resolved_event().ok()? {
            (ResolveResult::Bound(namespace), Event::Start(_) | Event::Empty(_)) => {
                let uri: &str = namespace.as_ref();
                return Some(normalize_ooxml_uri(uri).unwrap_or_else(|| uri.to_owned()));
            }
            (_, Event::Start(_) | Event::Empty(_) | Event::Eof) => return None,
            _ => {}
        }
    }
}

/// A start tag whose name is empty or that holds a `<`: quick-xml reads a
/// bare `<` in text, and the markup after it up to the next `>`, as one tag;
/// an end tag it reads that way names no open element, which quick-xml
/// refuses.
fn check_start_tag(e: &BytesStart<'_>) -> Result<(), ConvertError> {
    if e.name().as_ref().is_empty() || e.contains('<') {
        return Err(ConvertError::malformed("bare `<` in markup"));
    }
    Ok(())
}

fn unparseable(error: &quick_xml::Error) -> &'static str {
    match error {
        quick_xml::Error::IllFormed(
            IllFormedError::MismatchedEndTag { .. } | IllFormedError::UnmatchedEndTag(_),
        ) => "mismatched end tag",
        _ => "unparseable xml",
    }
}

/// Whether a document type declaration holds an internal subset, a `[`
/// outside its quoted identifiers.
fn has_internal_subset(doctype: &str) -> bool {
    let mut quote = None;
    for c in doctype.chars() {
        match (quote, c) {
            (None, '[') => return true,
            (None, '"' | '\'') => quote = Some(c),
            (Some(open), c) if c == open => quote = None,
            _ => {}
        }
    }
    false
}

/// Transcode an XML part to UTF-8 based on its BOM or encoding declaration,
/// beside why it cannot be read faithfully: bytes that did not decode, or a
/// declared label no decoder names, which leaves the bytes as they are.
pub(crate) fn to_utf8(bytes: &[u8]) -> (Cow<'_, [u8]>, Option<&'static str>) {
    let decoded = |encoding: &'static encoding_rs::Encoding| {
        let (s, _, had_errors) = encoding.decode(bytes);
        let fault = had_errors.then_some("bytes that do not decode");
        (Cow::Owned(s.into_owned().into_bytes()), fault)
    };
    match bytes {
        [0xFF, 0xFE, ..] => decoded(encoding_rs::UTF_16LE),
        [0xFE, 0xFF, ..] => decoded(encoding_rs::UTF_16BE),
        [0xEF, 0xBB, 0xBF, rest @ ..] => (Cow::Borrowed(rest), None),
        _ => {
            let head = &bytes[..bytes.len().min(200)];
            let Some(label) = declared_encoding(head) else {
                return (Cow::Borrowed(bytes), None);
            };
            match encoding_rs::Encoding::for_label(label.as_bytes()) {
                None => (Cow::Borrowed(bytes), Some("unknown encoding")),
                Some(encoding) if encoding == encoding_rs::UTF_8 => (Cow::Borrowed(bytes), None),
                Some(encoding) => decoded(encoding),
            }
        }
    }
}

fn declared_encoding(head: &[u8]) -> Option<&str> {
    let declaration = head
        .strip_prefix(b"<?xml")
        .filter(|rest| rest.first().is_some_and(u8::is_ascii_whitespace))?;
    let end = declaration.windows(2).position(|w| w == b"?>")?;
    let head = std::str::from_utf8(&declaration[..end]).ok()?;
    let (_, rest) = head.split_once("encoding")?;
    let rest = rest.trim_start().strip_prefix('=')?.trim_start();
    let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    rest.strip_prefix(quote)?.split_once(quote).map(|(label, _)| label)
}

/// ISO/IEC 29500 *Strict* namespace and relationship-type URIs are the
/// Transitional ones re-rooted under `http://purl.oclc.org/ooxml/` with the
/// `2006` version segment dropped. Normalizing to the Transitional family at
/// parse time lets the rest of the crate match a single set of constants.
pub(crate) fn normalize_ooxml_uri(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("http://purl.oclc.org/ooxml/")?;
    let (family, tail) = rest.split_once('/')?;
    Some(format!("http://schemas.openxmlformats.org/{family}/2006/{tail}"))
}

/// A bound namespace: `name`, the declared value with its references read
/// as an attribute's are, identifies it, and `dom` is that name rewritten
/// from Strict to Transitional, as the tree holds it.
#[derive(Clone)]
struct Namespace {
    name: Rc<str>,
    dom: Rc<str>,
}

/// quick-xml resolves a prefix to its declaration's raw value, references
/// unread.
fn intern(
    interner: &mut HashMap<String, Namespace>,
    raw: &str,
    dialect: Dialect,
    marks: &mut Marks,
) -> Namespace {
    if let Some(namespace) = interner.get(raw) {
        return namespace.clone();
    }
    let escaped = escape_unread_references(Cow::Borrowed(raw), dialect, marks);
    let read = quick_xml::escape::unescape_with(&escaped, |name| dialect.named(name))
        .map_or_else(|_| raw.to_owned(), Cow::into_owned);
    let name: Rc<str> = Rc::from(read);
    let dom = normalize_ooxml_uri(&name).map_or_else(|| Rc::clone(&name), Rc::from);
    let namespace = Namespace { name, dom };
    interner.insert(raw.to_owned(), namespace.clone());
    namespace
}

fn start_to_element(
    e: &BytesStart<'_>,
    reader: &NsReader<&[u8]>,
    interner: &mut HashMap<String, Namespace>,
    dialect: Dialect,
    marks: &mut Marks,
) -> Result<Element, ConvertError> {
    let (res, local) = reader.resolver().resolve_element(e.name());
    let ns = bound(interner, &res, dialect, marks)?.map(|namespace| namespace.dom);
    let local = local.as_ref().to_owned();
    let mut attrs = Vec::new();
    let mut names = Vec::new();
    for attr in e.attributes() {
        let attr = attr.map_err(|_| ConvertError::malformed("ill-formed attribute"))?;
        if attr.key.as_namespace_binding().is_some() {
            continue;
        }
        let (res, alocal) = reader.resolver().resolve_attribute(attr.key);
        let ans = bound(interner, &res, dialect, marks)?;
        let value = attribute_value(attr, dialect, marks)?;
        let ns = ans.map(|namespace| {
            names.push(namespace.name);
            namespace.dom
        });
        attrs.push(Attr { ns, local: alocal.as_ref().to_owned(), value });
    }
    check_expanded_names(&names, &attrs)?;
    // `mc:Choice/@Requires` holds prefixes in the element's lexical scope; resolve
    // them here to URIs. Unresolvable prefixes stay literal and match no requirement.
    if local == "Choice"
        && ns.as_deref() == Some(ns::MC)
        && let Some(requires) = attrs.iter_mut().find(|a| a.local == "Requires")
    {
        requires.value = requires
            .value
            .split_whitespace()
            .map(|prefix| {
                let probe = format!("{prefix}:x");
                match reader.resolver().resolve_element(QName(&probe)).0 {
                    ResolveResult::Bound(namespace) => {
                        intern(interner, namespace.as_ref(), dialect, marks).dom.to_string()
                    }
                    _ => prefix.to_string(),
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
    }
    Ok(Element { ns, local, attrs, children: Vec::new(), unresolved_text: false })
}

/// A name's interned namespace, `None` for an unprefixed name outside any
/// default namespace; a prefix no declaration binds is `Malformed`.
fn bound(
    interner: &mut HashMap<String, Namespace>,
    resolved: &ResolveResult<'_>,
    dialect: Dialect,
    marks: &mut Marks,
) -> Result<Option<Namespace>, ConvertError> {
    match resolved {
        ResolveResult::Bound(namespace) => {
            Ok(Some(intern(interner, namespace.as_ref(), dialect, marks)))
        }
        ResolveResult::Unbound => Ok(None),
        ResolveResult::Unknown(_) => Err(ConvertError::malformed("unbound prefix")),
    }
}

/// Two attributes of one start tag with one namespace and local name are
/// `Malformed`: quick-xml compares qualified names alone. `names` holds the
/// namespace name of each attribute in `attrs` that has one, in order.
fn check_expanded_names(names: &[Rc<str>], attrs: &[Attr]) -> Result<(), ConvertError> {
    if names.len() < 2 {
        return Ok(());
    }
    let locals = attrs.iter().filter(|attr| attr.ns.is_some()).map(|attr| attr.local.as_str());
    let mut seen = HashSet::new();
    if names.iter().map(|name| &**name).zip(locals).all(|qualified| seen.insert(qualified)) {
        Ok(())
    } else {
        Err(ConvertError::malformed("repeated attribute"))
    }
}

/// An attribute's value normalized as XML 1.0 does, its references read as
/// text reads them: one the dialect does not know stays literal and marks
/// the part, and an ampersand that begins no reference stays as written.
fn attribute_value(
    attr: Attribute<'_>,
    dialect: Dialect,
    marks: &mut Marks,
) -> Result<String, ConvertError> {
    let value = escape_unread_references(attr.value, dialect, marks);
    let attr = Attribute { key: attr.key, value };
    let value = attr
        .normalized_value_with(XmlVersion::Implicit1_0, 2, |name| match dialect.named(name)? {
            "&" => Some("&amp;"),
            replacement => Some(replacement),
        })
        .map_err(|_| ConvertError::malformed("unparseable attribute value"))?;
    Ok(value.into_owned())
}

/// `raw` with every `&` that begins no reference the dialect reads written
/// as `&amp;`, so the normalization keeps it literally.
fn escape_unread_references<'v>(
    raw: Cow<'v, str>,
    dialect: Dialect,
    marks: &mut Marks,
) -> Cow<'v, str> {
    if !raw.contains('&') {
        return raw;
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw.as_ref();
    while let Some((before, after)) = rest.split_once('&') {
        out.push_str(before);
        let name = match after.find([';', '&']) {
            Some(end) if after.as_bytes().get(end) == Some(&b';') => after.get(..end),
            _ => None,
        };
        out.push_str(match name.map(|name| dialect.reference(name)) {
            Some(Reference::Char(_) | Reference::Named(_)) => "&",
            Some(Reference::Unresolved) => {
                marks.unresolved = true;
                "&amp;"
            }
            Some(Reference::Literal) | None => "&amp;",
        });
        rest = after;
    }
    out.push_str(rest);
    Cow::Owned(out)
}

#[cfg(test)]
#[expect(
    clippy::format_collect,
    clippy::format_push_string,
    reason = "fixture XML is built with format!"
)]
mod tests {
    use super::*;
    use crate::diagnostics::{DiagnosticCode, LossClass, Region};
    use crate::shared::test_support::diagnostic_rows;
    use std::assert_matches;

    /// Takes every node directly under a top-level `list`, keeping the text
    /// of each element it takes.
    #[derive(Default)]
    struct Items(Vec<String>);

    impl NodeSink for Items {
        fn takes(&self, ancestors: &[Element]) -> bool {
            matches!(ancestors, [list] if list.local == "list")
        }

        fn take(&mut self, _: &[Element], elem: Element) -> Result<(), ConvertError> {
            self.0.push(elem.text());
            Ok(())
        }
    }

    #[test]
    fn a_declared_encoding_is_read_between_either_quote() {
        let double = br#"<?xml version="1.0" encoding="ISO-8859-1"?>"#;
        let single = b"<?xml version='1.0' encoding = 'windows-1252' ?>";
        assert_eq!(declared_encoding(double), Some("ISO-8859-1"));
        assert_eq!(declared_encoding(single), Some("windows-1252"));
        assert_eq!(declared_encoding(b"<?xml encoding=latin1?>"), None);
        assert_eq!(declared_encoding(b"<?xml encoding='latin1?>"), None);
        assert_eq!(declared_encoding(b"<?xml version='1.0'?>"), None);
        assert_eq!(declared_encoding(b"<?pi encoding='x-bogus'?><r/>"), None);
        assert_eq!(declared_encoding(b"<?xml-stylesheet encoding='x-bogus'?>"), None);
        assert_eq!(declared_encoding(b" <?xml version='1.0' encoding='x-bogus'?>"), None);
    }

    #[test]
    fn a_sink_holds_one_record_at_a_time_against_the_node_limit() {
        let limits = Limits { max_xml_nodes: 4, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        let xml = b"<list>\n<i>a</i>\n<i>b&amp;</i>\n<i/><i>c<![CDATA[d]]></i>\n</list><tail/>";
        let mut items = Items::default();
        let parsed = parse(&parse_ctx, xml, Dialect::Xml, &mut items).unwrap();
        assert_eq!(items.0, ["a", "b&", "", "cd"]);
        assert_eq!(parsed.nodes, 2);
        assert_eq!(parsed.root.child_elems().map(|e| e.children.len()).collect::<Vec<_>>(), [0, 0]);
        let exceeds = |xml: &[u8], sink: &mut dyn NodeSink| {
            let held = parse(&parse_ctx, xml, Dialect::Xml, sink).map(|parsed| parsed.nodes);
            matches!(held, Err(ConvertError::ResourceLimit { limit: Limit::XmlNodes, max: 4 }))
        };
        assert!(exceeds(xml, &mut WholeTree));
        assert!(exceeds(b"<list><i>a<b/><b/></i></list>", &mut Items::default()));
        assert!(exceeds(b"<list><i/></list><o><p/><p/><p/></o>", &mut Items::default()));
    }

    #[test]
    fn a_taken_record_releases_the_nodes_of_the_elements_inside_it() {
        let limits = Limits { max_xml_nodes: 4, ..Limits::default() };
        let xml = b"<list><i><b>a</b></i><i><b>b</b></i><i><b>c</b></i></list>";
        let mut items = Items::default();
        let parsed = parse(&ParseCtx::new(&limits), xml, Dialect::Xml, &mut items).unwrap();
        assert_eq!(items.0, ["a", "b", "c"]);
        assert_eq!(parsed.nodes, 1);
    }

    #[test]
    fn a_record_left_open_at_the_end_fails_the_part_after_the_sink_took_the_closed_ones() {
        let mut items = Items::default();
        let err = parse(&ParseCtx::default(), b"<list><i>a</i><i>b", Dialect::Xml, &mut items)
            .map(|_| ())
            .unwrap_err();
        assert_matches!(err, ConvertError::Malformed { part: None, reason: "unclosed element" });
        assert_eq!(items.0, ["a"]);
    }

    fn default_depth() -> usize {
        usize::try_from(Limits::default().max_nesting_depth).unwrap()
    }

    fn nested(depth: usize) -> Vec<u8> {
        format!("{}{}", "<d>".repeat(depth), "</d>".repeat(depth)).into_bytes()
    }

    #[test]
    fn depth_and_node_limits_are_the_contexts() {
        let limits = Limits { max_nesting_depth: 3, max_xml_nodes: 4, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        parse_xml(&parse_ctx, &nested(3), Role::Probe).unwrap();
        let err = parse_xml(&parse_ctx, &nested(4), Role::Probe).unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::NestingDepth, max: 3 });
        let err = parse_xml(&parse_ctx, b"<d><d><d>", Role::Probe).unwrap_err();
        assert_matches!(err, ConvertError::Malformed { reason: "unclosed element", .. });
        let err = parse_xml(&parse_ctx, b"<d><d><d><d>", Role::Probe).unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::NestingDepth, max: 3 });
        parse_xml(&parse_ctx, b"<d><d><d/></d></d>", Role::Probe).unwrap();
        let err = parse_xml(&parse_ctx, b"<d><d><d><d/></d></d></d>", Role::Probe).unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::NestingDepth, max: 3 });
        parse_xml(&parse_ctx, b"<a><b/>t<c/></a>", Role::Probe).unwrap();
        let err = parse_xml(&parse_ctx, b"<a><b/>t<c/><e/></a>", Role::Probe).unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::XmlNodes, max: 4 });
        let tight = Limits { max_nesting_depth: 3, max_xml_nodes: 3, ..Limits::default() };
        let err = parse_xml(&ParseCtx::new(&tight), &nested(4), Role::Probe).unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::NestingDepth, max: 3 });
    }

    #[test]
    fn every_node_kind_is_charged_against_the_contexts_node_limit() {
        let limits = Limits { max_xml_nodes: 1, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        parse_xml(&parse_ctx, b"<a></a>", Role::Probe).unwrap();
        for second in ["<b></b>", "<b/>", "t", "&amp;", "<![CDATA[t]]>"] {
            let xml = format!("<a>{second}</a>");
            let err = parse_xml(&parse_ctx, xml.as_bytes(), Role::Probe).unwrap_err();
            assert_matches!(
                err,
                ConvertError::ResourceLimit { limit: Limit::XmlNodes, max: 1 },
                "{second}"
            );
        }
    }

    #[test]
    fn an_empty_cdata_section_is_no_node() {
        let parse_ctx = ParseCtx::default();
        let parsed = parse(
            &parse_ctx,
            b"<a><![CDATA[]]>x<![CDATA[]]>y<![CDATA[]]><b/></a>",
            Dialect::Xml,
            &mut WholeTree,
        )
        .unwrap();
        let a = parsed.root.child_elems().next().unwrap();
        assert_matches!(
            a.children.as_slice(),
            [Node::Text(text), Node::Elem(b)] if text == "xy" && b.local == "b"
        );
        assert_eq!(parsed.nodes, 4);
    }

    #[test]
    fn every_xml_event_is_charged_once() {
        let parse_ctx = ParseCtx::default();
        parse_xml(&parse_ctx, b"<?xml version=\"1.0\"?><a><b/>t<!--c--></a>", Role::Probe).unwrap();
        assert_eq!(parse_ctx.fuel(), 7);
        let limits = Limits { max_work: 6, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        let err =
            parse_xml(&parse_ctx, b"<?xml version=\"1.0\"?><a><b/>t<!--c--></a>", Role::Probe)
                .unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::Work, max: 6 });
    }

    #[test]
    fn resolves_namespaces_regardless_of_prefix() {
        let xml =
            br#"<x:root xmlns:x="urn:a" xmlns:y="urn:b"><y:kid x:id="1" plain="p"/></x:root>"#;
        let root = parse_xml(&ParseCtx::default(), xml, Role::Probe).unwrap();
        let r = root.find("urn:a", "root").unwrap();
        let kid = r.find("urn:b", "kid").unwrap();
        assert_eq!(kid.attr("urn:a", "id"), Some("1"));
        assert_eq!(kid.attr("urn:whatever", "plain"), Some("p"));
        assert!(r.find("urn:b", "root").is_none());
    }

    #[test]
    fn qualified_and_unqualified_lookups_are_separate() {
        let xml =
            br#"<x:root xmlns:x="urn:a" xmlns:r="urn:r"><x:kid r:id="rel" id="plain"/></x:root>"#;
        let root = parse_xml(&ParseCtx::default(), xml, Role::Probe).unwrap();
        let kid = root.first_descendant("urn:a", "kid").unwrap();
        assert_eq!(kid.attr_qualified("urn:r", "id"), Some("rel"));
        assert_eq!(kid.attr_unqualified("id"), Some("plain"));
        assert_eq!(kid.attr("urn:r", "id"), Some("rel"));
        assert_eq!(kid.attr_qualified("urn:other", "id"), None);
    }

    #[test]
    fn strict_ooxml_namespaces_normalize_to_transitional() {
        let cases = [
            ("http://purl.oclc.org/ooxml/wordprocessingml/main", Some(ns::W)),
            ("http://purl.oclc.org/ooxml/officeDocument/relationships", Some(ns::R)),
            (
                "http://purl.oclc.org/ooxml/officeDocument/relationships/officeDocument",
                Some(
                    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument",
                ),
            ),
            (ns::W, None),
            ("urn:schemas-microsoft-com:vml", None),
        ];
        for (uri, transitional) in cases {
            assert_eq!(normalize_ooxml_uri(uri).as_deref(), transitional, "{uri}");
        }

        let xml = br#"<w:document xmlns:w="http://purl.oclc.org/ooxml/wordprocessingml/main"><w:body/></w:document>"#;
        let root = parse_xml(&ParseCtx::default(), xml, Role::Probe).unwrap();
        let doc = root.find(ns::W, "document").unwrap();
        assert!(doc.find(ns::W, "body").is_some());
    }

    #[test]
    fn default_namespace_applies_to_elements_not_attributes() {
        let xml = br#"<root xmlns="urn:d"><kid id="1"/></root>"#;
        let root = parse_xml(&ParseCtx::default(), xml, Role::Probe).unwrap();
        let r = root.find("urn:d", "root").unwrap();
        let kid = r.find("urn:d", "kid").unwrap();
        assert_eq!(kid.attr_any("id"), Some("1"));
    }

    #[test]
    fn requires_prefixes_resolve_in_their_lexical_scope() {
        let xml = br#"<mc:AlternateContent
            xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006">
            <mc:Choice xmlns:z="urn:one" Requires="z"/>
            <mc:Choice xmlns:z="urn:two" Requires="z unknownprefix"/>
        </mc:AlternateContent>"#;
        let root = parse_xml(&ParseCtx::default(), xml, Role::Probe).unwrap();
        let alt = root.find(ns::MC, "AlternateContent").unwrap();
        let choices: Vec<&Element> = alt.find_all(ns::MC, "Choice").collect();
        assert_eq!(choices[0].attr_any("Requires"), Some("urn:one"));
        assert_eq!(choices[1].attr_any("Requires"), Some("urn:two unknownprefix"));
    }

    #[test]
    fn utf16_with_bom_decodes() {
        let text = "<r a=\"caf\u{e9}\">\u{4f60}\u{597d}</r>";
        let root =
            parse_xml(&ParseCtx::default(), &utf16(text, Endian::Little, false), Role::Probe)
                .unwrap();
        let r = &root.child_elems().next().unwrap();
        assert_eq!(r.attr_any("a"), Some("caf\u{e9}"));
        assert_eq!(r.text(), "\u{4f60}\u{597d}");
    }

    fn reported(xml: &[u8], role: Role) -> Vec<(DiagnosticCode, LossClass, Region, u64)> {
        let parse_ctx = ParseCtx::default();
        parse_xml(&parse_ctx, xml, role).unwrap();
        diagnostic_rows(&parse_ctx.into_diagnostics())
    }

    #[derive(Clone, Copy)]
    enum Endian {
        Little,
        Big,
    }

    fn utf16(text: &str, endian: Endian, unpaired: bool) -> Vec<u8> {
        let mut units: Vec<u16> = text.encode_utf16().collect();
        if unpaired {
            units.splice(4..4, [0xD800, 0x41, 0xDC00]);
        }
        let mut bytes = match endian {
            Endian::Little => vec![0xFF, 0xFE],
            Endian::Big => vec![0xFE, 0xFF],
        };
        for unit in units {
            bytes.extend(match endian {
                Endian::Little => unit.to_le_bytes(),
                Endian::Big => unit.to_be_bytes(),
            });
        }
        bytes
    }

    fn malformed(xml: &[u8]) -> Option<&'static str> {
        match parse(&ParseCtx::default(), xml, Dialect::Xml, &mut WholeTree) {
            Err(ConvertError::Malformed { part: None, reason }) => Some(reason),
            Err(err) => panic!("{err:?}"),
            Ok(_) => None,
        }
    }

    #[test]
    fn bytes_that_do_not_decode_or_an_unknown_label_are_malformed() {
        const UNDECODABLE: Option<&str> = Some("bytes that do not decode");
        for bad in [
            utf16("<r>text</r>", Endian::Little, true),
            utf16("<r>text</r>", Endian::Big, true),
            b"<?xml version=\"1.0\" encoding=\"ISO-8859-3\"?><r>\xA5</r>".to_vec(),
        ] {
            assert_eq!(malformed(&bad), UNDECODABLE, "{bad:?}");
        }
        let bogus = b"<?xml version='1.0' encoding='x-bogus'?><r>t</r>";
        assert_eq!(malformed(bogus), Some("unknown encoding"));
        for good in [
            utf16("<r>text</r>", Endian::Little, false),
            utf16("<r>text</r>", Endian::Big, false),
            b"<?xml version=\"1.0\" encoding=\"ISO-8859-3\"?><r>\xA4</r>".to_vec(),
            b"<?xml version=\"1.0\" encoding=\"windows-1252\"?><r>\xE9</r>".to_vec(),
            b"<?xml version='1.0' encoding='utf8'?><r>t</r>".to_vec(),
            b"<?pi encoding='x-bogus'?><r>t</r>".to_vec(),
            "<r>\u{FFFD}&#xFFFD;</r>".as_bytes().to_vec(),
        ] {
            assert_eq!(malformed(&good), None, "{good:?}");
            assert_eq!(reported(&good, Role::Content(Region::Note)), []);
        }
    }

    #[test]
    fn a_namespace_declared_with_references_is_named_by_their_read_value() {
        let xml = b"<p:a xmlns:p='http://purl.oclc.org/ooxml/&#x77;ordprocessingml/main' \
                    xmlns:q='urn:&amp;&bogus;' q:x='1'/>";
        let parsed = parse(&ParseCtx::default(), xml, Dialect::Xml, &mut WholeTree).unwrap();
        let a = parsed.root.child_elems().next().unwrap();
        let main = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
        assert_eq!(a.ns.as_deref(), Some(main));
        assert_eq!(a.attrs[0].ns.as_deref(), Some("urn:&&bogus;"));
        assert!(parsed.marks.unresolved);
    }

    #[test]
    fn a_head_cut_inside_a_character_after_its_root_tag_still_names_its_namespace() {
        let mut head = utf16("<w:document xmlns:w=\"urn:w\"><w:t>\u{1F600}", Endian::Little, false);
        head.truncate(head.len() - 2);
        assert_eq!(malformed(&head), Some("bytes that do not decode"));
        assert_eq!(root_namespace(&head).as_deref(), Some("urn:w"));
        let head = b"<?xml version='1.0' encoding='x-bogus'?><a xmlns='urn:a'/>";
        assert_eq!(root_namespace(head).as_deref(), Some("urn:a"));
    }

    #[test]
    fn markup_with_a_mismatched_element_or_an_unbound_or_repeated_name_is_malformed() {
        const MISMATCHED: Option<&str> = Some("mismatched end tag");
        const UNCLOSED: Option<&str> = Some("unclosed element");
        const ATTRIBUTE: Option<&str> = Some("ill-formed attribute");
        const REPEATED: Option<&str> = Some("repeated attribute");
        const UNBOUND: Option<&str> = Some("unbound prefix");
        let distinct: String = (0..40).map(|i| format!(" a{i}='{i}'")).collect();
        for (inner, reason) in [
            ("<a><b></a>".to_owned(), MISMATCHED),
            ("</a>".into(), MISMATCHED),
            ("<w:t>a</x:t>".into(), MISMATCHED),
            ("<t>a</w:t>".into(), MISMATCHED),
            ("<w:t>a</t>".into(), MISMATCHED),
            ("<a>".into(), UNCLOSED),
            ("<a><b>text".into(), UNCLOSED),
            ("<a x='1' x='2'/>".into(), ATTRIBUTE),
            ("<a w:x='1' w:x='2'/>".into(), ATTRIBUTE),
            ("<a w:x='1' x:x='2'/>".into(), REPEATED),
            (format!("<a{distinct} w:x='1' x:x='2'/>"), REPEATED),
            ("<p:a/>".into(), UNBOUND),
            ("<a p:x='1'/>".into(), UNBOUND),
            ("<w:a><p:b/></w:a>".into(), UNBOUND),
            ("<a xmlns:p='urn:w' xmlns:q='urn:&#x77;' p:x='1' q:x='2'/>".into(), REPEATED),
            ("<a xmlns:p='urn:&amp;' xmlns:q='urn:&#38;' p:x='1' q:x='2'/>".into(), REPEATED),
        ] {
            let end = if reason == UNCLOSED { "" } else { "</r>" };
            let xml = format!("<r xmlns:w=\"urn:w\" xmlns:x=\"urn:w\">{inner}{end}");
            assert_eq!(malformed(xml.as_bytes()), reason, "{inner}");
        }
        for inner in [
            "<w:t>a</w:t>",
            "<w:t>a</w:t >",
            "<t>a</t>",
            "<a x='1' y='2'>t</a>",
            "<a w:x='1' x='2'/>",
            "<a w:x='1' p:x='2' xmlns:p='urn:p'/>",
            "<a xml:lang='tr' xml:space='preserve'/>",
            "<p:a xmlns:p='urn:p'/>",
            "<a xmlns='urn:d'><b xmlns=''/></a>",
            "<a xmlns:s='http://purl.oclc.org/ooxml/wordprocessingml/main' \
             xmlns:t='http://schemas.openxmlformats.org/wordprocessingml/2006/main' \
             s:x='1' t:x='2'/>",
        ] {
            let xml = format!("<r xmlns:w=\"urn:w\" xmlns:x=\"urn:w\">{inner}</r>");
            assert_eq!(malformed(xml.as_bytes()), None, "{inner}");
        }
    }

    #[test]
    fn invalid_utf8_in_a_utf8_part_is_malformed() {
        for xml in [
            &b"<r>caf\xE9</r>"[..],
            b"<r a=\"caf\xE9\"/>",
            b"<r><![CDATA[caf\xE9]]></r>",
            b"<r\xE9/>",
        ] {
            let err = parse_xml(&ParseCtx::default(), xml, Role::Probe).unwrap_err();
            assert_matches!(err, ConvertError::Malformed { .. });
        }
    }

    #[test]
    fn short_legacy_encoded_part_is_transcoded() {
        let root = parse_xml(
            &ParseCtx::default(),
            b"<?xml version=\"1.0\" encoding=\"windows-1252\"?><r a=\"\xE9\">caf\xE9</r>",
            Role::Probe,
        )
        .unwrap();
        let r = root.child_elems().next().unwrap();
        assert_eq!(r.attr_any("a"), Some("\u{e9}"));
        assert_eq!(r.text(), "caf\u{e9}");
    }

    #[test]
    fn text_cdata_and_references_keep_their_content() {
        let root = parse_xml(&ParseCtx::default(),
            b"<r xmlns=\"urn:d\" xmlns:q=\"urn:q\" a=\"x&amp;\ty\" b=\"&bogus;\">a\r\nb<![CDATA[<c>\r\n]]>&#x41;&lt;&bogus;</r>", Role::Probe,
        )
        .unwrap();
        let r = root.child_elems().next().unwrap();
        assert_eq!(r.attr_any("a"), Some("x& y"));
        assert_eq!(r.attr_any("b"), Some("&bogus;"));
        assert_eq!(r.attrs.len(), 2);
        assert_eq!(r.text(), "a\r\nb<c>\r\nA<&bogus;");
    }

    #[test]
    fn namespace_binding_overflow_is_malformed() {
        let decls = |from: usize, to: usize| {
            (from..to).map(|i| format!(" xmlns:p{i}=\"urn:{i}\"")).collect::<String>()
        };
        let xml = format!("<a{}><b{}/></a>", decls(0, 65), decls(65, 130));
        let err = parse_xml(&ParseCtx::default(), xml.as_bytes(), Role::Probe).unwrap_err();
        assert_matches!(err, ConvertError::Malformed { .. });
    }

    #[test]
    fn depth_limit_is_hard_error() {
        let xml = "<d>".repeat(default_depth() + 2);
        let err = parse_xml(&ParseCtx::default(), xml.as_bytes(), Role::Probe).unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::NestingDepth, .. });
    }

    #[test]
    fn deep_dom_traversal_is_iterative() {
        let depth = default_depth() - 1;
        let xml = format!("{}leaf{}", "<d>".repeat(depth), "</d>".repeat(depth));
        let root = parse_xml(&ParseCtx::default(), xml.as_bytes(), Role::Probe).unwrap();
        assert_eq!(root.text(), "leaf");
        assert_eq!(root.descendants("", "never").count(), 0);
    }

    #[test]
    fn outermost_yields_the_unnested_matches_in_document_order() {
        let ceiling = usize::try_from(Limits::MAX_NESTING_CEILING).unwrap();
        let limits = Limits { max_nesting_depth: Limits::MAX_NESTING_CEILING, ..Limits::default() };
        let chain = format!("{}{}", "<m>".repeat(ceiling - 3), "</m>".repeat(ceiling - 3));
        let xml = format!(
            "<r xmlns='urn:t'><o><m n='1'>{chain}</m></o><x><y><m n='2'/></y></x><m n='3'><x><m/></x></m></r>"
        );
        let root = parse_xml(&ParseCtx::new(&limits), xml.as_bytes(), Role::Probe).unwrap();
        let found: Vec<_> = root.outermost("urn:t", "m").map(|m| m.attr_unqualified("n")).collect();
        assert_eq!(found, [Some("1"), Some("2"), Some("3")]);
        let r = root.find("urn:t", "r").unwrap();
        let in_r: Vec<_> = r.outermost("urn:t", "m").map(|m| m.attr_unqualified("n")).collect();
        assert_eq!(in_r, found);
        assert_eq!(root.outermost("urn:t", "never").count(), 0);
    }

    type Reported = Vec<(DiagnosticCode, Region)>;

    fn parse_reported(xml: &str, dialect: Dialect) -> Result<(Element, Reported), ConvertError> {
        let parse_ctx = ParseCtx::default();
        let parsed = parse(&parse_ctx, xml.as_bytes(), dialect, &mut WholeTree)?;
        parsed.marks.report(&parse_ctx, Role::Content(Region::Note));
        let reported = parse_ctx.into_diagnostics().iter().map(|d| (d.code, d.region)).collect();
        Ok((parsed.root, reported))
    }

    fn parsed(xml: &str, dialect: Dialect) -> Result<(String, Reported), ConvertError> {
        let (root, reported) = parse_reported(xml, dialect)?;
        Ok((root.text(), reported))
    }

    fn attribute(xml: &str, dialect: Dialect) -> (String, Reported) {
        let (root, reported) = parse_reported(xml, dialect).unwrap();
        (root.child_elems().next().unwrap().attr_any("a").unwrap().to_owned(), reported)
    }

    const UNRESOLVED: [(DiagnosticCode, Region); 1] =
        [(DiagnosticCode::UnresolvedEntity, Region::Note)];
    const DIALECTS: [Dialect; 2] = [Dialect::Xml, Dialect::Xhtml];

    #[test]
    fn a_bare_less_than_or_a_misplaced_declaration_is_malformed() {
        const BARE: &str = "bare `<` in markup";
        const UNPARSEABLE: &str = "unparseable xml";
        const DECLARATION: &str = "declaration inside the content";
        const MISMATCHED: &str = "mismatched end tag";
        for (xml, reason) in [
            ("<r>Pump A < pump B</r>", BARE),
            ("<r>< First paragraph.</r>", BARE),
            ("<r>Pump A <pump B</r>", BARE),
            ("<r>< >later</r>", BARE),
            ("<r>< /></r>", BARE),
            ("<r a=\"bare < delimiter\"/>", BARE),
            ("<r>A <? B</r>", UNPARSEABLE),
            ("<r>A <? B ?> C</r>", "processing instruction without a target"),
            ("<r>A <! B</r>", UNPARSEABLE),
            ("<r>A <!- B -></r>", UNPARSEABLE),
            ("<r>A <!DOCTYPE B> C</r>", DECLARATION),
            ("<r>A <?xml version=\"1.0\"?> C</r>", DECLARATION),
            ("<r>t</r><!DOCTYPE r>", DECLARATION),
            ("<r/><?xml version=\"1.0\"?>", DECLARATION),
            ("<r>a </ b</r>c", MISMATCHED),
            ("<r><s>a </s b</r>c", MISMATCHED),
            ("<r>a </>b</r>", MISMATCHED),
            ("<r>a </ >b</r>", MISMATCHED),
            ("<r><s>a </ >b</r>", MISMATCHED),
        ] {
            for dialect in DIALECTS {
                let err = parsed(xml, dialect).unwrap_err();
                assert_matches!(
                    err,
                    ConvertError::Malformed { part: None, reason: r } if r == reason,
                    "{xml}"
                );
            }
        }
    }

    #[test]
    fn escapes_cdata_comments_and_instructions_keep_every_word() {
        let xml = "<?xml version=\"1.0\"?><!DOCTYPE r><?mso-application progid=\"x\"?>\
            <r>before &lt; middle <![CDATA[< after]]><!-- c --><?pi data?> end</r>";
        for dialect in DIALECTS {
            assert_eq!(
                parsed(xml, dialect).unwrap(),
                ("before < middle < after end".into(), vec![])
            );
        }
    }

    #[test]
    fn an_ampersand_that_begins_no_reference_stays_as_written() {
        for xml in [
            "<r>Tom & Jerry</r>",
            "<r>a & b; c</r>",
            "<r>a &</r>",
            "<r>x &&amp; y</r>",
            "<r>p &<b/>q</r>",
            "<r>a &1b; c</r>",
            "<r>&-x;</r>",
            "<r>&.x;</r>",
            "<r>a &\u{d7}; b</r>",
            "<r>&\u{f7};</r>",
            "<r>&\u{2014}x;</r>",
            "<r>&a\u{d7}b;</r>",
            "<r>&\u{b7}b;</r>",
        ] {
            let expected = xml
                .replace("<r>", "")
                .replace("</r>", "")
                .replace("<b/>", "")
                .replace("&amp;", "&");
            for dialect in DIALECTS {
                assert_eq!(parsed(xml, dialect).unwrap(), (expected.clone(), vec![]), "{xml}");
            }
        }
    }

    #[test]
    fn named_references_resolve_by_dialect_and_the_rest_stay_literal() {
        let xml = "<r>&amp;&lt;&#65;&#x42;&quot; &nbsp;&eacute;&AMP;</r>";
        assert_eq!(parsed(xml, Dialect::Xhtml).unwrap(), ("&<AB\" \u{a0}\u{e9}&".into(), vec![]));
        assert_eq!(
            parsed(xml, Dialect::Xml).unwrap(),
            ("&<AB\" &nbsp;&eacute;&AMP;".into(), UNRESOLVED.to_vec())
        );
        for truncated in ["&Ascr;", "&Aopf;", "&zfr;"] {
            let xml = format!("<r>{truncated}</r>");
            assert_eq!(
                parsed(&xml, Dialect::Xhtml).unwrap(),
                (truncated.into(), UNRESOLVED.to_vec())
            );
            let xml = format!("<r a=\"{truncated}\"/>");
            assert_eq!(attribute(&xml, Dialect::Xhtml), (truncated.into(), UNRESOLVED.to_vec()));
        }
        for unknown in ["&bogus;", "&#X41;", "&#0;", "&#+65;", "&#xD800;", "&#;", "&#x110000;"] {
            let xml = format!("<r>{unknown}</r>");
            for dialect in DIALECTS {
                assert_eq!(parsed(&xml, dialect).unwrap(), (unknown.into(), UNRESOLVED.to_vec()));
            }
        }
    }

    #[test]
    fn a_reference_name_is_an_xml_name_whatever_its_script() {
        for name in ["&\u{e9}t\u{e9};", "&a\u{b7}b;", "&\u{20ac}x;", "&\u{3b1}\u{301};"] {
            for dialect in DIALECTS {
                let xml = format!("<r>{name}</r>");
                assert_eq!(parsed(&xml, dialect).unwrap(), (name.into(), UNRESOLVED.to_vec()));
                let xml = format!("<r a=\"{name}\"/>");
                assert_eq!(attribute(&xml, dialect), (name.into(), UNRESOLVED.to_vec()), "{name}");
            }
        }
    }

    #[test]
    fn an_unresolved_reference_reports_once_by_role() {
        let xml = b"<r a=\"&bogus;\">&nbsp;&nbsp;</r>";
        for (role, expected) in [
            (
                Role::Content(Region::Footer),
                vec![(DiagnosticCode::UnresolvedEntity, LossClass::Lost, Region::Footer, 1)],
            ),
            (
                Role::Reported(Region::Body),
                vec![(DiagnosticCode::UnresolvedEntity, LossClass::Lost, Region::Body, 1)],
            ),
            (Role::Formatting, vec![]),
            (Role::Probe, vec![]),
        ] {
            assert_eq!(reported(xml, role), expected, "{role:?}");
        }
    }

    #[test]
    fn attributes_read_references_as_text_does() {
        for dialect in DIALECTS {
            for (value, expected) in [
                ("Tom & Jerry", "Tom & Jerry"),
                ("a & b; c", "a & b; c"),
                ("x&amp;\ty&#10;&#x41;&lt;", "x& y\nA<"),
                ("&&amp;", "&&"),
                ("end &", "end &"),
                ("a &1b; c", "a &1b; c"),
                ("&-x;", "&-x;"),
                ("a &\u{d7}; b", "a &\u{d7}; b"),
                ("&\u{2014}x;", "&\u{2014}x;"),
                ("&a\u{d7}b;", "&a\u{d7}b;"),
                ("&\u{b7}b;", "&\u{b7}b;"),
            ] {
                let xml = format!("<r a=\"{value}\"/>");
                assert_eq!(attribute(&xml, dialect), (expected.into(), vec![]), "{value}");
            }
        }
        let xml = "<r a=\"&nbsp;&AMP;&eacute;\"/>";
        assert_eq!(attribute(xml, Dialect::Xhtml), ("\u{a0}&\u{e9}".into(), vec![]));
        assert_eq!(
            attribute(xml, Dialect::Xml),
            ("&nbsp;&AMP;&eacute;".into(), UNRESOLVED.to_vec())
        );
        for unknown in ["&bogus;", "&#X41;", "&#xD800;"] {
            let xml = format!("<r a=\"{unknown} &amp;\"/>");
            for dialect in DIALECTS {
                assert_eq!(attribute(&xml, dialect), (format!("{unknown} &"), UNRESOLVED.to_vec()));
            }
        }
    }

    #[test]
    fn an_internal_subset_is_malformed_in_xml_and_ignored_in_xhtml() {
        let subset = "<!DOCTYPE r [<!ENTITY e \"expanded\">]><r>&e;&eacute;</r>";
        let err = parsed(subset, Dialect::Xml).unwrap_err();
        assert_matches!(
            err,
            ConvertError::Malformed { reason: "document type with an internal subset", .. }
        );
        assert_eq!(
            parsed(subset, Dialect::Xhtml).unwrap(),
            ("&e;\u{e9}".into(), UNRESOLVED.to_vec())
        );
        for doctype in
            ["<!DOCTYPE r>", "<!DOCTYPE r SYSTEM \"a[b\">", "<!DOCTYPE r PUBLIC 'p[' 's'>"]
        {
            let xml = format!("{doctype}<r>t</r>");
            for dialect in DIALECTS {
                assert_eq!(parsed(&xml, dialect).unwrap(), ("t".into(), vec![]), "{doctype}");
            }
        }
    }

    #[test]
    fn marks_are_returned_not_reported() {
        let marks = |xml: &[u8]| {
            let parse_ctx = ParseCtx::default();
            let parsed = parse(&parse_ctx, xml, Dialect::Xml, &mut WholeTree).unwrap();
            assert!(parse_ctx.diagnostics().is_empty());
            let Marks { unresolved } = parsed.marks;
            unresolved
        };
        assert!(marks(b"<a>&bogus;</a>"));
        assert!(!marks(b"<a>text</a>"));
    }

    mod round_trip {
        use super::*;
        use proptest::prelude::*;
        use std::fmt;

        const PREFIXES: [&str; 3] = ["p", "q", "r"];
        const NAMESPACES: [(&str, &str); 3] = [
            ("urn:a", "urn:a"),
            ("urn:b", "urn:b"),
            ("http://purl.oclc.org/ooxml/wordprocessingml/main", ns::W),
        ];
        const LOCALS: [&str; 5] = ["e", "item", "ünï", "日本", "a.b-c"];
        const PREDEFINED: [(char, &str); 5] =
            [('&', "&amp;"), ('<', "&lt;"), ('>', "&gt;"), ('"', "&quot;"), ('\'', "&apos;")];
        const SCOPE: [usize; 3] = [0, 1, 2];
        const REBINDING_LEVELS: usize = 48;

        #[derive(Debug, Clone)]
        enum Piece {
            Text(String),
            Predefined(usize),
            Decimal(char),
            Hex(char),
            CData(String),
            Comment,
            Instruction,
        }

        impl Piece {
            fn shown(&self, in_attribute: bool) -> String {
                match self {
                    Self::Text(text) if in_attribute => text.replace(['\t', '\n'], " "),
                    Self::Text(text) | Self::CData(text) => text.clone(),
                    Self::Predefined(at) => PREDEFINED[*at].0.to_string(),
                    Self::Decimal(c) | Self::Hex(c) => c.to_string(),
                    Self::Comment | Self::Instruction => String::new(),
                }
            }

            fn written(&self, in_attribute: bool) -> String {
                match self {
                    Self::Text(text) => escaped(text, in_attribute),
                    Self::Predefined(at) => PREDEFINED[*at].1.to_owned(),
                    Self::Decimal(c) => format!("&#{};", u32::from(*c)),
                    Self::Hex(c) => format!("&#x{:X};", u32::from(*c)),
                    Self::CData(text) => format!("<![CDATA[{text}]]>"),
                    Self::Comment => "<!-- a - b -->".to_owned(),
                    Self::Instruction => "<?target data?>".to_owned(),
                }
            }
        }

        fn escaped(text: &str, in_attribute: bool) -> String {
            let mut out = String::new();
            for c in text.chars() {
                match c {
                    '&' => out.push_str("&amp;"),
                    '<' => out.push_str("&lt;"),
                    '>' => out.push_str("&gt;"),
                    '"' if in_attribute => out.push_str("&quot;"),
                    c => out.push(c),
                }
            }
            out
        }

        #[derive(Debug, Clone)]
        enum Name {
            Unqualified,
            Prefixed(usize),
            Default(usize),
        }

        #[derive(Debug)]
        struct Attribute {
            prefix: Option<usize>,
            value: Vec<Piece>,
        }

        #[derive(Debug)]
        enum Content {
            Piece(Piece),
            Child(Spec),
        }

        #[derive(Debug)]
        struct Spec {
            rebinding: Option<(usize, usize)>,
            local: usize,
            name: Name,
            attributes: Vec<Attribute>,
            self_closing: bool,
            content: Vec<Content>,
        }

        impl Spec {
            fn depth(&self) -> usize {
                let deepest = self.content.iter().filter_map(|content| match content {
                    Content::Child(child) => Some(child.depth()),
                    Content::Piece(_) => None,
                });
                1 + deepest.max().unwrap_or(0)
            }

            fn scope(&self, mut scope: [usize; 3]) -> [usize; 3] {
                if let Some((prefix, namespace)) = self.rebinding {
                    scope[prefix] = namespace;
                }
                scope
            }

            fn write(&self, default_declared: bool, out: &mut String) {
                let local = LOCALS[self.local];
                let name = match self.name {
                    Name::Prefixed(at) => format!("{}:{local}", PREFIXES[at]),
                    Name::Unqualified | Name::Default(_) => local.to_owned(),
                };
                out.push_str(&format!("<{name}"));
                if let Some((prefix, namespace)) = self.rebinding {
                    out.push_str(&format!(
                        " xmlns:{}=\"{}\"",
                        PREFIXES[prefix], NAMESPACES[namespace].0
                    ));
                }
                match self.name {
                    Name::Default(at) => out.push_str(&format!(" xmlns=\"{}\"", NAMESPACES[at].0)),
                    Name::Unqualified if default_declared => out.push_str(" xmlns=\"\""),
                    Name::Unqualified | Name::Prefixed(_) => {}
                }
                for (index, attribute) in self.attributes.iter().enumerate() {
                    let prefix =
                        attribute.prefix.map_or(String::new(), |at| format!("{}:", PREFIXES[at]));
                    let value: String =
                        attribute.value.iter().map(|piece| piece.written(true)).collect();
                    let local = LOCALS[index % LOCALS.len()];
                    out.push_str(&format!(" {prefix}{local}{index}=\"{value}\""));
                }
                if self.self_closing && self.content.is_empty() {
                    out.push_str("/>");
                    return;
                }
                out.push('>');
                let default_declared = match self.name {
                    Name::Default(_) => true,
                    Name::Unqualified => false,
                    Name::Prefixed(_) => default_declared,
                };
                for content in &self.content {
                    match content {
                        Content::Piece(piece) => out.push_str(&piece.written(false)),
                        Content::Child(child) => child.write(default_declared, out),
                    }
                }
                out.push_str(&format!("</{name}>"));
            }

            fn expected(&self, scope: [usize; 3]) -> Element {
                let scope = self.scope(scope);
                let namespace = |at: usize| Rc::<str>::from(NAMESPACES[at].1);
                let ns = match self.name {
                    Name::Unqualified => None,
                    Name::Prefixed(at) => Some(namespace(scope[at])),
                    Name::Default(at) => Some(namespace(at)),
                };
                let attrs = self
                    .attributes
                    .iter()
                    .enumerate()
                    .map(|(index, attribute)| Attr {
                        ns: attribute.prefix.map(|at| namespace(scope[at])),
                        local: format!("{}{index}", LOCALS[index % LOCALS.len()]),
                        value: attribute.value.iter().map(|piece| piece.shown(true)).collect(),
                    })
                    .collect();
                let mut children: Vec<Node> = Vec::new();
                for content in &self.content {
                    match content {
                        Content::Child(child) => children.push(Node::Elem(child.expected(scope))),
                        Content::Piece(piece) => merge_text(&mut children, piece.shown(false)),
                    }
                }
                Element {
                    ns,
                    local: LOCALS[self.local].to_owned(),
                    attrs,
                    children,
                    unresolved_text: false,
                }
            }
        }

        fn merge_text(children: &mut Vec<Node>, text: String) {
            match children.last_mut() {
                _ if text.is_empty() => {}
                Some(Node::Text(previous)) => previous.push_str(&text),
                _ => children.push(Node::Text(text)),
            }
        }

        fn character() -> impl Strategy<Value = char> {
            prop_oneof![
                8 => prop::char::range(' ', '~'),
                1 => prop::sample::select(vec!['\t', '\n']),
                2 => prop::char::range('\u{A0}', '\u{7FF}'),
                1 => prop::char::range('\u{E000}', '\u{FFFD}'),
                1 => prop::char::range('\u{10000}', '\u{10FFFF}'),
            ]
        }

        fn text() -> impl Strategy<Value = String> {
            prop::collection::vec(character(), 1..6).prop_map(|chars| chars.into_iter().collect())
        }

        fn reference() -> impl Strategy<Value = Piece> {
            let referenced = || prop_oneof![character(), Just('\r')];
            prop_oneof![
                3 => text().prop_map(Piece::Text),
                1 => (0..PREDEFINED.len()).prop_map(Piece::Predefined),
                1 => referenced().prop_map(Piece::Decimal),
                1 => referenced().prop_map(Piece::Hex),
            ]
        }

        fn piece() -> impl Strategy<Value = Piece> {
            prop_oneof![
                6 => reference(),
                1 => prop::collection::vec(character(), 0..6)
                    .prop_map(|chars| Piece::CData(chars.into_iter().collect::<String>().replace("]]>", "]] "))),
                1 => Just(Piece::Comment),
                1 => Just(Piece::Instruction),
            ]
        }

        fn attribute() -> impl Strategy<Value = Attribute> {
            (prop::option::of(0..PREFIXES.len()), prop::collection::vec(reference(), 0..4))
                .prop_map(|(prefix, value)| Attribute { prefix, value })
        }

        fn element(content: impl Strategy<Value = Vec<Content>>) -> impl Strategy<Value = Spec> {
            let name = prop_oneof![
                Just(Name::Unqualified),
                (0..PREFIXES.len()).prop_map(Name::Prefixed),
                (0..NAMESPACES.len()).prop_map(Name::Default),
            ];
            (
                prop::option::of((0..PREFIXES.len(), 0..NAMESPACES.len())),
                0..LOCALS.len(),
                name,
                prop::collection::vec(attribute(), 0..4),
                any::<bool>(),
                content,
            )
                .prop_map(
                    |(rebinding, local, name, attributes, self_closing, content)| Spec {
                        rebinding,
                        local,
                        name,
                        attributes,
                        self_closing,
                        content,
                    },
                )
        }

        fn pieces() -> impl Strategy<Value = Vec<Content>> {
            prop::collection::vec(piece().prop_map(Content::Piece), 0..3)
        }

        fn tree() -> impl Strategy<Value = Spec> {
            element(pieces()).prop_recursive(4, 24, 4, |inner| {
                let content =
                    prop_oneof![piece().prop_map(Content::Piece), inner.prop_map(Content::Child)];
                element(prop::collection::vec(content, 0..5))
            })
        }

        /// A chain of nested elements, deep enough to cross the depth cap, whose
        /// namespace declarations stay inside the resolver's binding budget.
        fn chain() -> impl Strategy<Value = Spec> {
            let cap = usize::try_from(Limits::default().max_nesting_depth).unwrap();
            prop_oneof![1usize..=6, cap - 3..=cap]
                .prop_flat_map(|depth| prop::collection::vec(element(pieces()), depth))
                .prop_map(|mut levels| {
                    for level in levels.iter_mut().skip(REBINDING_LEVELS) {
                        level.rebinding = None;
                        if matches!(level.name, Name::Default(_)) {
                            level.name = Name::Unqualified;
                        }
                    }
                    levels
                        .into_iter()
                        .rev()
                        .reduce(|inner, mut outer| {
                            let at = outer.content.len() / 2;
                            outer.content.insert(at, Content::Child(inner));
                            outer
                        })
                        .unwrap()
                })
        }

        struct Case {
            spec: Spec,
            xml: String,
        }

        impl fmt::Debug for Case {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{} bytes of {:.400}", self.xml.len(), self.xml)
            }
        }

        fn case() -> impl Strategy<Value = Case> {
            let prolog = prop::sample::select(vec![
                "",
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>",
                "<!-- a --><?target data?>",
            ]);
            (prop_oneof![tree(), chain()], prolog, any::<bool>()).prop_map(
                |(spec, prolog, epilog)| {
                    let declarations: String = PREFIXES
                        .iter()
                        .zip(SCOPE)
                        .map(|(prefix, at)| format!(" xmlns:{prefix}=\"{}\"", NAMESPACES[at].0))
                        .collect();
                    let mut xml = format!("{prolog}<root{declarations}>");
                    spec.write(false, &mut xml);
                    xml.push_str("</root>");
                    if epilog {
                        xml.push_str("<!-- z -->");
                    }
                    Case { spec, xml }
                },
            )
        }

        fn first_difference(parsed: &Element, expected: &Element, depth: usize) -> Option<String> {
            let head = |element: &Element| (element.ns.clone(), element.local.clone());
            if head(parsed) != head(expected) || parsed.attrs != expected.attrs {
                return Some(format!(
                    "depth {depth}: {:?} {:?} against {:?} {:?}",
                    head(parsed),
                    parsed.attrs,
                    head(expected),
                    expected.attrs
                ));
            }
            for (a, b) in parsed.children.iter().zip(&expected.children) {
                let difference = match (a, b) {
                    (Node::Elem(a), Node::Elem(b)) => first_difference(a, b, depth + 1),
                    (a, b) if a != b => Some(format!("depth {}: {a:?} against {b:?}", depth + 1)),
                    _ => None,
                };
                if difference.is_some() {
                    return difference;
                }
            }
            (parsed.children.len() != expected.children.len()).then(|| {
                let (held, wanted) = (parsed.children.len(), expected.children.len());
                format!("depth {depth}: {held} children against {wanted}")
            })
        }

        proptest! {
            #[test]
            fn a_generated_tree_parses_to_itself_with_adjacent_text_merged_or_fails_past_the_depth_cap(
                case in case(),
            ) {
                let cap = Limits::default().max_nesting_depth;
                let parsed =
                    parse(&ParseCtx::default(), case.xml.as_bytes(), Dialect::Xml, &mut WholeTree);
                if 1 + case.spec.depth() as u64 > cap {
                    prop_assert!(
                        matches!(
                            parsed,
                            Err(ConvertError::ResourceLimit { limit: Limit::NestingDepth, max }) if max == cap
                        ),
                        "{:?}",
                        parsed.map(|_| "parsed")
                    );
                } else {
                    let Parsed { root: document, marks, .. } = parsed.unwrap();
                    prop_assert!(!marks.unresolved, "{marks:?}");
                    let root = document.child_elems().next().unwrap();
                    let expected = Element {
                        unresolved_text: false,
                        ns: None,
                        local: "root".into(),
                        attrs: Vec::new(),
                        children: vec![Node::Elem(case.spec.expected(SCOPE))],
                    };
                    prop_assert!(root == &expected, "{:?}", first_difference(root, &expected, 1));
                }
            }
        }
    }
}

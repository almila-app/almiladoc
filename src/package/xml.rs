//! Namespace-aware DOM built on quick-xml.
//!
//! Parses from bytes so BOMs and encoding declarations are honored. Element
//! and attribute names carry their resolved namespace URI (interned). Depth
//! and node caps are enforced during parsing. Unclosed or mismatched tags and
//! ill-formed attributes are repaired and the repair returned to the reader;
//! undecodable bytes and references the part's [`Dialect`] does not resolve
//! report by the reading call's role; markup a repair would misread fails.

use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use crate::limits::Limit;
use crate::package::archive::{Role, report};
use quick_xml::events::attributes::Attribute;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::{QName, ResolveResult};
use quick_xml::{NsReader, XmlVersion};
use std::borrow::Cow;
use std::collections::HashMap;
use std::rc::Rc;

/// Well-known namespace URIs.
pub mod ns {
    pub const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
    pub const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    pub const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
    pub const PIC: &str = "http://schemas.openxmlformats.org/drawingml/2006/picture";
    pub const WP: &str = "http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing";
    pub const MC: &str = "http://schemas.openxmlformats.org/markup-compatibility/2006";
    pub const CHART: &str = "http://schemas.openxmlformats.org/drawingml/2006/chart";
    pub const DGM: &str = "http://schemas.openxmlformats.org/drawingml/2006/diagram";
    pub const P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
    pub const SML: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
    pub const XDR: &str = "http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing";
    pub const PKG_RELS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

    pub const OFFICE: &str = "urn:oasis:names:tc:opendocument:xmlns:office:1.0";
    pub const TEXT: &str = "urn:oasis:names:tc:opendocument:xmlns:text:1.0";
    pub const TABLE: &str = "urn:oasis:names:tc:opendocument:xmlns:table:1.0";
    pub const DRAW: &str = "urn:oasis:names:tc:opendocument:xmlns:drawing:1.0";
    pub const STYLE: &str = "urn:oasis:names:tc:opendocument:xmlns:style:1.0";
    pub const FO: &str = "urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0";
    pub const PRESENTATION: &str = "urn:oasis:names:tc:opendocument:xmlns:presentation:1.0";
    pub const MANIFEST: &str = "urn:oasis:names:tc:opendocument:xmlns:manifest:1.0";
    pub const XLINK: &str = "http://www.w3.org/1999/xlink";
    pub const XML: &str = "http://www.w3.org/XML/1998/namespace";
    pub const SVG_COMPAT: &str = "urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0";
    pub const FORM: &str = "urn:oasis:names:tc:opendocument:xmlns:form:1.0";

    pub const VML: &str = "urn:schemas-microsoft-com:vml";
    pub const O_VML: &str = "urn:schemas-microsoft-com:office:office";
    pub const X_VML: &str = "urn:schemas-microsoft-com:office:excel";
    pub const WPS: &str = "http://schemas.microsoft.com/office/word/2010/wordprocessingShape";
    pub const WPG: &str = "http://schemas.microsoft.com/office/word/2010/wordprocessingGroup";
    pub const A14: &str = "http://schemas.microsoft.com/office/drawing/2010/main";

    pub const M: &str = "http://schemas.openxmlformats.org/officeDocument/2006/math";
    pub const MATHML: &str = "http://www.w3.org/1998/Math/MathML";
}

#[derive(Debug)]
pub enum Node {
    Elem(Element),
    Text(String),
}

#[derive(Debug)]
pub struct Element {
    /// Resolved namespace URI (interned); `None` for unqualified names.
    pub ns: Option<Rc<str>>,
    pub local: String,
    pub attrs: Vec<Attr>,
    pub children: Vec<Node>,
}

#[derive(Debug)]
pub struct Attr {
    pub ns: Option<Rc<str>>,
    pub local: String,
    pub value: String,
}

impl Element {
    pub fn is(&self, ns: &str, local: &str) -> bool {
        self.local == local && self.ns.as_deref() == Some(ns)
    }

    /// Same-vocabulary attribute lookup: the qualified attribute wins, and
    /// an explicitly unqualified one with the same local name is accepted
    /// as a deliberate leniency - schemas often leave their own attributes
    /// unqualified (DrawingML `gridSpan`) and producers vary. Lookups in a
    /// *different* vocabulary than the element's (`r:id`, `xml:id`) must use
    /// [`Element::attr_qualified`], where that fallback would misattribute.
    pub fn attr(&self, ns: &str, local: &str) -> Option<&str> {
        self.attr_qualified(ns, local).or_else(|| self.attr_unqualified(local))
    }

    /// Strictly namespace-qualified attribute lookup, no fallback.
    pub fn attr_qualified(&self, ns: &str, local: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|a| a.local == local && matches!(&a.ns, Some(n) if **n == *ns))
            .map(|a| a.value.as_str())
    }

    /// Explicitly unqualified attribute lookup (no namespace), no fallback.
    pub fn attr_unqualified(&self, local: &str) -> Option<&str> {
        self.attrs.iter().find(|a| a.local == local && a.ns.is_none()).map(|a| a.value.as_str())
    }

    /// Attribute by local name regardless of namespace. For elements whose
    /// vocabulary is unambiguous in context (relationships, container files).
    pub fn attr_any(&self, local: &str) -> Option<&str> {
        self.attrs.iter().find(|a| a.local == local).map(|a| a.value.as_str())
    }

    pub fn child_elems(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|n| match n {
            Node::Elem(e) => Some(e),
            Node::Text(_) => None,
        })
    }

    pub fn find(&self, ns: &str, local: &str) -> Option<&Element> {
        self.child_elems().find(|e| e.is(ns, local))
    }

    pub fn find_all<'a>(
        &'a self,
        ns: &'a str,
        local: &'a str,
    ) -> impl Iterator<Item = &'a Element> {
        self.child_elems().filter(move |e| e.is(ns, local))
    }

    /// All descendant nodes (elements and text), depth-first in document
    /// order, iteratively (deep DOMs must not overflow the call stack).
    pub fn descendant_nodes(&self) -> DescendantNodes<'_> {
        let mut stack: Vec<&Node> = self.children.iter().collect();
        stack.reverse();
        DescendantNodes { stack }
    }

    /// All descendant elements, depth-first in document order, lazily.
    pub fn descendant_elems(&self) -> impl Iterator<Item = &Element> {
        self.descendant_nodes().filter_map(|node| match node {
            Node::Elem(e) => Some(e),
            Node::Text(_) => None,
        })
    }

    /// First descendant with the given name, depth-first.
    pub fn first_descendant(&self, ns: &str, local: &str) -> Option<&Element> {
        self.descendant_elems().find(|e| e.is(ns, local))
    }

    /// All descendants with the given name, in document order, lazily.
    pub fn descendants<'a>(
        &'a self,
        ns: &'a str,
        local: &'a str,
    ) -> impl Iterator<Item = &'a Element> {
        self.descendant_elems().filter(move |e| e.is(ns, local))
    }

    /// The descendants with the given name that no other such descendant
    /// holds, in document order: a match's own subtree is never walked.
    pub fn outermost<'a>(
        &'a self,
        ns: &'a str,
        local: &'a str,
    ) -> impl Iterator<Item = &'a Element> {
        let mut stack: Vec<&Element> = self.child_elems().collect();
        stack.reverse();
        std::iter::from_fn(move || {
            while let Some(elem) = stack.pop() {
                if elem.is(ns, local) {
                    return Some(elem);
                }
                let start = stack.len();
                stack.extend(elem.child_elems());
                if let Some(children) = stack.get_mut(start..) {
                    children.reverse();
                }
            }
            None
        })
    }

    /// All descendants matching a local name regardless of namespace, in
    /// document order. For vocabularies where the namespace varies across
    /// producers (EPUB container/OPF metadata).
    pub fn descendants_any<'a>(&'a self, local: &'a str) -> impl Iterator<Item = &'a Element> {
        self.descendant_elems().filter(move |e| e.local == local)
    }

    /// Concatenated text of all descendant text nodes.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for node in self.descendant_nodes() {
            if let Node::Text(t) = node {
                out.push_str(t);
            }
        }
        out
    }
}

/// Iterative depth-first traversal over a subtree's nodes; the shared core
/// of every descendant lookup.
pub struct DescendantNodes<'a> {
    stack: Vec<&'a Node>,
}

impl<'a> Iterator for DescendantNodes<'a> {
    type Item = &'a Node;

    fn next(&mut self) -> Option<&'a Node> {
        let node = self.stack.pop()?;
        if let Node::Elem(e) = node {
            let start = self.stack.len();
            self.stack.extend(e.children.iter());
            self.stack[start..].reverse();
        }
        Some(node)
    }
}

/// Which named references a part's markup may use: declared (DTD) entities
/// never expand in either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// XML's five predefined entities alone: OOXML and OpenDocument parts.
    Xml,
    /// HTML5's named references as well: EPUB's XHTML and package documents.
    Xhtml,
}

impl Dialect {
    fn named(self, name: &str) -> Option<&'static str> {
        match self {
            Dialect::Xml => quick_xml::escape::resolve_xml_entity(name),
            // quick-xml 0.42's table cuts each five-digit `*fr`, `*scr` and `*opf`
            // code point to its first four digits, all in U+1D00..=U+1DFF, a
            // block no HTML5 reference names.
            Dialect::Xhtml => quick_xml::escape::resolve_html5_entity(name).filter(|value| {
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
    let (digits, radix) = match number.strip_prefix('x') {
        Some(hex) => (hex, 16),
        None => (number, 10),
    };
    if digits.starts_with(['+', '-']) {
        return None;
    }
    let code = u32::from_str_radix(digits, radix).ok().filter(|&code| code != 0)?;
    char::from_u32(code)
}

/// Whether `name` is an XML 1.0 (fifth edition) `Name`, so `&name;` is a
/// reference rather than an ampersand followed by text.
fn is_name(name: &str) -> bool {
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
    parse_ctx: &ParseCtx,
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
/// resolution sees one consistent encoding. Markup a repair would misread is
/// `Malformed`: a `<` that opens no well-formed tag, a declaration or
/// document type inside the content, and in [`Dialect::Xml`] a document
/// type with an internal subset. Every node `sink` takes goes to it as it
/// completes and is released, so `max_xml_nodes` bounds the nodes held at
/// once.
pub(crate) fn parse(
    parse_ctx: &ParseCtx,
    bytes: &[u8],
    dialect: Dialect,
    sink: &mut dyn NodeSink,
) -> Result<Parsed, ConvertError> {
    let limits = parse_ctx.limits();
    let (utf8, undecodable) = to_utf8(bytes);
    let mut reader = NsReader::from_reader(utf8.as_ref());
    reader.config_mut().check_end_names = false;
    reader.config_mut().allow_dangling_amp = true;

    let mut interner: HashMap<String, Rc<str>> = HashMap::new();
    let mut root =
        Element { ns: None, local: String::new(), attrs: Vec::new(), children: Vec::new() };
    let mut stack: Vec<Element> = Vec::new();
    let mut open_names: Vec<Box<str>> = Vec::new();
    let mut held_before: Vec<u64> = Vec::new();
    let mut nodes: u64 = 0;
    let mut marks = Marks { undecodable, ..Marks::default() };

    loop {
        let event = reader.read_event().map_err(|_| ConvertError::malformed("unparseable xml"))?;
        parse_ctx.charge(1)?;
        match event {
            Event::Start(e) => {
                check_start_tag(&e)?;
                if stack.len() as u64 >= limits.max_nesting_depth {
                    return Err(ConvertError::limit(Limit::NestingDepth, limits.max_nesting_depth));
                }
                held_before.push(nodes);
                bump_nodes(&mut nodes, limits.max_xml_nodes)?;
                stack.push(start_to_element(&e, &reader, &mut interner, dialect, &mut marks)?);
                open_names.push(e.name().as_ref().into());
            }
            Event::Empty(e) => {
                check_start_tag(&e)?;
                let before = nodes;
                bump_nodes(&mut nodes, limits.max_xml_nodes)?;
                let elem = start_to_element(&e, &reader, &mut interner, dialect, &mut marks)?;
                close(&mut stack, &mut root, elem, sink, &mut nodes, before)?;
            }
            Event::End(end) => {
                if end.trim().is_empty() || end.contains('<') {
                    return Err(ConvertError::malformed("bare `<` in markup"));
                }
                match stack.pop() {
                    Some(elem) => {
                        if open_names.pop().as_deref() != Some(end.name().as_ref()) {
                            marks.recovered = true;
                        }
                        let before = held_before.pop().unwrap_or_default();
                        close(&mut stack, &mut root, elem, sink, &mut nodes, before)?;
                    }
                    None => marks.recovered = true,
                }
            }
            Event::Text(e) => {
                let text = e.into_inner().into_owned();
                if !text.is_empty() {
                    push_text(&mut stack, &mut root, text, sink, &mut nodes, limits.max_xml_nodes)?;
                }
            }
            Event::GeneralRef(e) => {
                let text = match dialect.reference(&e) {
                    Reference::Char(c) => c.to_string(),
                    Reference::Named(replacement) => replacement.to_owned(),
                    Reference::Unresolved => {
                        marks.unresolved = true;
                        format!("&{};", &*e)
                    }
                    Reference::Literal => format!("&{};", &*e),
                };
                push_text(&mut stack, &mut root, text, sink, &mut nodes, limits.max_xml_nodes)?;
            }
            Event::CData(e) => {
                let text = e.into_inner().into_owned();
                push_text(&mut stack, &mut root, text, sink, &mut nodes, limits.max_xml_nodes)?;
            }
            Event::PI(e) if !is_name(e.target()) => {
                return Err(ConvertError::malformed("processing instruction without a target"));
            }
            Event::Decl(_) | Event::DocType(_) if began(&stack, &root) => {
                return Err(ConvertError::malformed("declaration inside the content"));
            }
            Event::DocType(e) if dialect == Dialect::Xml && has_internal_subset(&e) => {
                return Err(ConvertError::malformed("document type with an internal subset"));
            }
            Event::Eof => break,
            Event::PI(_) | Event::Decl(_) | Event::DocType(_) | Event::Comment(_) => {}
        }
    }
    if !stack.is_empty() {
        marks.recovered = true;
        while let Some(elem) = stack.pop() {
            let before = held_before.pop().unwrap_or_default();
            close(&mut stack, &mut root, elem, sink, &mut nodes, before)?;
        }
    }
    Ok(Parsed { root, marks, nodes })
}

/// What a parse saw that its tree cannot show.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Marks {
    /// Unclosed tags, an end tag that does not repeat its start tag's
    /// qualified name, or ill-formed attributes, were repaired.
    pub recovered: bool,
    /// A reference the dialect does not resolve stayed literal.
    pub unresolved: bool,
    /// Bytes did not decode in the part's encoding.
    pub undecodable: bool,
}

impl Marks {
    /// Report each mark by `role`, the repair last.
    pub(crate) fn report(self, parse_ctx: &ParseCtx, role: Role) {
        if self.undecodable {
            report(parse_ctx, role.undecodable());
        }
        if self.unresolved {
            report(parse_ctx, role.unresolved());
        }
        if self.recovered {
            report(parse_ctx, role.recovered());
        }
    }
}

/// The namespace of the first start tag in `head`, the leading bytes of a
/// part, normalized from Strict to Transitional; markup past that tag is
/// never read, so a head cut short anywhere after it still names it.
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
/// the end-tag arm applies the same rule.
fn check_start_tag(e: &BytesStart) -> Result<(), ConvertError> {
    if e.name().as_ref().is_empty() || e.contains('<') {
        return Err(ConvertError::malformed("bare `<` in markup"));
    }
    Ok(())
}

fn began(stack: &[Element], root: &Element) -> bool {
    !stack.is_empty() || root.child_elems().next().is_some()
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
/// and say whether any bytes did not decode.
fn to_utf8(bytes: &[u8]) -> (Cow<'_, [u8]>, bool) {
    let decoded = |encoding: &'static encoding_rs::Encoding| {
        let (s, _, had_errors) = encoding.decode(bytes);
        (Cow::Owned(s.into_owned().into_bytes()), had_errors)
    };
    match bytes {
        [0xFF, 0xFE, ..] => decoded(encoding_rs::UTF_16LE),
        [0xFE, 0xFF, ..] => decoded(encoding_rs::UTF_16BE),
        [0xEF, 0xBB, 0xBF, rest @ ..] => (Cow::Borrowed(rest), false),
        _ => {
            // Sniff a non-UTF-8 encoding declaration in the XML prolog.
            let head = &bytes[..bytes.len().min(200)];
            if let Some(label) = declared_encoding(head)
                && let Some(enc) = encoding_rs::Encoding::for_label(label.as_bytes())
                && enc != encoding_rs::UTF_8
            {
                return decoded(enc);
            }
            (Cow::Borrowed(bytes), false)
        }
    }
}

fn declared_encoding(head: &[u8]) -> Option<String> {
    let end = head.windows(2).position(|w| w == b"?>")?;
    let head = std::str::from_utf8(&head[..end]).ok()?;
    let idx = head.find("encoding")?;
    let rest = &head[idx + "encoding".len()..];
    let rest = rest.trim_start().strip_prefix('=')?.trim_start();
    let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let rest = &rest[1..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

fn bump_nodes(nodes: &mut u64, max: u64) -> Result<(), ConvertError> {
    *nodes += 1;
    if *nodes > max {
        return Err(ConvertError::limit(Limit::XmlNodes, max));
    }
    Ok(())
}

/// Attach a closed element to its parent, or hand it to `sink` and release
/// its nodes, setting `nodes` back to the count `before` it opened.
fn close(
    stack: &mut [Element],
    root: &mut Element,
    elem: Element,
    sink: &mut dyn NodeSink,
    nodes: &mut u64,
    before: u64,
) -> Result<(), ConvertError> {
    if sink.takes(stack) {
        *nodes = before;
        return sink.take(stack, elem);
    }
    match stack.last_mut() {
        Some(parent) => parent.children.push(Node::Elem(elem)),
        None => root.children.push(Node::Elem(elem)),
    }
    Ok(())
}

/// ISO/IEC 29500 *Strict* namespace and relationship-type URIs are the
/// Transitional ones re-rooted under `http://purl.oclc.org/ooxml/` with the
/// `2006` version segment dropped. Normalizing to the Transitional family at
/// parse time lets the rest of the crate match a single set of constants.
pub fn normalize_ooxml_uri(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("http://purl.oclc.org/ooxml/")?;
    let (family, tail) = rest.split_once('/')?;
    Some(format!("http://schemas.openxmlformats.org/{family}/2006/{tail}"))
}

fn intern(interner: &mut HashMap<String, Rc<str>>, uri: &str) -> Rc<str> {
    if let Some(rc) = interner.get(uri) {
        return rc.clone();
    }
    let rc: Rc<str> = Rc::from(normalize_ooxml_uri(uri).unwrap_or_else(|| uri.to_owned()));
    interner.insert(uri.to_owned(), rc.clone());
    rc
}

fn start_to_element(
    e: &BytesStart,
    reader: &NsReader<&[u8]>,
    interner: &mut HashMap<String, Rc<str>>,
    dialect: Dialect,
    marks: &mut Marks,
) -> Result<Element, ConvertError> {
    let (res, local) = reader.resolver().resolve_element(e.name());
    let ns = match res {
        ResolveResult::Bound(namespace) => Some(intern(interner, namespace.as_ref())),
        _ => None,
    };
    let local = local.as_ref().to_owned();
    let mut attrs = Vec::new();
    for attr in e.attributes() {
        let Ok(attr) = attr else {
            marks.recovered = true;
            continue;
        };
        // Namespace declarations are consumed by the reader's resolver; the
        // DOM stores only resolved names.
        if attr.key.as_namespace_binding().is_some() {
            continue;
        }
        let (res, alocal) = reader.resolver().resolve_attribute(attr.key);
        let ans = match res {
            ResolveResult::Bound(namespace) => Some(intern(interner, namespace.as_ref())),
            _ => None,
        };
        let value = attribute_value(attr, dialect, marks)?;
        attrs.push(Attr { ns: ans, local: alocal.as_ref().to_owned(), value });
    }
    // `mc:Choice/@Requires` holds namespace *prefixes* evaluated in the
    // element's lexical scope (prefixes may be rebound locally). Resolve
    // them to URIs here, where the scope is known, so consumers never need
    // a document-wide prefix map. Unresolvable prefixes stay literal and
    // match no requirement.
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
                    ResolveResult::Bound(namespace) => normalize_ooxml_uri(namespace.as_ref())
                        .unwrap_or_else(|| namespace.as_ref().to_owned()),
                    _ => prefix.to_string(),
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
    }
    Ok(Element { ns, local, attrs, children: Vec::new() })
}

/// An attribute's value normalized as XML 1.0 does, its references read as
/// text reads them: one the dialect does not know stays literal and marks
/// the part, and an ampersand that begins no reference stays as written.
fn attribute_value(
    attr: Attribute,
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
        let read = match name.map(|name| dialect.reference(name)) {
            Some(Reference::Char(_) | Reference::Named(_)) => true,
            Some(Reference::Unresolved) => {
                marks.unresolved = true;
                false
            }
            Some(Reference::Literal) | None => false,
        };
        out.push_str(if read { "&" } else { "&amp;" });
        rest = after;
    }
    out.push_str(rest);
    Cow::Owned(out)
}

fn push_text(
    stack: &mut [Element],
    root: &mut Element,
    text: String,
    sink: &dyn NodeSink,
    nodes: &mut u64,
    max: u64,
) -> Result<(), ConvertError> {
    if sink.takes(stack) {
        return Ok(());
    }
    bump_nodes(nodes, max)?;
    let target = match stack.last_mut() {
        Some(parent) => &mut parent.children,
        None => &mut root.children,
    };
    if let Some(Node::Text(prev)) = target.last_mut() {
        prev.push_str(&text);
    } else {
        target.push(Node::Text(text));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::{DiagnosticCode, LossClass, Region};
    use crate::limits::Limits;
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
    fn a_record_left_open_at_the_end_still_reaches_the_sink() {
        let mut items = Items::default();
        let parsed =
            parse(&ParseCtx::default(), b"<list><i>a</i><i>b", Dialect::Xml, &mut items).unwrap();
        assert_eq!(items.0, ["a", "b"]);
        assert!(parsed.marks.recovered);
        assert_eq!(parsed.nodes, 1);
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
        assert!(parse_xml(&parse_ctx, &nested(3), Role::Probe).is_ok());
        let err = parse_xml(&parse_ctx, &nested(4), Role::Probe).unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::NestingDepth, max: 3 });
        assert!(parse_xml(&parse_ctx, b"<a><b/>t<c/></a>", Role::Probe).is_ok());
        let err = parse_xml(&parse_ctx, b"<a><b/>t<c/><e/></a>", Role::Probe).unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::XmlNodes, max: 4 });
    }

    #[test]
    fn every_node_kind_is_charged_against_the_contexts_node_limit() {
        let limits = Limits { max_xml_nodes: 1, ..Limits::default() };
        let parse_ctx = ParseCtx::new(&limits);
        assert!(parse_xml(&parse_ctx, b"<a></a>", Role::Probe).is_ok());
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
        // The lenient lookup prefers the qualified match, while a strict
        // cross-namespace lookup never picks up the unqualified one.
        assert_eq!(kid.attr("urn:r", "id"), Some("rel"));
        assert_eq!(kid.attr_qualified("urn:other", "id"), None);
    }

    #[test]
    fn strict_ooxml_namespaces_normalize_to_transitional() {
        assert_eq!(
            normalize_ooxml_uri("http://purl.oclc.org/ooxml/wordprocessingml/main").as_deref(),
            Some(ns::W)
        );
        assert_eq!(
            normalize_ooxml_uri("http://purl.oclc.org/ooxml/officeDocument/relationships")
                .as_deref(),
            Some(ns::R)
        );
        assert_eq!(
            normalize_ooxml_uri(
                "http://purl.oclc.org/ooxml/officeDocument/relationships/officeDocument"
            )
            .as_deref(),
            Some(
                "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument"
            )
        );
        assert_eq!(normalize_ooxml_uri(ns::W), None);
        assert_eq!(normalize_ooxml_uri("urn:schemas-microsoft-com:vml"), None);

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
        // The same prefix rebound locally must resolve per element, not
        // through a document-wide first-declaration-wins map.
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
        let mut bytes = vec![0xFF, 0xFE];
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let root = parse_xml(&ParseCtx::default(), &bytes, Role::Probe).unwrap();
        let r = &root.child_elems().next().unwrap();
        assert_eq!(r.attr_any("a"), Some("caf\u{e9}"));
        assert_eq!(r.text(), "\u{4f60}\u{597d}");
    }

    fn reported(xml: &[u8], role: Role) -> Vec<(DiagnosticCode, LossClass, Region, u64)> {
        let parse_ctx = ParseCtx::default();
        parse_xml(&parse_ctx, xml, role).unwrap();
        parse_ctx.into_diagnostics().iter().map(|d| (d.code, d.class, d.region, d.count)).collect()
    }

    fn utf16(text: &str, big_endian: bool, unpaired: bool) -> Vec<u8> {
        let mut units: Vec<u16> = text.encode_utf16().collect();
        if unpaired {
            units.splice(4..4, [0xD800, 0x41, 0xDC00]);
        }
        let mut bytes = if big_endian { vec![0xFE, 0xFF] } else { vec![0xFF, 0xFE] };
        for unit in units {
            bytes.extend(if big_endian { unit.to_be_bytes() } else { unit.to_le_bytes() });
        }
        bytes
    }

    #[test]
    fn an_undecodable_part_reports_invalid_encoding_once_by_role() {
        let lost = |region| vec![(DiagnosticCode::InvalidEncoding, LossClass::Lost, region, 1)];
        let latin3 = b"<?xml version=\"1.0\" encoding=\"ISO-8859-3\"?><r>\xA5 \xA5</r>";
        for bad in
            [utf16("<r>text</r>", false, true), utf16("<r>text</r>", true, true), latin3.to_vec()]
        {
            assert_eq!(reported(&bad, Role::Content(Region::Note)), lost(Region::Note));
            assert_eq!(reported(&bad, Role::Reported(Region::Body)), lost(Region::Body));
            assert_eq!(
                reported(&bad, Role::Formatting),
                [(DiagnosticCode::InvalidStyleEncoding, LossClass::Info, Region::Container, 1)]
            );
            assert_eq!(reported(&bad, Role::Probe), []);
        }
        let latin3 = b"<?xml version=\"1.0\" encoding=\"ISO-8859-3\"?><r>\xA4</r>";
        for good in
            [utf16("<r>text</r>", false, false), utf16("<r>text</r>", true, false), latin3.to_vec()]
        {
            assert_eq!(reported(&good, Role::Content(Region::Note)), []);
        }
    }

    #[test]
    fn recovered_markup_reports_recovered_xml_once_by_role() {
        let lost = |region| vec![(DiagnosticCode::RecoveredXml, LossClass::Lost, region, 1)];
        for recovered in [
            &b"<a><b>text"[..],
            b"<a><b>text</c></a>",
            b"<a><b x=\"1\" x=\"2\"/><c/></a>",
            b"<a><b>t</c><d y=\"1\" y=\"2\"/>",
        ] {
            assert_eq!(reported(recovered, Role::Content(Region::Footer)), lost(Region::Footer));
            assert_eq!(reported(recovered, Role::Reported(Region::Note)), lost(Region::Note));
            assert_eq!(
                reported(recovered, Role::Formatting),
                [(DiagnosticCode::StylePartUnreadable, LossClass::Info, Region::Container, 1)]
            );
            assert_eq!(reported(recovered, Role::Probe), []);
        }
        assert_eq!(reported(b"<a><b x=\"1\" y=\"2\">t</b></a>", Role::Content(Region::Body)), []);
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
    fn an_end_tag_that_does_not_repeat_its_start_tags_qualified_name_is_repaired() {
        let recovered = |inner: &str| {
            let xml = format!("<r xmlns:w=\"urn:w\" xmlns:x=\"urn:w\">{inner}</r>");
            parse(&ParseCtx::default(), xml.as_bytes(), Dialect::Xml, &mut WholeTree)
                .unwrap()
                .marks
                .recovered
        };
        for inner in ["<w:t>a</x:t>", "<t>a</w:t>", "<w:t>a</t>"] {
            assert!(recovered(inner), "{inner}");
        }
        for inner in ["<w:t>a</w:t>", "<w:t>a</w:t >", "<t>a</t>"] {
            assert!(!recovered(inner), "{inner}");
        }
    }

    #[test]
    fn unclosed_elements_recovered() {
        let root = parse_xml(&ParseCtx::default(), b"<a><b>text", Role::Probe).unwrap();
        let a = root.child_elems().next().unwrap();
        assert_eq!(a.local, "a");
        assert_eq!(a.text(), "text");
    }

    #[test]
    fn depth_limit_is_hard_error() {
        let mut xml = Vec::new();
        for _ in 0..(default_depth() + 2) {
            xml.extend_from_slice(b"<d>");
        }
        let err = parse_xml(&ParseCtx::default(), &xml, Role::Probe).unwrap_err();
        assert_matches!(err, ConvertError::ResourceLimit { limit: Limit::NestingDepth, .. });
    }

    #[test]
    fn deep_dom_traversal_is_iterative() {
        // Just under the cap: descendants/text must not overflow the stack.
        let depth = default_depth() - 1;
        let mut xml = Vec::new();
        for _ in 0..depth {
            xml.extend_from_slice(b"<d>");
        }
        xml.extend_from_slice(b"leaf");
        for _ in 0..depth {
            xml.extend_from_slice(b"</d>");
        }
        let root = parse_xml(&ParseCtx::default(), &xml, Role::Probe).unwrap();
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
        assert_eq!(root.outermost("urn:t", "never").count(), 0);
    }

    fn parsed(
        xml: &str,
        dialect: Dialect,
    ) -> Result<(String, Vec<(DiagnosticCode, Region)>), ConvertError> {
        let parse_ctx = ParseCtx::default();
        let parsed = parse(&parse_ctx, xml.as_bytes(), dialect, &mut WholeTree)?;
        assert!(!parsed.marks.recovered, "{xml}");
        parsed.marks.report(&parse_ctx, Role::Content(Region::Note));
        let reported = parse_ctx.into_diagnostics().iter().map(|d| (d.code, d.region)).collect();
        Ok((parsed.root.text(), reported))
    }

    fn attribute(xml: &str, dialect: Dialect) -> (String, Vec<(DiagnosticCode, Region)>) {
        let parse_ctx = ParseCtx::default();
        let parsed = parse(&parse_ctx, xml.as_bytes(), dialect, &mut WholeTree).unwrap();
        parsed.marks.report(&parse_ctx, Role::Content(Region::Note));
        let value = parsed.root.child_elems().next().unwrap().attr_any("a").unwrap().to_owned();
        (value, parse_ctx.into_diagnostics().iter().map(|d| (d.code, d.region)).collect())
    }

    const UNRESOLVED: [(DiagnosticCode, Region); 1] =
        [(DiagnosticCode::UnresolvedEntity, Region::Note)];
    const DIALECTS: [Dialect; 2] = [Dialect::Xml, Dialect::Xhtml];

    #[test]
    fn a_bare_less_than_or_a_misplaced_declaration_is_malformed() {
        const BARE: &str = "bare `<` in markup";
        const UNPARSEABLE: &str = "unparseable xml";
        const DECLARATION: &str = "declaration inside the content";
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
            ("<r>a </ b</r>c", BARE),
            ("<r><s>a </s b</r>c", BARE),
            ("<r>a </>b</r>", BARE),
            ("<r>a </ >b</r>", BARE),
            ("<r><s>a </ >b</r>", BARE),
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
            let Marks { recovered, unresolved, undecodable } = parsed.marks;
            (recovered, unresolved, undecodable)
        };
        for xml in ["<a><b>text", "<a><b>text</c></a>", "<a><b x=\"1\" x=\"2\"/></a>"] {
            assert_eq!(marks(xml.as_bytes()), (true, false, false), "{xml}");
        }
        assert_eq!(marks(b"<a>&bogus;</a>"), (false, true, false));
        assert_eq!(marks(&utf16("<a>text</a>", false, true)), (false, false, true));
        assert_eq!(marks(b"<a>text</a>"), (false, false, false));
    }
}

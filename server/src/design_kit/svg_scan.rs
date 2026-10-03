//! Bounds an untrusted SVG before usvg converts it.
//!
//! usvg (and the CSS engine it uses, simplecss) have no limits of their own on depth,
//! expansion or work, and a thread that overflows its stack aborts the whole process
//! (`catch_unwind` cannot catch that). Everything here is iterative and runs on the
//! parsed XML before usvg sees it:
//!
//! * [`depth_upper_bound`]: a byte scan that bounds element nesting before roxmltree's
//!   recursive tokenizer runs (about 5000 nested tags overflow a 2 MiB stack).
//! * [`scan`]: counts and lint facts, the CSS cost, and two **reference graphs**:
//!   - usvg's parser copies the target of every `<use>` into the tree (and a `<use>`
//!     inside a target is copied again with it);
//!   - its converter then follows references: `fill`/`stroke` paint servers
//!     (patterns), `clip-path`, `mask`, `filter` (and `feImage` hrefs), markers
//!     (instantiated at every vertex) and `<use>`, each converting the target's content
//!     nested inside the referencing element. Definitions (`<defs>`, patterns, masks,
//!     ...) are converted only through references.
//!
//!   From those:
//!   - a **cycle** of three or more references (pattern A fills with B, B with C, C
//!     with A), or a pattern whose content inherits a fill that points back at it,
//!     recurses forever: usvg only breaks one- and two-step cycles. Verified to abort the
//!     process even on a 64 MiB stack, with a 600-byte file. Rejected.
//!   - the **nesting** along a chain of references multiplies: 64 patterns, each with 60
//!     groups inside, is about 4,000 levels deep although no element is nested more than
//!     64 deep. Bounded by `Limits::max_svg_nesting`; deep enough to need a big stack
//!     (see `process_upload`).
//!   - the **expansion** multiplies too: every `<use>` copies its target, every path
//!     with an objectBoundingBox pattern gets its own copy of the pattern's content, and
//!     every vertex its own marker. Bounded by `Limits::max_svg_expansion`.
//!
//!   References can come from attributes, `style` attributes and `<style>` sheets, and
//!   `fill`, `stroke` and markers are inherited. Stylesheet rules are matched with the
//!   same CSS engine and element view as usvg's, and every candidate value counts (not
//!   just the cascade winner), so the graph is a superset of what usvg follows.
//! * The **CSS cost**, bounded by `Limits::max_css_work` before simplecss runs:
//!   - simplecss computes a line and column from the start of the text whenever a value
//!     ends, so parsing is quadratic: 16,000 declarations (144 KB) take 1.2 s;
//!   - usvg matches every rule against every element (and `<use>` copy), and a
//!     descendant combinator backtracks through every ancestor, so
//!     `x g g g g g g g g g g` over 60 nested groups is about 10^12 steps.

use std::collections::HashMap;

use simplecss::{SelectorToken, SelectorTokenizer};
use usvg::roxmltree::{self, Node};

use super::{Limits, ProcessError};

const SVG_NS: &str = "http://www.w3.org/2000/svg";
const XLINK_NS: &str = "http://www.w3.org/1999/xlink";

/// Most own references (attribute, `style` and stylesheet values naming an element).
const MAX_REFS: usize = 20_000;
/// Most edges in a reference graph (children, own and inherited references).
const MAX_EDGES: usize = 400_000;
/// Most compound selectors in one CSS selector (simplecss matches them recursively).
const MAX_SELECTOR_COMPONENTS: usize = 32;

const TOO_MANY_REFS: ProcessError = ProcessError::TooComplex("too many references");
const CSS_TOO_COMPLEX: ProcessError = ProcessError::TooComplex("the CSS is too complex");
const EXPANDS: ProcessError =
    ProcessError::TooComplex("references expand to too many shapes (<use>, patterns or markers)");

/// Input facts the prescan noticed, for lints.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Facts {
    /// `<text>`, `<tspan>` or `<textPath>`: not converted (no fonts).
    pub text: bool,
    /// `<image>` elements: never loaded.
    pub images: usize,
    /// Scripts, event handlers, embedded HTML, animations or external links.
    pub active: bool,
    /// `<filter>` definitions: ignored when painting.
    pub filters: bool,
}

/// Conservative upper bound on element nesting depth, computed without recursion.
/// Skips comments, CDATA, processing instructions and the DOCTYPE (with its internal
/// subset), and honours quotes inside tags, so `a="/>"` can't fake a self-closing tag.
/// Over-estimates are fine: it is only used to reject.
///
/// `None` when the markup can't be bounded: a DOCTYPE or `<!` declaration that is
/// malformed or unterminated (roxmltree rejects those too). Skipping more than roxmltree
/// does would hide elements from the count, so the DOCTYPE is read with roxmltree
/// 0.21.1's own grammar ([`doctype_end`]), and anything else is refused.
pub(crate) fn depth_upper_bound(b: &[u8]) -> Option<usize> {
    let n = b.len();
    let (mut i, mut depth, mut max) = (0usize, 0usize, 0usize);
    while i < n {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        let rest = &b[i..];
        if rest.starts_with(b"<!--") {
            i = after(b, i + 4, b"-->").unwrap_or(n);
        } else if rest.starts_with(b"<![CDATA[") {
            i = after(b, i + 9, b"]]>").unwrap_or(n);
        } else if rest.starts_with(b"<?") {
            i = after(b, i + 2, b"?>").unwrap_or(n);
        } else if rest.starts_with(b"<!DOCTYPE") {
            i = doctype_end(b, i + 9)?;
        } else if rest.starts_with(b"<!") {
            // roxmltree accepts no other declaration outside the DOCTYPE.
            return None;
        } else if rest.starts_with(b"</") {
            depth = depth.saturating_sub(1);
            i = after(b, i + 2, b">").unwrap_or(n);
        } else {
            let mut j = i + 1;
            let mut quote = 0u8;
            while j < n {
                let c = b[j];
                if quote != 0 {
                    if c == quote {
                        quote = 0;
                    }
                } else if c == b'"' || c == b'\'' {
                    quote = c;
                } else if c == b'>' {
                    break;
                }
                j += 1;
            }
            let self_closing = j < n && b[j - 1] == b'/';
            if !self_closing {
                depth += 1;
                max = max.max(depth);
            }
            i = j + 1;
        }
    }
    Some(max)
}

/// The index just past the first `pat` at or after `from`.
fn after(b: &[u8], from: usize, pat: &[u8]) -> Option<usize> {
    b.get(from..)?
        .windows(pat.len())
        .position(|w| w == pat)
        .map(|p| from + p + pat.len())
}

/// The index just past a DOCTYPE whose `<!DOCTYPE` ends before `j`, read as roxmltree
/// 0.21.1 reads it (`tokenizer.rs`, `parse_doctype`):
///
/// * before the internal subset, quoted literals (the external ID) are skipped whole,
///   up to `[` or `>`;
/// * in the subset: `<!ENTITY ...>` with quoted values, comments, processing
///   instructions, and `<!ELEMENT`, `<!ATTLIST` and `<!NOTATION`, which roxmltree skips
///   to the first `>` **without** honouring quotes (so `<!ATTLIST a b CDATA ">">` ends
///   at the first `>`, and so must we); then `]`, spaces and `>`.
///
/// `None` for anything else, or at the end of the input: roxmltree fails there too.
fn doctype_end(b: &[u8], mut j: usize) -> Option<usize> {
    let n = b.len();
    let quoted = |j: usize| -> Option<usize> {
        let q = b[j];
        b.get(j + 1..)?
            .iter()
            .position(|&c| c == q)
            .map(|p| j + 1 + p + 1)
    };
    let space = |c: u8| matches!(c, b' ' | b'\t' | b'\r' | b'\n');
    loop {
        match *b.get(j)? {
            b'"' | b'\'' => j = quoted(j)?,
            b'>' => return Some(j + 1),
            b'[' => break,
            _ => j += 1,
        }
    }
    j += 1;
    loop {
        while j < n && space(b[j]) {
            j += 1;
        }
        let rest = b.get(j..)?;
        if rest.starts_with(b"<!ENTITY") {
            j += 8;
            loop {
                match *b.get(j)? {
                    b'"' | b'\'' => j = quoted(j)?,
                    b'>' => break,
                    _ => j += 1,
                }
            }
            j += 1;
        } else if rest.starts_with(b"<!--") {
            j = after(b, j + 4, b"-->")?;
        } else if rest.starts_with(b"<?") {
            j = after(b, j + 2, b"?>")?;
        } else if rest.starts_with(b"<!ELEMENT")
            || rest.starts_with(b"<!ATTLIST")
            || rest.starts_with(b"<!NOTATION")
        {
            j = after(b, j, b">")?;
        } else if rest.starts_with(b"]") {
            j += 1;
            while j < n && space(b[j]) {
                j += 1;
            }
            return (b.get(j) == Some(&b'>')).then_some(j + 1);
        } else {
            return None;
        }
    }
}

/// How a reference behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum RefKind {
    /// `fill` / `stroke`: inherited by descendants.
    Paint,
    /// `marker-start/mid/end` (and the `marker` shorthand): inherited, and instantiated
    /// at every vertex of a path, line, polyline or polygon.
    Marker,
    /// `clip-path`, `mask`, `filter`, `href` and anything else: not inherited.
    Other,
}

impl RefKind {
    fn of_property(name: &str) -> RefKind {
        match name {
            "fill" | "stroke" => RefKind::Paint,
            "marker" | "marker-start" | "marker-mid" | "marker-end" => RefKind::Marker,
            _ => RefKind::Other,
        }
    }

    fn inherited(self) -> bool {
        matches!(self, RefKind::Paint | RefKind::Marker)
    }
}

/// How usvg treats an element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Converted as graphics: children, references and inherited references count.
    Graphic,
    /// Gradients, stops and filter primitives: only an `href` is followed (they aren't
    /// drawn, so inherited paint doesn't recurse through them).
    HrefOnly,
    /// Never parsed: non-SVG namespaces, `foreignObject`, `style`, `script`, metadata.
    Inert,
}

struct El<'a, 'input> {
    node: Node<'a, 'input>,
    parent: Option<usize>,
    /// Root `<svg>` is 1.
    depth: usize,
    /// One past the last descendant, in document order.
    end: usize,
    role: Role,
    /// Only converted through references (`<defs>`, patterns, masks, ...).
    definition: bool,
    /// The `href` and `xlink:href` of a `<use>`.
    use_hrefs: [Option<&'a str>; 2],
    /// Own references: kind and target id.
    refs: Vec<(RefKind, &'a str)>,
    /// Nearest proper ancestor with inherited (paint or marker) references.
    inherit_from: Option<usize>,
    /// Upper bound on vertices, for marker instances (paths, lines, polylines, polygons).
    vertices: u64,
    /// Upper bound on simplecss's work parsing the `style` attribute.
    style_cost: u64,
    /// Nodes the parser visits for this element: itself, and the comments and text
    /// before it (matching `:first-child` scans back over them).
    visit_cost: u64,
}

/// Element names that run code, embed other documents or animate attributes (SMIL can
/// set an `href` to `javascript:`).
const ACTIVE: [&str; 15] = [
    "script",
    "foreignObject",
    "iframe",
    "embed",
    "object",
    "audio",
    "video",
    "canvas",
    "animate",
    "set",
    "animateMotion",
    "animateTransform",
    "animateColor",
    "discard",
    "handler",
];

const XHTML_NS: &str = "http://www.w3.org/1999/xhtml";

/// Illustrator's "Save As SVG" with editing data: `<switch><foreignObject
/// requiredExtensions="&ns_ai;">` holding a reference to its private data, then the
/// drawing as the fallback. Not web content (no HTML inside), and usvg skips any branch
/// with `requiredExtensions`.
fn illustrator_fallback(node: Node) -> bool {
    node.tag_name().name() == "foreignObject"
        && node.has_attribute("requiredExtensions")
        && node
            .parent_element()
            .is_some_and(|p| p.tag_name().name() == "switch")
        && !node
            .descendants()
            .any(|d| d.tag_name().namespace() == Some(XHTML_NS))
}

/// Definitions that usvg resolves by reference (counted against `max_svg_defs`).
const DEFS: [&str; 8] = [
    "clipPath",
    "mask",
    "pattern",
    "marker",
    "symbol",
    "linearGradient",
    "radialGradient",
    "filter",
];

/// Elements whose `href` usvg follows to another element.
const HREF_FOLLOWED: [&str; 8] = [
    "use",
    "pattern",
    "linearGradient",
    "radialGradient",
    "filter",
    "feImage",
    "textPath",
    "tref",
];

/// Upper bound on simplecss's work parsing `text`: the end of every declaration (one
/// `:` each) and every rule (one `{`) computes a line and column by scanning from the
/// start of the text.
fn parse_cost(text: &str) -> u64 {
    let ends = text.bytes().filter(|b| matches!(b, b':' | b'{')).count() as u64;
    (ends + 1).saturating_mul(text.len() as u64 + 1)
}

/// Prescan the parsed document. See the module docs.
pub(crate) fn scan(doc: &roxmltree::Document, limits: &Limits) -> Result<Facts, ProcessError> {
    let root = doc.root_element();
    if root.tag_name().name() != "svg" {
        return Err(ProcessError::InvalidSvg(
            "the root element is not <svg>".into(),
        ));
    }

    // ---- elements in document order: parents, depths, roles, facts, costs ----
    let total_nodes = doc.descendants().count();
    let mut index_of = vec![u32::MAX; total_nodes + 1];
    let mut els: Vec<El> = Vec::new();
    let mut facts = Facts::default();
    let (mut defs, mut uses) = (0usize, 0usize);
    let mut sheets: Vec<&str> = Vec::new();
    for node in root.descendants().filter(|n| n.is_element()) {
        let parent = node
            .parent_element()
            .and_then(|p| index_of.get(p.id().get_usize()).copied())
            .filter(|&i| i != u32::MAX)
            .map(|i| i as usize);
        let parent_el = parent.and_then(|p| els.get(p));
        let depth = parent_el.map_or(1, |p| p.depth + 1);
        if depth > limits.max_svg_depth {
            return Err(ProcessError::TooComplex("elements are nested too deeply"));
        }
        let name = node.tag_name().name();
        let svg_ns = matches!(node.tag_name().namespace(), None | Some(SVG_NS));
        let role = if parent_el.is_some_and(|p| p.role == Role::Inert)
            || !svg_ns
            || matches!(
                name,
                "foreignObject" | "style" | "script" | "title" | "desc" | "metadata"
            ) {
            Role::Inert
        } else if matches!(name, "linearGradient" | "radialGradient" | "stop")
            || name.starts_with("fe")
            || parent_el.is_some_and(|p| p.role == Role::HrefOnly)
        {
            Role::HrefOnly
        } else {
            Role::Graphic
        };
        // usvg reads every `style` element, in any namespace and anywhere.
        if name == "style"
            && matches!(node.attribute("type"), None | Some("text/css"))
            && let Some(text) = node.text()
        {
            sheets.push(text);
        }

        if svg_ns {
            match name {
                "text" | "tspan" | "textPath" => facts.text = true,
                "image" => facts.images += 1,
                "filter" => facts.filters = true,
                "use" => uses += 1,
                _ => {}
            }
            if ACTIVE.contains(&name) && !illustrator_fallback(node) {
                facts.active = true;
            }
            if DEFS.contains(&name) {
                defs += 1;
            }
        }
        for a in node.attributes() {
            if a.name().len() > 2 && a.name().starts_with("on") {
                facts.active = true;
            }
            // External links (`<a href>`, `<use href="file.svg#x">`, ...). Images are
            // counted separately and never loaded.
            if a.name() == "href" && name != "image" && !a.value().trim_start().starts_with('#') {
                facts.active = true;
            }
        }

        let use_hrefs = if role != Role::Inert && name == "use" {
            [node.attribute("href"), node.attribute((XLINK_NS, "href"))]
        } else {
            [None, None]
        };
        let vertices = match name {
            "path" => node.attribute("d").map_or(0, str::len) + 2,
            "polyline" | "polygon" => node.attribute("points").map_or(0, str::len) + 2,
            "line" => 2,
            _ => 0,
        } as u64;
        let preceding = node
            .prev_siblings()
            .skip(1)
            .take_while(|n| !n.is_element())
            .count() as u64;
        if let Some(slot) = index_of.get_mut(node.id().get_usize()) {
            *slot = els.len() as u32;
        }
        els.push(El {
            node,
            parent,
            depth,
            end: 0,
            role,
            definition: name == "defs" || DEFS.contains(&name),
            use_hrefs,
            refs: Vec::new(),
            inherit_from: None,
            vertices,
            style_cost: node.attribute("style").map_or(0, parse_cost),
            visit_cost: 1 + preceding,
        });
    }
    if uses > limits.max_svg_uses {
        return Err(ProcessError::TooComplex("too many <use> elements"));
    }
    if defs > limits.max_svg_defs {
        return Err(ProcessError::TooComplex(
            "too many clip paths, masks, patterns, gradients or filters",
        ));
    }
    for (i, el) in els.iter_mut().enumerate() {
        el.end = i + 1;
    }
    for i in (0..els.len()).rev() {
        if let Some(p) = els[i].parent {
            els[p].end = els[p].end.max(els[i].end);
        }
    }
    let max_depth = els.iter().map(|e| e.depth).max().unwrap_or(1);

    // ---- ids (duplicates allowed: a reference may resolve to any of them). usvg also
    // takes `xml:id` and `svg:id`, so any namespace counts. ----
    let mut ids: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, el) in els.iter().enumerate() {
        for a in el.node.attributes().filter(|a| a.name() == "id") {
            ids.entry(a.value()).or_default().push(i);
        }
    }

    // ---- the parser's tree: every element, plus a copy of each `<use>` target ----
    // Bounds the `<use>` expansion and the per-copy CSS work (style attributes are
    // parsed, and every rule matched, once per copy) before simplecss runs.
    let sheet_cost = sheets
        .iter()
        .fold(0u64, |acc, t| acc.saturating_add(parse_cost(t)));
    if sheet_cost > limits.max_css_work {
        return Err(CSS_TOO_COMPLEX);
    }
    let mut parsed = Graph::with_capacity(els.len());
    for (i, el) in els.iter().enumerate() {
        parsed.begin_node()?;
        if el.role != Role::Inert {
            parsed.children(i, &els, |c| c.role != Role::Inert);
            for id in el.use_hrefs.into_iter().flatten().flat_map(href_ids) {
                parsed.add_targets(&ids, id, 1);
            }
        }
    }
    parsed.finish();
    parsed.check_cycles()?;
    let copies = parsed.walk(|e| els[e].visit_cost);
    let style_work = parsed.walk(|e| els[e].style_cost);
    if style_work.size > limits.max_css_work {
        return Err(CSS_TOO_COMPLEX);
    }
    let max_expanded = u64::from(limits.max_svg_nodes).saturating_add(limits.max_svg_expansion);
    if copies.size > max_expanded {
        return Err(EXPANDS);
    }

    // ---- stylesheet rules: what matching them costs usvg ----
    let mut sheet = simplecss::StyleSheet::new();
    for text in &sheets {
        sheet.parse_more(text);
    }
    let mut per_element = 0u64;
    let mut ref_rules = Vec::new();
    for rule in &sheet.rules {
        per_element = per_element.saturating_add(selector_cost(&rule.selector, max_depth)?);
        if rule
            .declarations
            .iter()
            .any(|d| d.value.contains("url(") || is_inherit(d.value))
        {
            ref_rules.push(rule);
        }
    }
    // usvg tries every rule on every copy; we only try the rules that can add a
    // reference on the originals.
    if per_element.saturating_mul(copies.size) > limits.max_css_work {
        return Err(CSS_TOO_COMPLEX);
    }

    // ---- own references: attributes, `style` attributes, stylesheet rules ----
    // Every attribute counts, whatever its namespace: usvg reads presentation attributes
    // in the XML namespace too. A value of `inherit` takes the parent's value (for
    // `clip-path`, `mask` and `filter` too), so it takes the parent's references.
    let mut n_refs = 0usize;
    for i in 0..els.len() {
        let el = &els[i];
        if el.role == Role::Inert {
            continue;
        }
        let name = el.node.tag_name().name();
        let mut refs = Vec::new();
        let mut inherits = false;
        for a in el.node.attributes() {
            let value = a.value();
            if a.name() == "href" {
                if HREF_FOLLOWED.contains(&name) {
                    refs.extend(href_ids(value).map(|id| (RefKind::Other, id)));
                }
            } else if a.name() == "style" {
                for decl in simplecss::DeclarationTokenizer::from(value) {
                    let kind = RefKind::of_property(decl.name);
                    refs.extend(url_ids(decl.value).map(|id| (kind, id)));
                    inherits |= is_inherit(decl.value);
                }
            } else {
                let kind = RefKind::of_property(a.name());
                refs.extend(url_ids(value).map(|id| (kind, id)));
                inherits |= is_inherit(value);
            }
        }
        if el.role == Role::Graphic {
            for rule in &ref_rules {
                if rule.selector.matches(&XmlNode(el.node)) {
                    for decl in &rule.declarations {
                        let kind = RefKind::of_property(decl.name);
                        refs.extend(url_ids(decl.value).map(|id| (kind, id)));
                        inherits |= is_inherit(decl.value);
                    }
                }
            }
            if inherits && let Some(p) = el.parent {
                refs.extend(els[p].refs.iter().copied());
            }
        } else {
            // Gradients, stops and primitives: only an `href` is followed.
            refs.retain(|(k, _)| *k == RefKind::Other);
        }
        refs.sort_unstable();
        refs.dedup();
        refs.retain(|(_, id)| ids.contains_key(id));
        n_refs += refs.len();
        if n_refs > MAX_REFS {
            return Err(TOO_MANY_REFS);
        }
        els[i].refs = refs;
    }

    // ---- inherited references: nearest ancestor with paint or marker references ----
    for i in 0..els.len() {
        if let Some(p) = els[i].parent {
            let parent_has = els[p].refs.iter().any(|(k, _)| k.inherited());
            els[i].inherit_from = if parent_has {
                Some(p)
            } else {
                els[p].inherit_from
            };
        }
    }

    // ---- the converter: rendered children, own and inherited references ----
    let mut converted = Graph::with_capacity(els.len());
    for (i, el) in els.iter().enumerate() {
        converted.begin_node()?;
        if el.role == Role::Inert {
            continue;
        }
        if el.role == Role::Graphic {
            // Definitions inside are converted only when referenced.
            converted.children(i, &els, |c| !c.definition && c.role != Role::Inert);
        }
        converted.refs(&ids, &el.refs, el.vertices, false);
        if el.role == Role::Graphic {
            let mut a = el.inherit_from;
            while let Some(anc) = a {
                converted.refs(&ids, &els[anc].refs, el.vertices, true);
                if converted.edges.len() > MAX_EDGES {
                    return Err(TOO_MANY_REFS);
                }
                a = els[anc].inherit_from;
            }
        }
    }
    converted.finish();
    converted.check_cycles()?;
    let shapes = converted.walk(|_| 1);
    if shapes.chain.max(copies.chain) > limits.max_svg_nesting {
        return Err(ProcessError::TooComplex(
            "references nest too deeply (patterns, masks, clip paths, markers or <use>)",
        ));
    }
    if shapes.size > max_expanded {
        return Err(EXPANDS);
    }
    Ok(facts)
}

/// A graph over the elements (node 0 is the root), in compressed adjacency form. Edge
/// weight: how many copies of the target's content following the edge makes (one per
/// vertex for markers).
struct Graph {
    start: Vec<usize>,
    edges: Vec<(usize, u64)>,
}

/// The longest chain from the root, and the weighted size of everything it expands to.
struct Reach {
    chain: usize,
    size: u64,
}

impl Graph {
    fn with_capacity(nodes: usize) -> Graph {
        Graph {
            start: Vec::with_capacity(nodes + 1),
            edges: Vec::new(),
        }
    }

    /// Start the edges of the next node, in element order.
    fn begin_node(&mut self) -> Result<(), ProcessError> {
        if self.edges.len() > MAX_EDGES {
            return Err(TOO_MANY_REFS);
        }
        self.start.push(self.edges.len());
        Ok(())
    }

    /// After the last node.
    fn finish(&mut self) {
        self.start.push(self.edges.len());
    }

    /// Edges to element `i`'s children that `keep` accepts.
    fn children(&mut self, i: usize, els: &[El], keep: impl Fn(&El) -> bool) {
        let mut c = i + 1;
        while c < els[i].end {
            if keep(&els[c]) {
                self.edges.push((c, 1));
            }
            c = els[c].end;
        }
    }

    fn add_targets(&mut self, ids: &HashMap<&str, Vec<usize>>, id: &str, copies: u64) {
        for &t in ids.get(id).map_or(&[][..], Vec::as_slice) {
            self.edges.push((t, copies));
        }
    }

    /// Edges for references (all of them, or only the inherited kinds).
    fn refs(
        &mut self,
        ids: &HashMap<&str, Vec<usize>>,
        refs: &[(RefKind, &str)],
        vertices: u64,
        inherited_only: bool,
    ) {
        for &(kind, id) in refs {
            if inherited_only && !kind.inherited() {
                continue;
            }
            let copies = match kind {
                RefKind::Marker => vertices,
                _ => 1,
            };
            if copies > 0 {
                self.add_targets(ids, id, copies);
            }
        }
    }

    fn nodes(&self) -> usize {
        self.start.len().saturating_sub(1)
    }

    fn out(&self, node: usize) -> &[(usize, u64)] {
        &self.edges[self.start[node]..self.start[node + 1]]
    }

    /// Reject any cycle, starting from every node. Iterative.
    fn check_cycles(&self) -> Result<(), ProcessError> {
        const NEW: u8 = 0;
        const OPEN: u8 = 1;
        const DONE: u8 = 2;
        let n = self.nodes();
        let mut state = vec![NEW; n];
        let mut stack: Vec<(usize, usize)> = Vec::new();
        for root in 0..n {
            if state[root] != NEW {
                continue;
            }
            state[root] = OPEN;
            stack.push((root, 0));
            while let Some(top) = stack.last_mut() {
                let (node, k) = *top;
                if let Some(&(t, _)) = self.out(node).get(k) {
                    top.1 += 1;
                    match state[t] {
                        NEW => {
                            state[t] = OPEN;
                            stack.push((t, 0));
                        }
                        OPEN => {
                            return Err(ProcessError::TooComplex(
                                "patterns, masks, clip paths, markers, filters or <use> refer \
                                 to each other in a loop",
                            ));
                        }
                        _ => {}
                    }
                } else {
                    state[node] = DONE;
                    stack.pop();
                }
            }
        }
        Ok(())
    }

    /// Longest chain and weighted expanded size from the root (node 0). The graph must
    /// be acyclic (`check_cycles`). Iterative.
    fn walk(&self, weight: impl Fn(usize) -> u64) -> Reach {
        let n = self.nodes();
        if n == 0 {
            return Reach { chain: 0, size: 0 };
        }
        let mut done = vec![false; n];
        let mut chain = vec![0usize; n];
        let mut size = vec![0u64; n];
        let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
        while let Some(top) = stack.last_mut() {
            let (node, k) = *top;
            if let Some(&(t, _)) = self.out(node).get(k) {
                top.1 += 1;
                if !done[t] {
                    stack.push((t, 0));
                }
            } else {
                let out = self.out(node);
                chain[node] = 1 + out.iter().map(|&(t, _)| chain[t]).max().unwrap_or(0);
                size[node] = out.iter().fold(weight(node), |acc, &(t, copies)| {
                    acc.saturating_add(size[t].saturating_mul(copies))
                });
                done[node] = true;
                stack.pop();
            }
        }
        Reach {
            chain: chain[0],
            size: size[0],
        }
    }
}

/// Upper bound on the work of matching `selector` against one element: its length (each
/// simple selector is checked) times `max_depth + 1` per descendant combinator (each
/// backtracks through every ancestor).
fn selector_cost(selector: &simplecss::Selector, max_depth: usize) -> Result<u64, ProcessError> {
    let text = selector.to_string();
    let (mut descendant, mut components) = (0u32, 1usize);
    let parsed = SelectorTokenizer::from(text.as_str()).try_for_each(|t| {
        match t? {
            SelectorToken::DescendantCombinator => {
                descendant += 1;
                components += 1;
            }
            SelectorToken::ChildCombinator | SelectorToken::AdjacentCombinator => components += 1,
            _ => {}
        }
        Ok::<(), simplecss::Error>(())
    });
    if parsed.is_err() {
        // Our display of it didn't re-tokenise; assume every space is a combinator.
        let spaces = text.matches(' ').count();
        descendant = u32::try_from(spaces).unwrap_or(u32::MAX);
        components = spaces + 1;
    }
    if components > MAX_SELECTOR_COMPONENTS {
        return Err(CSS_TOO_COMPLEX);
    }
    let per_level = max_depth as u64 + 1;
    Ok((text.len() as u64 + 1).saturating_mul(per_level.saturating_pow(descendant)))
}

/// A property value of `inherit`.
fn is_inherit(value: &str) -> bool {
    value.trim().eq_ignore_ascii_case("inherit")
}

/// XML and svgtypes whitespace.
fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

/// The id an `href` names, as svgtypes reads it (`IRI`: spaces, `#`, then everything up
/// to a space), and the same trimmed. Ids may contain tabs or newlines (written as
/// character references), so both are kept: an extra candidate only adds an edge.
fn href_ids(value: &str) -> impl Iterator<Item = &str> {
    let exact = value
        .trim_start_matches(is_space)
        .strip_prefix('#')
        .map(|rest| rest.split(' ').next().unwrap_or(rest));
    candidates(exact)
}

/// Ids in `url(#id)` references in a property value, as svgtypes reads them
/// (`FuncIRI`: quoted, up to the quote and trimmed at the end; unquoted, up to a space or
/// `)`), and the same trimmed.
fn url_ids(value: &str) -> impl Iterator<Item = &str> {
    value.split("url(").skip(1).flat_map(|rest| {
        let rest = rest.trim_start_matches(is_space);
        let quote = rest.chars().next().filter(|c| matches!(c, '\'' | '"'));
        let rest = match quote {
            Some(q) => rest[q.len_utf8()..].trim_start_matches(is_space),
            None => rest,
        };
        let exact = rest.strip_prefix('#').map(|link| match quote {
            Some(q) => link.split(q).next().unwrap_or(link).trim_end(),
            None => link.split([' ', ')']).next().unwrap_or(link),
        });
        candidates(exact)
    })
}

fn candidates(exact: Option<&str>) -> impl Iterator<Item = &str> {
    let trimmed = exact.map(str::trim).filter(|t| Some(*t) != exact);
    exact.into_iter().chain(trimmed).filter(|id| !id.is_empty())
}

/// The element view simplecss matches against: the same as usvg's (`svgtree/parse.rs`),
/// so the same rules match.
struct XmlNode<'a, 'input>(Node<'a, 'input>);

impl simplecss::Element for XmlNode<'_, '_> {
    fn parent_element(&self) -> Option<Self> {
        self.0.parent_element().map(XmlNode)
    }

    fn prev_sibling_element(&self) -> Option<Self> {
        self.0.prev_sibling_element().map(XmlNode)
    }

    fn has_local_name(&self, local_name: &str) -> bool {
        self.0.tag_name().name() == local_name
    }

    fn attribute_matches(&self, local_name: &str, operator: simplecss::AttributeOperator) -> bool {
        match self.0.attribute(local_name) {
            Some(value) => operator.matches(value),
            None => false,
        }
    }

    fn pseudo_class_matches(&self, class: simplecss::PseudoClass) -> bool {
        match class {
            simplecss::PseudoClass::FirstChild => self.prev_sibling_element().is_none(),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_scan_counts_open_tags() {
        assert_eq!(depth_upper_bound(b"<svg><g><g/></g></svg>"), Some(2));
        assert_eq!(
            depth_upper_bound(b"<svg><!-- <g><g><g> --><g></g></svg>"),
            Some(2)
        );
        assert_eq!(
            depth_upper_bound(b"<!DOCTYPE svg [<!ELEMENT g ANY>]><svg><g a=\"/>\"></g></svg>"),
            Some(2)
        );
        assert_eq!(depth_upper_bound(b"<svg><![CDATA[<g><g>]]></svg>"), Some(1));
    }

    #[test]
    fn depth_scan_reads_the_doctype_as_roxmltree_does() {
        let nested = "<svg><g><g></g></g></svg>";
        let depth = |doctype: &str| depth_upper_bound(format!("{doctype}{nested}").as_bytes());
        // Each of these is a DOCTYPE that roxmltree parses before the three elements.
        for doctype in [
            "<!DOCTYPE svg PUBLIC \"-//W3C//DTD SVG 1.1//EN\" \"http://x/svg11.dtd\">",
            "<!DOCTYPE svg PUBLIC \"a>b\" 'c]>'>",
            // A quoted `[` (the review's bypass: counting brackets swallowed the file).
            "<!DOCTYPE svg [<!ATTLIST svg a CDATA \"[\">]>",
            // roxmltree ends ATTLIST, ELEMENT and NOTATION at the first `>`, quoted or not.
            "<!DOCTYPE svg [<!ATTLIST svg a CDATA \">]>",
            "<!DOCTYPE svg [<!ELEMENT svg '>]>",
            // Entity values, comments and PIs are skipped whole.
            "<!DOCTYPE svg [<!ENTITY a \">]>\"> <!ENTITY b SYSTEM 'x>]'>]>",
            "<!DOCTYPE svg [ <!-- ]> --> <?pi ]> ?> ]\n>",
        ] {
            assert_eq!(depth(doctype), Some(3), "{doctype}");
        }
        // Anything roxmltree doesn't parse can't be bounded.
        for bad in [
            "<!DOCTYPE svg [<!ENTITY a \"x\">",
            "<!DOCTYPE svg [<!FOO>]>",
            "<!DOCTYPE svg [<!ATTLIST svg a CDATA \"x\"]>",
            "<!DOCTYPE svg [<!-- ]>",
            "<!DOCTYPE svg \"",
            "<!doctype svg>",
            "<!FOO>",
        ] {
            assert_eq!(depth(bad), None, "{bad}");
        }
        // An unterminated comment, CDATA section or tag is the end of the input for
        // roxmltree too.
        assert_eq!(depth_upper_bound(b"<svg><g><!-- <g>"), Some(2));
    }

    #[test]
    fn reference_ids_are_read_as_svgtypes_reads_them() {
        let ids: Vec<&str> = url_ids("url(#a) url( \"#b\" ) url('#c'),url(x.svg#d) none").collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
        assert_eq!(url_ids("url(#)").count(), 0);
        // A tab (a character reference in the file) is part of the id for svgtypes.
        assert_eq!(url_ids("url(#a\t)").collect::<Vec<_>>(), vec!["a\t", "a"]);
        assert_eq!(url_ids("url('#a\t ')").collect::<Vec<_>>(), vec!["a"]);
        assert_eq!(href_ids(" #a").collect::<Vec<_>>(), vec!["a"]);
        assert_eq!(href_ids("#a\t").collect::<Vec<_>>(), vec!["a\t", "a"]);
        assert_eq!(href_ids("#a b").collect::<Vec<_>>(), vec!["a"]);
        assert_eq!(href_ids("x.svg#a").count(), 0);
    }

    fn parse(svg: &str) -> roxmltree::Document<'_> {
        roxmltree::Document::parse(svg).expect("test XML")
    }

    fn run(svg: &str) -> Result<Facts, ProcessError> {
        scan(&parse(svg), &Limits::default())
    }

    #[test]
    fn three_step_pattern_cycle_is_rejected() {
        let pat = |id: &str, next: &str| {
            format!(
                "<pattern id=\"{id}\" width=\"9\" height=\"9\"><rect width=\"5\" height=\"5\" \
                 fill=\"url(#{next})\"/></pattern>"
            )
        };
        let svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\"><defs>{}{}{}</defs>\
             <rect width=\"9\" height=\"9\" fill=\"url(#A)\"/></svg>",
            pat("A", "B"),
            pat("B", "C"),
            pat("C", "A")
        );
        assert!(matches!(run(&svg), Err(ProcessError::TooComplex(m)) if m.contains("loop")));
        // Without the back edge it is fine.
        let ok = svg.replace("url(#A)\"/></pattern>", "black\"/></pattern>");
        assert_eq!(run(&ok), Ok(Facts::default()));
    }

    #[test]
    fn inherited_and_stylesheet_references_count() {
        // A pattern whose content inherits the fill that points at it.
        let inherited = "<svg xmlns=\"http://www.w3.org/2000/svg\"><g fill=\"url(#A)\">\
             <pattern id=\"A\" width=\"9\" height=\"9\"><rect width=\"5\" height=\"5\"/></pattern>\
             <rect width=\"9\" height=\"9\"/></g></svg>";
        assert!(matches!(run(inherited), Err(ProcessError::TooComplex(_))));
        // The same loop made with CSS classes.
        let css = "<svg xmlns=\"http://www.w3.org/2000/svg\"><style>.a{fill:url(#B)}\
             .b{fill:url(#A)}</style><pattern id=\"A\" width=\"9\" height=\"9\">\
             <rect class=\"a\" width=\"5\" height=\"5\"/></pattern><pattern id=\"B\" width=\"9\" \
             height=\"9\"><rect class=\"b\" width=\"5\" height=\"5\"/></pattern>\
             <rect width=\"9\" height=\"9\" fill=\"url(#A)\"/></svg>";
        assert!(matches!(run(css), Err(ProcessError::TooComplex(_))));
        // Gradients referenced from a class on their own group are not a loop: stops
        // aren't drawn.
        let grad = "<svg xmlns=\"http://www.w3.org/2000/svg\"><style>.g{fill:url(#G)}</style>\
             <g class=\"g\"><linearGradient id=\"G\"><stop offset=\"0\"/></linearGradient>\
             <rect width=\"9\" height=\"9\"/></g></svg>";
        assert_eq!(run(grad), Ok(Facts::default()));
    }

    #[test]
    fn nesting_multiplies_along_reference_chains() {
        // 40 patterns, each with 40 nested groups: under every per-element cap, but
        // about 1700 levels deep once followed.
        let limits = Limits {
            max_svg_nesting: 1000,
            ..Limits::default()
        };
        let mut s = String::from("<svg xmlns=\"http://www.w3.org/2000/svg\"><defs>");
        for i in 0..40 {
            let fill = if i < 39 {
                format!("url(#p{})", i + 1)
            } else {
                "black".into()
            };
            s += &format!(
                "<pattern id=\"p{i}\" width=\"9\" height=\"9\">{}<rect width=\"5\" height=\"5\" \
                 fill=\"{fill}\"/>{}</pattern>",
                "<g>".repeat(40),
                "</g>".repeat(40)
            );
        }
        s += "</defs><rect width=\"9\" height=\"9\" fill=\"url(#p0)\"/></svg>";
        let doc = parse(&s);
        assert!(matches!(
            scan(&doc, &limits),
            Err(ProcessError::TooComplex(m)) if m.contains("nest")
        ));
        assert!(scan(&doc, &Limits::default()).is_ok());
    }

    #[test]
    fn markers_expand_per_vertex() {
        let mut s = String::from(
            "<svg xmlns=\"http://www.w3.org/2000/svg\"><marker id=\"m\" markerWidth=\"9\" \
             markerHeight=\"9\">",
        );
        s += &"<rect width=\"1\" height=\"1\"/>".repeat(1000);
        s += "</marker><path marker-mid=\"url(#m)\" d=\"M0 0";
        s += &" 1 1".repeat(1000);
        s += "\"/></svg>";
        assert_eq!(run(&s), Err(EXPANDS));
    }

    #[test]
    fn css_costs_are_bounded_before_simplecss_runs() {
        // Backtracking selectors.
        let svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\"><style>x g g g g g g g g g g \
             {{fill:red}}</style>{}<rect width=\"9\" height=\"9\"/>{}</svg>",
            "<g>".repeat(60),
            "</g>".repeat(60)
        );
        assert_eq!(run(&svg), Err(CSS_TOO_COMPLEX));
        // Quadratic declaration parsing, in a sheet and in a style attribute.
        let decls = "fill:red;".repeat(16_000);
        let sheet =
            format!("<svg xmlns=\"http://www.w3.org/2000/svg\"><style>a{{{decls}}}</style></svg>");
        assert_eq!(run(&sheet), Err(CSS_TOO_COMPLEX));
        let attr =
            format!("<svg xmlns=\"http://www.w3.org/2000/svg\"><rect style=\"{decls}\"/></svg>");
        assert_eq!(run(&attr), Err(CSS_TOO_COMPLEX));
        // A modest style attribute copied by many `<use>`s.
        let copied = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\"><g id=\"g\">{}</g>{}</svg>",
            format!("<rect style=\"{}\"/>", "fill:red;".repeat(200)).repeat(20),
            "<use href=\"#g\"/>".repeat(400)
        );
        assert_eq!(run(&copied), Err(CSS_TOO_COMPLEX));
        // Simple class rules (what Illustrator exports) are cheap.
        let simple = "<svg xmlns=\"http://www.w3.org/2000/svg\"><style>.st0{fill:#231F20;}\
             .st1{fill:url(#g)}</style><linearGradient id=\"g\"><stop offset=\"0\"/>\
             </linearGradient><path class=\"st0\" d=\"M0 0H9V9Z\"/><path class=\"st1\" \
             d=\"M0 0H9V9Z\"/></svg>";
        assert_eq!(run(simple), Ok(Facts::default()));
    }
}

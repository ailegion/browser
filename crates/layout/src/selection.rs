//! Text positions and selections over a laid-out tree.
//!
//! A position is a text node and a byte offset into the text of the inline
//! root that node is part of (`TextFragment::text`). Both survive scrolling
//! and relayout, since neither depends on where the text landed. Tree order
//! of the text fragments stands in for document order.

use std::collections::HashMap;
use std::sync::Arc;

use browser_dom::NodeId;

use crate::{Fragment, FragmentContent, LayoutTree, Rect, TextFragment};

/// A place between two characters of a text node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextPos {
    pub node: NodeId,
    /// Byte offset into the inline root's text.
    pub offset: usize,
}

/// What a selection covers, per text node: a byte range of the node's
/// inline root text. Nodes wholly inside get `0..usize::MAX`.
#[derive(Debug, Clone, Default)]
pub struct SelectionRanges {
    by_node: HashMap<NodeId, (usize, usize)>,
}

impl SelectionRanges {
    pub fn is_empty(&self) -> bool {
        self.by_node.is_empty()
    }

    pub fn get(&self, node: NodeId) -> Option<(usize, usize)> {
        self.by_node.get(&node).copied()
    }
}

/// A text fragment with a node, in tree order.
struct TextRef<'a> {
    rect: Rect,
    node: NodeId,
    text: &'a TextFragment,
}

fn texts(tree: &LayoutTree) -> Vec<TextRef<'_>> {
    fn visit<'a>(f: &'a Fragment, out: &mut Vec<TextRef<'a>>) {
        if let (FragmentContent::Text(t), Some(node)) = (&f.content, f.node) {
            out.push(TextRef {
                rect: f.rect,
                node,
                text: t,
            });
        }
        for c in &f.children {
            visit(c, out);
        }
    }
    let mut out = Vec::new();
    visit(&tree.root, &mut out);
    out
}

/// The character boundary nearest to `x` within the fragment.
fn offset_at_x(t: &TextRef<'_>, x: f32) -> usize {
    let lx = x - t.rect.x;
    let clusters = &t.text.clusters;
    let (Some(first), Some(last)) = (clusters.first(), clusters.last()) else {
        return t.text.range.start;
    };
    let left_edge = |c: &crate::Cluster| if c.rtl { c.end } else { c.start };
    let right_edge = |c: &crate::Cluster| if c.rtl { c.start } else { c.end };
    if lx < first.x {
        return left_edge(first);
    }
    for c in clusters {
        if lx < c.x + c.advance {
            return if lx < c.x + c.advance / 2.0 {
                left_edge(c)
            } else {
                right_edge(c)
            };
        }
    }
    right_edge(last)
}

/// The text position nearest to a point in page coordinates: within the
/// fragment under it; else on the fragment of the same line nearest
/// horizontally; else at the end of the last text above the point, or the
/// start of the document when the point is above all text.
pub fn text_position_at(tree: &LayoutTree, x: f32, y: f32) -> Option<TextPos> {
    let texts = texts(tree);
    if let Some(t) = texts.iter().find(|t| t.rect.contains(x, y)) {
        return Some(TextPos {
            node: t.node,
            offset: offset_at_x(t, x),
        });
    }
    let same_line = texts
        .iter()
        .filter(|t| y >= t.rect.y && y < t.rect.bottom())
        .min_by(|a, b| {
            let d = |t: &TextRef<'_>| if x < t.rect.x { t.rect.x - x } else { x - t.rect.right() };
            d(a).total_cmp(&d(b))
        });
    if let Some(t) = same_line {
        return Some(TextPos {
            node: t.node,
            offset: offset_at_x(t, x),
        });
    }
    if let Some(t) = texts.iter().rev().find(|t| t.rect.bottom() <= y) {
        return Some(TextPos {
            node: t.node,
            offset: t.text.range.end,
        });
    }
    texts.first().map(|t| TextPos {
        node: t.node,
        offset: t.text.range.start,
    })
}

/// The first and last positions of the document's text.
pub fn text_extent(tree: &LayoutTree) -> Option<(TextPos, TextPos)> {
    let texts = texts(tree);
    let first = texts.first()?;
    let last = texts.last()?;
    Some((
        TextPos {
            node: first.node,
            offset: first.text.range.start,
        },
        TextPos {
            node: last.node,
            offset: last.text.range.end,
        },
    ))
}

/// Index in `texts` of the first fragment of each node.
fn first_index(texts: &[TextRef<'_>]) -> HashMap<NodeId, usize> {
    let mut first = HashMap::new();
    for (i, t) in texts.iter().enumerate() {
        first.entry(t.node).or_insert(i);
    }
    first
}

/// Sort key of a position: its node's place in tree order, then the offset.
fn key(first: &HashMap<NodeId, usize>, pos: TextPos) -> Option<(usize, usize)> {
    first.get(&pos.node).map(|&i| (i, pos.offset))
}

fn ranges_of(texts: &[TextRef<'_>], first: &HashMap<NodeId, usize>, a: TextPos, b: TextPos) -> SelectionRanges {
    let mut out = SelectionRanges::default();
    let (Some(ka), Some(kb)) = (key(first, a), key(first, b)) else {
        return out;
    };
    let (ka, kb) = if ka <= kb { (ka, kb) } else { (kb, ka) };
    if ka == kb {
        return out;
    }
    for t in texts {
        let ord = first[&t.node];
        if ord < ka.0 || ord > kb.0 || out.by_node.contains_key(&t.node) {
            continue;
        }
        let start = if ord == ka.0 { ka.1 } else { 0 };
        let end = if ord == kb.0 { kb.1 } else { usize::MAX };
        if start < end {
            out.by_node.insert(t.node, (start, end));
        }
    }
    out
}

/// What the painter highlights for a selection between two positions, in
/// either order.
pub fn selection_ranges(tree: &LayoutTree, a: TextPos, b: TextPos) -> SelectionRanges {
    let texts = texts(tree);
    let first = first_index(&texts);
    ranges_of(&texts, &first, a, b)
}

/// The selected text, as it would be copied: each inline root contributes
/// the slice of its text between the outermost selected clusters, and
/// roots are joined with newlines.
pub fn selection_text(tree: &LayoutTree, a: TextPos, b: TextPos) -> String {
    let texts = texts(tree);
    let first = first_index(&texts);
    let ranges = ranges_of(&texts, &first, a, b);
    let mut out = String::new();
    // The root being gathered: its text and the selected slice so far.
    let mut current: Option<(&Arc<str>, usize, usize)> = None;
    let flush = |current: &mut Option<(&Arc<str>, usize, usize)>, out: &mut String| {
        if let Some((text, lo, hi)) = current.take()
            && lo < hi
            && let Some(slice) = text.get(lo..hi)
        {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(slice);
        }
    };
    for t in &texts {
        let Some((sa, sb)) = ranges.get(t.node) else { continue };
        let lo = t.text.range.start.max(sa);
        let hi = t.text.range.end.min(sb);
        if lo >= hi {
            continue;
        }
        match &mut current {
            Some((text, clo, chi)) if Arc::ptr_eq(text, &t.text.text) => {
                *clo = (*clo).min(lo);
                *chi = (*chi).max(hi);
            }
            _ => {
                flush(&mut current, &mut out);
                current = Some((&t.text.text, lo, hi));
            }
        }
    }
    flush(&mut current, &mut out);
    out
}

/// The word around a position, for a double click: a run of letters and
/// digits, or a run of spaces, or a single other character, kept within
/// the node's own text.
pub fn word_at(tree: &LayoutTree, pos: TextPos) -> Option<(TextPos, TextPos)> {
    let texts = texts(tree);
    let mine: Vec<&TextRef<'_>> = texts.iter().filter(|t| t.node == pos.node).collect();
    let first = mine.first()?;
    let text: &str = &first.text.text;
    let lo = mine.iter().map(|t| t.text.range.start).min()?;
    let hi = mine.iter().map(|t| t.text.range.end).max()?;
    let offset = pos.offset.clamp(lo, hi);
    let slice = text.get(lo..hi)?;
    let rel = offset - lo;
    #[derive(PartialEq)]
    enum Class {
        Word,
        Space,
        Other,
    }
    let class = |c: char| {
        if c.is_alphanumeric() || c == '_' || c == '\'' {
            Class::Word
        } else if c.is_whitespace() {
            Class::Space
        } else {
            Class::Other
        }
    };
    // The character at the offset, or the one before it at the end.
    let at = slice[rel..].chars().next().or_else(|| slice[..rel].chars().next_back())?;
    let cls = class(at);
    if cls == Class::Other {
        let (s, e) = if slice[rel..].starts_with(at) {
            (rel, rel + at.len_utf8())
        } else {
            (rel - at.len_utf8(), rel)
        };
        return Some((
            TextPos { node: pos.node, offset: lo + s },
            TextPos { node: pos.node, offset: lo + e },
        ));
    }
    let start = slice[..rel]
        .char_indices()
        .rev()
        .take_while(|(_, c)| class(*c) == cls)
        .last()
        .map_or(rel, |(i, _)| i);
    let end = slice[rel..]
        .char_indices()
        .take_while(|(_, c)| class(*c) == cls)
        .last()
        .map_or(rel, |(i, c)| rel + i + c.len_utf8());
    Some((
        TextPos { node: pos.node, offset: lo + start },
        TextPos { node: pos.node, offset: lo + end },
    ))
}

/// The whole inline root (a paragraph, usually) around a position, for a
/// triple click.
pub fn paragraph_at(tree: &LayoutTree, pos: TextPos) -> Option<(TextPos, TextPos)> {
    let texts = texts(tree);
    let root = &texts.iter().find(|t| t.node == pos.node)?.text.text;
    let mut members = texts.iter().filter(|t| Arc::ptr_eq(&t.text.text, root));
    let first = members.next()?;
    let last = members.next_back().unwrap_or(first);
    Some((
        TextPos {
            node: first.node,
            offset: 0,
        },
        TextPos {
            node: last.node,
            offset: root.len(),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use browser_dom::Document;
    use browser_style::{Stylist, Viewport, compute_styles, ua::ua_stylesheet};

    fn layout(html: &str) -> (Document, LayoutTree) {
        let doc = browser_dom::parse_html(html.as_bytes());
        let mut stylist = Stylist::new();
        stylist.add_sheet(ua_stylesheet());
        let styles = compute_styles(&doc, &stylist, &Viewport::default());
        let mut engine = crate::LayoutEngine::new();
        let tree = engine.layout(&doc, &styles, 800.0, 600.0, &());
        (doc, tree)
    }

    const PAGE: &str = "<body style='margin:0;font-size:16px;line-height:20px'>\
        <p style='margin:0'>Hello <b>big</b> world</p>\
        <p style='margin:0'>Second line here</p></body>";

    #[test]
    fn positions_map_to_offsets_and_select_across_paragraphs() {
        let (_doc, tree) = layout(PAGE);
        let texts = texts(&tree);
        assert!(texts.len() >= 4, "one fragment per span at least: {}", texts.len());
        assert_eq!(&*texts[0].text.text, "Hello big world");
        assert_eq!(texts[0].text.range, 0..6);
        assert!(texts[0].text.clusters.len() == 6);

        // Far left of the first line: the start.
        let start = text_position_at(&tree, -5.0, 10.0).expect("pos");
        assert_eq!(start.offset, 0);
        // Far right of the first line: its end.
        let end1 = text_position_at(&tree, 790.0, 10.0).expect("pos");
        assert_eq!(end1.offset, 15);
        // Inside the second paragraph, past its end.
        let end2 = text_position_at(&tree, 790.0, 30.0).expect("pos");
        assert_eq!(end2.offset, "Second line here".len());
        // Below everything: the end of the last text.
        assert_eq!(text_position_at(&tree, 100.0, 500.0), Some(end2));
        // Above everything: the start.
        assert_eq!(text_position_at(&tree, 100.0, -50.0), Some(start));

        // From the middle of "Hello" to the middle of "Second".
        let h_mid = texts[0].rect.x + texts[0].text.clusters[2].x + 0.1;
        let a = text_position_at(&tree, h_mid, 10.0).expect("a");
        assert_eq!(a.offset, 2);
        let b = text_position_at(&tree, h_mid, 30.0).expect("b");
        assert_eq!(b.offset, 2);
        assert_eq!(selection_text(&tree, a, b), "llo big world\nSe");
        assert_eq!(selection_text(&tree, b, a), "llo big world\nSe", "order does not matter");
        let ranges = selection_ranges(&tree, a, b);
        assert_eq!(ranges.get(a.node), Some((2, usize::MAX)));
        assert_eq!(ranges.get(b.node), Some((0, 2)));
        assert!(ranges.get(texts[1].node).is_some(), "the bold node in between");

        // A collapsed selection is nothing.
        assert!(selection_ranges(&tree, a, a).is_empty());
        assert_eq!(selection_text(&tree, a, a), "");

        // Everything.
        let (s, e) = text_extent(&tree).expect("extent");
        assert_eq!(selection_text(&tree, s, e), "Hello big world\nSecond line here");
    }

    #[test]
    fn words_and_paragraphs() {
        let (_doc, tree) = layout(PAGE);
        let texts = texts(&tree);
        // Inside "world" (the third text node, after the bold one).
        let world = TextPos {
            node: texts[2].node,
            offset: 12,
        };
        let (a, b) = word_at(&tree, world).expect("word");
        assert_eq!(selection_text(&tree, a, b), "world");
        // On the space after "big" within the first node's text: the space.
        let space = TextPos {
            node: texts[0].node,
            offset: 5,
        };
        let (a, b) = word_at(&tree, space).expect("space");
        assert_eq!((a.offset, b.offset), (5, 6));
        // Inside "Hello": the word.
        let (a, b) = word_at(&tree, TextPos { node: texts[0].node, offset: 3 }).expect("word");
        assert_eq!(selection_text(&tree, a, b), "Hello");
        // The paragraph around the bold word is the whole first line.
        let (a, b) = paragraph_at(&tree, TextPos { node: texts[1].node, offset: 7 }).expect("para");
        assert_eq!(selection_text(&tree, a, b), "Hello big world");
    }

    #[test]
    fn a_position_outside_the_layout_selects_nothing() {
        let (doc, tree) = layout("<p>shown</p><p style='display:none'>hidden</p>");
        let hidden = doc
            .descendants(doc.root())
            .find(|&n| matches!(&doc.get(n).kind, browser_dom::NodeKind::Text(t) if t == "hidden"))
            .expect("hidden text node");
        let (s, _) = text_extent(&tree).expect("extent");
        let stale = TextPos {
            node: hidden,
            offset: 3,
        };
        assert!(selection_ranges(&tree, s, stale).is_empty());
        assert_eq!(selection_text(&tree, stale, s), "");
        assert!(word_at(&tree, stale).is_none());
        assert!(paragraph_at(&tree, stale).is_none());
    }
}

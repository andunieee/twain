//! Line diffing, intra-line diffing and the vertical layout of a side-by-side file diff.

use similar::{Algorithm, DiffOp, DiffTag, TextDiff};
use std::collections::BTreeMap;
use std::ops::Range;
use std::time::{Duration, Instant};

/// Unchanged lines shown around each change when a file is collapsed.
pub const CONTEXT: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChunkKind {
    Insert,
    Delete,
    Replace,
}

#[derive(Clone, Debug)]
pub struct Chunk {
    pub kind: ChunkKind,
    pub old: Range<usize>,
    pub new: Range<usize>,
}

#[derive(Clone, Debug)]
pub enum Block {
    Equal { old: usize, new: usize, len: usize },
    Change(Chunk),
}

/// Makes the text safe to put in a GtkTextBuffer without changing its line structure:
/// the buffer treats lone `\r` and U+2028/U+2029 as line breaks, but the differ does not.
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' if chars.peek() != Some(&'\n') => out.push('␍'),
            '\u{2028}' | '\u{2029}' => out.push('␤'),
            c => out.push(c),
        }
    }
    out
}

/// Diffs two texts line by line. Adjacent insertions/deletions are merged into a single
/// change, so the result strictly alternates between equal and changed blocks.
pub fn diff_lines(old: &str, new: &str) -> Vec<Block> {
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Patience)
        .deadline(Instant::now() + Duration::from_secs(2))
        .diff_lines(old, new);
    let mut blocks = Vec::new();
    for op in diff.ops() {
        if let DiffOp::Equal {
            old_index,
            new_index,
            len,
        } = *op
        {
            blocks.push(Block::Equal {
                old: old_index,
                new: new_index,
                len,
            });
            continue;
        }
        let (o, n) = (op.old_range(), op.new_range());
        match blocks.last_mut() {
            Some(Block::Change(c)) => {
                c.old.end = o.end;
                c.new.end = n.end;
            }
            _ => blocks.push(Block::Change(Chunk {
                kind: ChunkKind::Replace,
                old: o,
                new: n,
            })),
        }
    }
    for b in &mut blocks {
        if let Block::Change(c) = b {
            c.kind = match (c.old.is_empty(), c.new.is_empty()) {
                (true, _) => ChunkKind::Insert,
                (_, true) => ChunkKind::Delete,
                _ => ChunkKind::Replace,
            };
        }
    }
    blocks
}

/// Changed char ranges in the old and the new text.
pub type InlineChanges = (Vec<Range<usize>>, Vec<Range<usize>>);

/// Character ranges (in chars, relative to the start of each text) that differ between
/// two versions of a replaced chunk. Returns `None` when the texts are too different
/// for intra-line highlighting to be useful.
pub fn diff_inline(old: &str, new: &str) -> Option<InlineChanges> {
    if old.len() + new.len() > 20_000 {
        return None;
    }
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Myers)
        .deadline(Instant::now() + Duration::from_millis(50))
        .diff_chars(old, new);
    let ops = diff.ops();

    // Treat tiny islands of equality between two changes as changed, which avoids the
    // noisy "every other letter matches" look of raw char diffs.
    let mut changed: Vec<(Range<usize>, Range<usize>)> = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        let is_change = op.tag() != DiffTag::Equal;
        let is_island =
            op.tag() == DiffTag::Equal && op.old_range().len() <= 2 && i > 0 && i + 1 < ops.len();
        if !is_change && !is_island {
            continue;
        }
        let (o, n) = (op.old_range(), op.new_range());
        match changed.last_mut() {
            Some((lo, ln)) if lo.end == o.start && ln.end == n.start => {
                lo.end = o.end;
                ln.end = n.end;
            }
            _ => changed.push((o, n)),
        }
    }

    let changed_len: usize = changed.iter().map(|(o, n)| o.len() + n.len()).sum();
    let total = diff.old_slices().len() + diff.new_slices().len();
    if changed.is_empty() || changed_len * 10 > total * 7 {
        return None;
    }
    let (o, n) = changed.into_iter().unzip::<_, _, Vec<_>, Vec<_>>();
    Some((
        o.into_iter().filter(|r| !r.is_empty()).collect(),
        n.into_iter().filter(|r| !r.is_empty()).collect(),
    ))
}

/// Vertical geometry of a changed chunk, in pixels from the top of the diff body.
#[derive(Clone, Debug)]
pub struct ChunkGeom {
    pub kind: ChunkKind,
    pub left: (i32, i32),
    pub right: (i32, i32),
}

/// A run of hidden unchanged lines, drawn as a separator band.
#[derive(Clone, Debug)]
pub struct Gap {
    pub y: i32,
    pub height: i32,
    pub hidden: usize,
}

/// One side of the layout.
#[derive(Clone, Debug, Default)]
pub struct SideLayout {
    /// Line ranges to hide.
    pub hidden: Vec<Range<usize>>,
    /// Extra pixels above a line, used to keep both sides aligned.
    pub pad_above: BTreeMap<usize, i32>,
    /// (first line, y) for each run of consecutive visible lines.
    runs: Vec<(usize, i32)>,
}

impl SideLayout {
    fn pad(&mut self, line: usize, pending: &mut i32) {
        if *pending > 0 {
            *self.pad_above.entry(line).or_default() += *pending;
            *pending = 0;
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Layout {
    pub sides: [SideLayout; 2],
    pub chunks: Vec<ChunkGeom>,
    pub gaps: Vec<Gap>,
    pub height: i32,
    line_height: i32,
}

impl Layout {
    /// y of the top of a (visible) line on one side.
    pub fn line_y(&self, side: usize, line: usize) -> i32 {
        let runs = &self.sides[side].runs;
        let i = runs.partition_point(|&(start, _)| start <= line);
        match i.checked_sub(1).map(|i| runs[i]) {
            Some((start, y)) => y + (line - start) as i32 * self.line_height,
            None => 0,
        }
    }
}

/// Computes which lines to hide and how much to pad, so that the two sides line up:
/// every run of unchanged lines starts at the same height on both sides, and each changed
/// chunk occupies the height of its taller side.
pub fn layout(blocks: &[Block], collapsed: bool, lh: i32, gap: i32) -> Layout {
    let mut l = Layout {
        line_height: lh,
        ..Default::default()
    };
    let [old_side, new_side] = &mut l.sides;
    let mut y = 0;
    let (mut pend_old, mut pend_new) = (0, 0);

    for (i, block) in blocks.iter().enumerate() {
        match *block {
            Block::Equal { old, new, len } => {
                // Blocks alternate, so any neighbour is a change.
                let (head, tail) = if collapsed {
                    (
                        if i > 0 { CONTEXT } else { 0 },
                        if i + 1 < blocks.len() { CONTEXT } else { 0 },
                    )
                } else {
                    (len, 0)
                };
                if head + tail >= len {
                    if len > 0 {
                        old_side.pad(old, &mut pend_old);
                        new_side.pad(new, &mut pend_new);
                        old_side.runs.push((old, y));
                        new_side.runs.push((new, y));
                    }
                    y += len as i32 * lh;
                    continue;
                }
                if head > 0 {
                    old_side.pad(old, &mut pend_old);
                    new_side.pad(new, &mut pend_new);
                    old_side.runs.push((old, y));
                    new_side.runs.push((new, y));
                    y += head as i32 * lh;
                }
                let hidden_end = len - tail;
                old_side.hidden.push(old + head..old + hidden_end);
                new_side.hidden.push(new + head..new + hidden_end);
                if tail > 0 {
                    l.gaps.push(Gap {
                        y,
                        height: gap,
                        hidden: hidden_end - head,
                    });
                    pend_old += gap;
                    pend_new += gap;
                    y += gap;
                    old_side.pad(old + hidden_end, &mut pend_old);
                    new_side.pad(new + hidden_end, &mut pend_new);
                    old_side.runs.push((old + hidden_end, y));
                    new_side.runs.push((new + hidden_end, y));
                    y += tail as i32 * lh;
                } else if head > 0 {
                    // Hidden until the end of the file: the band goes below the last visible
                    // line, in the space the link map's height adds below the text.
                    l.gaps.push(Gap {
                        y,
                        height: gap,
                        hidden: hidden_end - head,
                    });
                    y += gap;
                }
            }
            Block::Change(ref c) => {
                let a = c.old.len() as i32 * lh;
                let b = c.new.len() as i32 * lh;
                if a > 0 {
                    old_side.pad(c.old.start, &mut pend_old);
                    old_side.runs.push((c.old.start, y));
                }
                if b > 0 {
                    new_side.pad(c.new.start, &mut pend_new);
                    new_side.runs.push((c.new.start, y));
                }
                l.chunks.push(ChunkGeom {
                    kind: c.kind,
                    left: (y, y + a),
                    right: (y, y + b),
                });
                let m = a.max(b);
                pend_old += m - a;
                pend_new += m - b;
                y += m;
            }
        }
    }
    l.height = y;
    l
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_alternate() {
        let old = "a\nb\nc\nd\n";
        let new = "a\nB\nc\nd\ne\n";
        let blocks = diff_lines(old, new);
        assert!(matches!(blocks[0], Block::Equal { len: 1, .. }));
        assert!(matches!(&blocks[1], Block::Change(c) if c.kind == ChunkKind::Replace));
        assert!(matches!(blocks[2], Block::Equal { len: 2, .. }));
        assert!(matches!(&blocks[3], Block::Change(c) if c.kind == ChunkKind::Insert));
    }

    #[test]
    fn layout_aligns_and_hides() {
        let old: String = (0..20).map(|i| format!("{i}\n")).collect();
        let new = old.replace("10\n", "ten\nTEN\n");
        let blocks = diff_lines(&old, &new);
        let l = layout(&blocks, true, 10, 5);
        // lines 0..7 hidden, 7..10 context, change, 11..14 context, rest hidden
        assert_eq!(l.sides[0].hidden, vec![0..7, 14..20]);
        assert_eq!(l.sides[1].hidden, vec![0..7, 15..21]);
        assert_eq!(l.gaps.len(), 2);
        assert_eq!(l.chunks[0].left, (35, 45));
        assert_eq!(l.chunks[0].right, (35, 55));
        // the old side gets padded after the shorter chunk
        assert_eq!(l.sides[0].pad_above.get(&11), Some(&10));
        assert_eq!(l.line_y(0, 11), 55);
        assert_eq!(l.line_y(1, 12), 55);
        assert_eq!(l.height, 90);
    }

    #[test]
    fn inline_diff_finds_word() {
        let (o, n) = diff_inline("let foo = 1;\n", "let bar = 1;\n").unwrap();
        assert_eq!(o, vec![4..7]);
        assert_eq!(n, vec![4..7]);
    }
}

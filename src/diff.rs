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

/// Space reserved below some lines of one side, for a comment card.
#[derive(Clone, Debug)]
pub struct Note {
    pub side: usize,
    /// The lines it's attached to; they're never hidden, and the note goes below the last.
    pub lines: Range<usize>,
    pub height: i32,
}

impl Note {
    fn last(&self) -> usize {
        self.lines.end.max(self.lines.start + 1) - 1
    }
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
    /// Starts a run of visible lines at `y`, padding its first line by what's pending.
    fn start_run(&mut self, line: usize, y: i32, pending: &mut i32) {
        if *pending > 0 {
            *self.pad_above.entry(line).or_default() += *pending;
            *pending = 0;
        }
        self.runs.push((line, y));
    }
}

#[derive(Clone, Debug, Default)]
pub struct Layout {
    pub sides: [SideLayout; 2],
    pub chunks: Vec<ChunkGeom>,
    pub gaps: Vec<Gap>,
    /// y of the space reserved for each note, `None` if its lines aren't in the file.
    pub notes: Vec<Option<i32>>,
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

    pub fn line_height(&self) -> i32 {
        self.line_height
    }
}

/// Sorts ranges and merges the ones that overlap or touch, dropping empty ones.
fn merge(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.retain(|r| !r.is_empty());
    ranges.sort_by_key(|r| r.start);
    let mut out: Vec<Range<usize>> = Vec::new();
    for r in ranges {
        match out.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => out.push(r),
        }
    }
    out
}

/// Computes which lines to hide and how much to pad, so that the two sides line up:
/// every run of unchanged lines starts at the same height on both sides, and each changed
/// chunk occupies the height of its taller side. Notes get their height reserved below their
/// last line: across both sides among unchanged lines, on their own side within a chunk.
pub fn layout(blocks: &[Block], notes: &[Note], collapsed: bool, lh: i32, gap: i32) -> Layout {
    let mut l = Layout {
        line_height: lh,
        notes: vec![None; notes.len()],
        ..Default::default()
    };
    let mut y = 0;
    let mut pending = [0, 0];

    for (i, block) in blocks.iter().enumerate() {
        match *block {
            Block::Equal { old, new, len } => {
                let starts = [old, new];
                // Offsets within the block that stay visible. Blocks alternate, so any
                // neighbour is a change.
                let mut visible = Vec::new();
                if collapsed {
                    let head = if i > 0 { CONTEXT.min(len) } else { 0 };
                    let tail = if i + 1 < blocks.len() {
                        CONTEXT.min(len)
                    } else {
                        0
                    };
                    visible.extend([0..head, len - tail..len]);
                    for n in notes {
                        let s = starts[n.side];
                        let (a, b) = (n.lines.start.max(s), n.lines.end.min(s + len));
                        if a < b {
                            visible.push(a - s..b - s);
                        }
                    }
                } else {
                    visible.push(0..len);
                }
                // Notes by the offset of the line they go below.
                let mut slots: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
                for (ni, n) in notes.iter().enumerate() {
                    let s = starts[n.side];
                    if (s..s + len).contains(&n.last()) {
                        slots.entry(n.last() - s).or_default().push(ni);
                    }
                }

                let mut pos = 0;
                for r in merge(visible).into_iter().chain([len..len]) {
                    if r.start > pos {
                        for (side, &s) in l.sides.iter_mut().zip(&starts) {
                            side.hidden.push(s + pos..s + r.start);
                        }
                        // When hidden up to the end of the file, the band goes below the last
                        // visible line, in the space the link map's height adds below the text.
                        l.gaps.push(Gap {
                            y,
                            height: gap,
                            hidden: r.start - pos,
                        });
                        pending = pending.map(|p| p + gap);
                        y += gap;
                    }
                    let mut seg = r.start;
                    while seg < r.end {
                        let stop = slots
                            .range(seg..r.end)
                            .next()
                            .map_or(r.end, |(&k, _)| k + 1);
                        for ((side, &s), p) in l.sides.iter_mut().zip(&starts).zip(&mut pending) {
                            side.start_run(s + seg, y, p);
                        }
                        y += (stop - seg) as i32 * lh;
                        for &ni in slots.get(&(stop - 1)).into_iter().flatten() {
                            l.notes[ni] = Some(y);
                            pending = pending.map(|p| p + notes[ni].height);
                            y += notes[ni].height;
                        }
                        seg = stop;
                    }
                    pos = r.end;
                }
            }
            Block::Change(ref c) => {
                let mut heights = [0, 0];
                for (side, range) in [&c.old, &c.new].into_iter().enumerate() {
                    let mut sy = y;
                    let mut seg = range.start;
                    while seg < range.end {
                        let on_side = || {
                            notes.iter().enumerate().filter(|(_, n)| {
                                n.side == side && (seg..range.end).contains(&n.last())
                            })
                        };
                        let stop = on_side()
                            .map(|(_, n)| n.last() + 1)
                            .min()
                            .unwrap_or(range.end);
                        l.sides[side].start_run(seg, sy, &mut pending[side]);
                        sy += (stop - seg) as i32 * lh;
                        for (ni, n) in on_side().filter(|(_, n)| n.last() == stop - 1) {
                            l.notes[ni] = Some(sy);
                            pending[side] += n.height;
                            sy += n.height;
                        }
                        seg = stop;
                    }
                    heights[side] = sy - y;
                }
                let [a, b] = heights;
                l.chunks.push(ChunkGeom {
                    kind: c.kind,
                    left: (y, y + a),
                    right: (y, y + b),
                });
                let m = a.max(b);
                pending[0] += m - a;
                pending[1] += m - b;
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
        let l = layout(&blocks, &[], true, 10, 5);
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

    #[test]
    fn notes_reserve_space_and_stay_visible() {
        let old: String = (0..20).map(|i| format!("{i}\n")).collect();
        let new = old.replace("10\n", "ten\nTEN\n");
        let blocks = diff_lines(&old, &new);
        let notes = [
            // In hidden unchanged lines: shown, space on both sides.
            Note {
                side: 1,
                lines: 1..3,
                height: 7,
            },
            // Inside the change, on the new side only.
            Note {
                side: 1,
                lines: 10..11,
                height: 30,
            },
        ];
        let l = layout(&blocks, &notes, true, 10, 5);
        assert_eq!(l.sides[0].hidden, vec![0..1, 3..7, 14..20]);
        assert_eq!(l.sides[1].hidden, vec![0..1, 3..7, 15..21]);
        // gap, lines 1-2, then the note
        assert_eq!(l.notes[0], Some(25));
        assert_eq!(l.line_y(1, 1), 5);
        // note + gap pad the next visible line
        assert_eq!(l.sides[0].pad_above.get(&7), Some(&12));
        assert_eq!(l.line_y(0, 7), 37);
        // gap(5) + 2 lines + note(7) + gap(5) + 3 context lines = 67; the change starts there
        assert_eq!(l.chunks[0].right, (67, 67 + 10 + 30 + 10));
        assert_eq!(l.notes[1], Some(77));
        assert_eq!(l.line_y(1, 11), 107);
        assert_eq!(l.chunks[0].left, (67, 77));
        assert_eq!(l.line_y(0, 11), l.line_y(1, 12));
    }
}

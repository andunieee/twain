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

/// Vertical geometry of a changed chunk, in pixels from the top of each side's body.
#[derive(Clone, Debug)]
pub struct ChunkGeom {
    pub kind: ChunkKind,
    pub left: (i32, i32),
    pub right: (i32, i32),
}

/// A run of hidden unchanged lines, drawn as a separator band on both sides.
#[derive(Clone, Debug)]
pub struct Gap {
    /// y on each side.
    pub y: [i32; 2],
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
    /// Extra pixels above a line, for the gap bands and notes.
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
    /// y of the space reserved for each note on its side, `None` if its lines aren't in the file.
    pub notes: Vec<Option<i32>>,
    /// Height of each side.
    pub height: [i32; 2],
    /// Pairs of corresponding y positions on the two sides, increasing on both, from the top
    /// to the bottom of both sides. Scrolling one side moves the other by interpolating
    /// between them.
    pub anchors: Vec<[i32; 2]>,
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

    fn anchor(&mut self, y: [i32; 2]) {
        if self.anchors.last() != Some(&y) {
            self.anchors.push(y);
        }
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

/// Lays out each side on its own, one line after the other, like meld: the sides aren't
/// padded to line up, the anchors say which heights correspond. Collapsed files hide the
/// same unchanged lines on both sides, leaving a gap band in their place. Notes get their
/// height reserved below their last line, on their own side.
pub fn layout(blocks: &[Block], notes: &[Note], collapsed: bool, lh: i32, gap: i32) -> Layout {
    let mut l = Layout {
        line_height: lh,
        notes: vec![None; notes.len()],
        anchors: vec![[0, 0]],
        ..Default::default()
    };
    let mut y = [0, 0];
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
                        // visible line, in the space the body's height adds below the text.
                        l.anchor(y);
                        l.gaps.push(Gap {
                            y,
                            height: gap,
                            hidden: r.start - pos,
                        });
                        pending = pending.map(|p| p + gap);
                        y = y.map(|y| y + gap);
                    }
                    let mut seg = r.start;
                    while seg < r.end {
                        let stop = slots
                            .range(seg..r.end)
                            .next()
                            .map_or(r.end, |(&k, _)| k + 1);
                        for (side, (sl, &s)) in l.sides.iter_mut().zip(&starts).enumerate() {
                            sl.start_run(s + seg, y[side], &mut pending[side]);
                        }
                        l.anchor(y);
                        y = y.map(|y| y + (stop - seg) as i32 * lh);
                        l.anchor(y);
                        for &ni in slots.get(&(stop - 1)).into_iter().flatten() {
                            let side = notes[ni].side;
                            l.notes[ni] = Some(y[side]);
                            pending[side] += notes[ni].height;
                            y[side] += notes[ni].height;
                        }
                        seg = stop;
                    }
                    pos = r.end;
                }
            }
            Block::Change(ref c) => {
                l.anchor(y);
                let top = y;
                for (side, range) in [&c.old, &c.new].into_iter().enumerate() {
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
                        l.sides[side].start_run(seg, y[side], &mut pending[side]);
                        y[side] += (stop - seg) as i32 * lh;
                        for (ni, n) in on_side().filter(|(_, n)| n.last() == stop - 1) {
                            l.notes[ni] = Some(y[side]);
                            pending[side] += n.height;
                            y[side] += n.height;
                        }
                        seg = stop;
                    }
                }
                l.chunks.push(ChunkGeom {
                    kind: c.kind,
                    left: (top[0], y[0]),
                    right: (top[1], y[1]),
                });
                l.anchor(y);
            }
        }
    }
    l.anchor(y);
    l.height = y;
    l
}

/// Maps a y position on side `from` to the other side, interpolating between the anchors
/// around it (meld's scroll syncing). Past either end, the sides move together.
pub fn map_y(anchors: &[[f64; 2]], from: usize, y: f64) -> f64 {
    let to = 1 - from;
    let i = anchors.partition_point(|a| a[from] < y);
    match (i.checked_sub(1).map(|i| anchors[i]), anchors.get(i).copied()) {
        (Some(a), Some(b)) => {
            let span = b[from] - a[from];
            let t = if span > 0.0 { (y - a[from]) / span } else { 1.0 };
            a[to] + t * (b[to] - a[to])
        }
        (None, Some(a)) | (Some(a), None) => a[to] + y - a[from],
        (None, None) => y,
    }
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
    fn layout_hides_without_padding() {
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
        // each side just continues after the chunk
        assert!(l.sides[0].pad_above.get(&11).is_none());
        assert_eq!(l.line_y(0, 11), 45);
        assert_eq!(l.line_y(1, 12), 55);
        assert_eq!(l.gaps[1].y, [75, 85]);
        assert_eq!(l.height, [80, 90]);
        assert_eq!(l.anchors.first(), Some(&[0, 0]));
        assert_eq!(l.anchors.last(), Some(&[80, 90]));
        assert!(l.anchors.contains(&[35, 35]) && l.anchors.contains(&[45, 55]));
        assert!(l.anchors.windows(2).all(|w| w[0][0] <= w[1][0] && w[0][1] <= w[1][1]));
    }

    #[test]
    fn map_y_interpolates_between_anchors() {
        // an insertion of 20px on the right at y = 10
        let anchors = [[0.0, 0.0], [10.0, 10.0], [10.0, 30.0], [20.0, 40.0]];
        assert_eq!(map_y(&anchors, 0, 5.0), 5.0);
        assert_eq!(map_y(&anchors, 0, 10.0), 10.0);
        assert_eq!(map_y(&anchors, 0, 15.0), 35.0);
        assert_eq!(map_y(&anchors, 1, 20.0), 10.0);
        assert_eq!(map_y(&anchors, 1, 35.0), 15.0);
        assert_eq!(map_y(&anchors, 0, 25.0), 45.0);
        assert_eq!(map_y(&anchors, 0, -5.0), -5.0);
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
        // gap, lines 1-2, then the note, on the new side only
        assert_eq!(l.notes[0], Some(25));
        assert_eq!(l.line_y(1, 1), 5);
        // note + gap pad the next visible line on the new side, the gap alone on the old
        assert_eq!(l.sides[0].pad_above.get(&7), Some(&5));
        assert_eq!(l.sides[1].pad_above.get(&7), Some(&12));
        assert_eq!(l.line_y(0, 7), 30);
        assert_eq!(l.line_y(1, 7), 37);
        assert!(l.anchors.contains(&[25, 25]) && l.anchors.contains(&[30, 37]));
        assert_eq!(l.chunks[0].left, (60, 70));
        assert_eq!(l.chunks[0].right, (67, 67 + 10 + 30 + 10));
        assert_eq!(l.notes[1], Some(77));
        assert_eq!(l.line_y(1, 11), 107);
        assert_eq!(l.line_y(0, 11), 70);
        assert_eq!(l.line_y(1, 12), 117);
    }
}

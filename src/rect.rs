// rectangles and regions. rects are x1/y1/x2/y2, i32, half-open. regions are
// normalized band lists: sorted by y then x, rects sharing a y1 share a y2,
// x-spans within a band disjoint, identical adjacent bands merged. the
// strip-based normalization trades the classic band-merge sweep for something
// shorter; compositor regions stay tiny.

use std::rc::Rc;

// -- rect --

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}

impl Rect {
    pub fn new(x1: i32, y1: i32, x2: i32, y2: i32) -> Option<Rect> {
        if x2 < x1 || y2 < y1 {
            return None;
        }
        Some(Rect { x1, y1, x2, y2 })
    }

    pub fn new_sized(x: i32, y: i32, w: i32, h: i32) -> Option<Rect> {
        if w < 0 || h < 0 {
            return None;
        }
        Some(Rect {
            x1: x,
            y1: y,
            x2: x.checked_add(w)?,
            y2: y.checked_add(h)?,
        })
    }

    pub fn new_sized_saturating(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect {
            x1: x,
            y1: y,
            x2: x.saturating_add(w.max(0)),
            y2: y.saturating_add(h.max(0)),
        }
    }

    pub fn width(self) -> i32 {
        self.x2 - self.x1
    }

    pub fn height(self) -> i32 {
        self.y2 - self.y1
    }

    pub fn is_empty(self) -> bool {
        self.x1 >= self.x2 || self.y1 >= self.y2
    }

    pub fn contains(self, x: i32, y: i32) -> bool {
        self.x1 <= x && self.y1 <= y && self.x2 > x && self.y2 > y
    }

    pub fn intersects(self, other: Rect) -> bool {
        self.x1 < other.x2 && other.x1 < self.x2 && self.y1 < other.y2 && other.y1 < self.y2
    }

    /// bounding box
    pub fn union(self, other: Rect) -> Rect {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        Rect {
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
            x2: self.x2.max(other.x2),
            y2: self.y2.max(other.y2),
        }
    }

    pub fn intersect(self, other: Rect) -> Rect {
        let r = Rect {
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
            x2: self.x2.min(other.x2),
            y2: self.y2.min(other.y2),
        };
        if r.is_empty() {
            Rect::default()
        } else {
            r
        }
    }

    pub fn move_(self, dx: i32, dy: i32) -> Rect {
        Rect {
            x1: self.x1.saturating_add(dx),
            y1: self.y1.saturating_add(dy),
            x2: self.x2.saturating_add(dx),
            y2: self.y2.saturating_add(dy),
        }
    }
}

// -- region --

pub struct Region {
    rects: Vec<Rect>,
    extents: Rect,
}

impl Region {
    pub fn empty() -> Rc<Region> {
        thread_local! {
            static EMPTY: Rc<Region> = Rc::new(Region {
                rects: Vec::new(),
                extents: Rect::default(),
            });
        }
        EMPTY.with(|e| e.clone())
    }

    pub fn from_rects(rects: &[Rect]) -> Rc<Region> {
        let (rects, extents) = normalize(rects.to_vec());
        if rects.is_empty() {
            return Region::empty();
        }
        Rc::new(Region { rects, extents })
    }

    pub fn union(&self, other: &Region) -> Rc<Region> {
        if self.rects.is_empty() {
            return Region::from_rects(&other.rects);
        }
        if other.rects.is_empty() {
            return Region::from_rects(&self.rects);
        }
        let mut all = self.rects.clone();
        all.extend_from_slice(&other.rects);
        let (rects, extents) = normalize(all);
        Rc::new(Region { rects, extents })
    }

    pub fn subtract(&self, other: &Region) -> Rc<Region> {
        if self.rects.is_empty() || other.rects.is_empty() || !self.extents.intersects(other.extents)
        {
            return Region::from_rects(&self.rects);
        }
        let (rects, extents) = subtract(&self.rects, &other.rects);
        if rects.is_empty() {
            return Region::empty();
        }
        Rc::new(Region { rects, extents })
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        if !self.extents.contains(x, y) {
            return false;
        }
        self.rects.iter().any(|r| r.contains(x, y))
    }

    pub fn extents(&self) -> Rect {
        self.extents
    }

    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    #[allow(dead_code)]
    pub fn rects(&self) -> &[Rect] {
        &self.rects
    }
}

/// slice into y-strips, merge x-spans per strip, merge identical adjacent bands
fn normalize(mut rects: Vec<Rect>) -> (Vec<Rect>, Rect) {
    rects.retain(|r| !r.is_empty());
    if rects.is_empty() {
        return (Vec::new(), Rect::default());
    }
    let mut ys: Vec<i32> = rects.iter().flat_map(|r| [r.y1, r.y2]).collect();
    ys.sort_unstable();
    ys.dedup();
    let mut out: Vec<Rect> = Vec::new();
    let mut prev: Option<(usize, usize)> = None;
    for w in ys.windows(2) {
        let (y1, y2) = (w[0], w[1]);
        let mut xs: Vec<(i32, i32)> = rects
            .iter()
            .filter(|r| r.y1 <= y1 && r.y2 >= y2)
            .map(|r| (r.x1, r.x2))
            .collect();
        if xs.is_empty() {
            prev = None;
            continue;
        }
        xs.sort_unstable();
        let mut merged: Vec<(i32, i32)> = Vec::new();
        for (a, b) in xs {
            if let Some(last) = merged.last_mut() {
                if a <= last.1 {
                    last.1 = last.1.max(b);
                    continue;
                }
            }
            merged.push((a, b));
        }
        prev = push_band(&mut out, prev, y1, y2, &merged);
    }
    let extents = bounds(&out);
    (out, extents)
}

fn subtract(a: &[Rect], b: &[Rect]) -> (Vec<Rect>, Rect) {
    let mut ys: Vec<i32> = a
        .iter()
        .chain(b.iter())
        .flat_map(|r| [r.y1, r.y2])
        .collect();
    ys.sort_unstable();
    ys.dedup();
    let mut out: Vec<Rect> = Vec::new();
    let mut prev: Option<(usize, usize)> = None;
    for w in ys.windows(2) {
        let (y1, y2) = (w[0], w[1]);
        let spans_a = strip_spans(a, y1, y2);
        if spans_a.is_empty() {
            prev = None;
            continue;
        }
        let spans_b = strip_spans(b, y1, y2);
        let mut result: Vec<(i32, i32)> = Vec::new();
        for (mut a1, a2) in spans_a {
            for &(b1, b2) in &spans_b {
                if b2 <= a1 {
                    continue;
                }
                if b1 >= a2 {
                    break;
                }
                if b1 > a1 {
                    result.push((a1, b1));
                }
                a1 = a1.max(b2);
                if a1 >= a2 {
                    break;
                }
            }
            if a1 < a2 {
                result.push((a1, a2));
            }
        }
        if result.is_empty() {
            prev = None;
            continue;
        }
        prev = push_band(&mut out, prev, y1, y2, &result);
    }
    let extents = bounds(&out);
    (out, extents)
}

/// merged x-spans of all rects covering the strip [y1, y2)
fn strip_spans(rects: &[Rect], y1: i32, y2: i32) -> Vec<(i32, i32)> {
    let mut xs: Vec<(i32, i32)> = rects
        .iter()
        .filter(|r| r.y1 <= y1 && r.y2 >= y2 && !r.is_empty())
        .map(|r| (r.x1, r.x2))
        .collect();
    xs.sort_unstable();
    let mut merged: Vec<(i32, i32)> = Vec::new();
    for (a, b) in xs {
        if let Some(last) = merged.last_mut() {
            if a <= last.1 {
                last.1 = last.1.max(b);
                continue;
            }
        }
        merged.push((a, b));
    }
    merged
}

fn push_band(
    out: &mut Vec<Rect>,
    prev: Option<(usize, usize)>,
    y1: i32,
    y2: i32,
    spans: &[(i32, i32)],
) -> Option<(usize, usize)> {
    if let Some((ps, pe)) = prev {
        let same = pe - ps == spans.len()
            && out[ps].y2 == y1
            && out[ps..pe]
                .iter()
                .zip(spans)
                .all(|(r, s)| r.x1 == s.0 && r.x2 == s.1);
        if same {
            for r in &mut out[ps..pe] {
                r.y2 = y2;
            }
            return Some((ps, pe));
        }
    }
    let s = out.len();
    for &(a, b) in spans {
        out.push(Rect {
            x1: a,
            y1,
            x2: b,
            y2,
        });
    }
    Some((s, out.len()))
}

fn bounds(rects: &[Rect]) -> Rect {
    let mut it = rects.iter();
    let Some(first) = it.next() else {
        return Rect::default();
    };
    it.fold(*first, |acc, r| acc.union(*r))
}

// -- builder --

/// batches consecutive same-op rects so n adds cost one normalization
pub struct RegionBuilder {
    base: Rc<Region>,
    subtracting: bool,
    pending: Vec<Rect>,
}

impl Default for RegionBuilder {
    fn default() -> Self {
        RegionBuilder {
            base: Region::empty(),
            subtracting: false,
            pending: Vec::new(),
        }
    }
}

impl RegionBuilder {
    pub fn add(&mut self, r: Rect) {
        if self.subtracting {
            self.flush();
            self.subtracting = false;
        }
        self.pending.push(r);
    }

    pub fn sub(&mut self, r: Rect) {
        if !self.subtracting {
            self.flush();
            self.subtracting = true;
        }
        self.pending.push(r);
    }

    fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let batch = Region::from_rects(&self.pending);
        self.pending.clear();
        self.base = if self.subtracting {
            self.base.subtract(&batch)
        } else {
            self.base.union(&batch)
        };
    }

    pub fn get(&mut self) -> Rc<Region> {
        self.flush();
        self.base.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x1: i32, y1: i32, x2: i32, y2: i32) -> Rect {
        Rect { x1, y1, x2, y2 }
    }

    #[test]
    fn normalize_merges_bands() {
        // two horizontally adjacent rects fuse into one
        let reg = Region::from_rects(&[r(0, 0, 5, 5), r(5, 0, 10, 5)]);
        assert_eq!(reg.rects(), &[r(0, 0, 10, 5)]);
        // vertically identical bands fuse too
        let reg = Region::from_rects(&[r(0, 0, 10, 5), r(0, 5, 10, 10)]);
        assert_eq!(reg.rects(), &[r(0, 0, 10, 10)]);
    }

    #[test]
    fn normalize_overlap() {
        let reg = Region::from_rects(&[r(0, 0, 6, 6), r(3, 3, 9, 9)]);
        assert_eq!(
            reg.rects(),
            &[r(0, 0, 6, 3), r(0, 3, 9, 6), r(3, 6, 9, 9)]
        );
        assert_eq!(reg.extents(), r(0, 0, 9, 9));
    }

    #[test]
    fn union_of_disjoint() {
        let a = Region::from_rects(&[r(0, 0, 2, 2)]);
        let b = Region::from_rects(&[r(10, 10, 12, 12)]);
        let u = a.union(&b);
        assert_eq!(u.rects().len(), 2);
        assert!(u.contains(1, 1));
        assert!(u.contains(11, 11));
        assert!(!u.contains(5, 5));
    }

    #[test]
    fn subtract_punches_a_hole() {
        let a = Region::from_rects(&[r(0, 0, 10, 10)]);
        let b = Region::from_rects(&[r(3, 3, 7, 7)]);
        let s = a.subtract(&b);
        assert!(s.contains(0, 0));
        assert!(s.contains(9, 9));
        assert!(s.contains(2, 5));
        assert!(s.contains(7, 5));
        assert!(!s.contains(3, 3));
        assert!(!s.contains(6, 6));
        assert_eq!(s.extents(), r(0, 0, 10, 10));
        // punching it back out empties the region
        let hole = Region::from_rects(&[r(0, 0, 10, 10)]);
        assert!(s.subtract(&hole).is_empty());
    }

    #[test]
    fn subtract_edge_overlap() {
        let a = Region::from_rects(&[r(0, 0, 10, 10)]);
        let b = Region::from_rects(&[r(5, 0, 15, 10)]);
        let s = a.subtract(&b);
        assert_eq!(s.rects(), &[r(0, 0, 5, 10)]);
    }

    #[test]
    fn builder_batches_and_snapshots() {
        let mut b = RegionBuilder::default();
        b.add(r(0, 0, 10, 10));
        b.sub(r(2, 2, 4, 4));
        let snap = b.get();
        assert!(!snap.contains(3, 3));
        assert!(snap.contains(5, 5));
        // later mutation must not affect the snapshot
        b.add(r(2, 2, 4, 4));
        assert!(!snap.contains(3, 3));
        assert!(b.get().contains(3, 3));
    }

    #[test]
    fn contains_half_open() {
        let reg = Region::from_rects(&[r(0, 0, 10, 10)]);
        assert!(reg.contains(0, 0));
        assert!(reg.contains(9, 9));
        assert!(!reg.contains(10, 10));
        assert!(!reg.contains(-1, 0));
    }

    #[test]
    fn empty_rects_dropped() {
        let reg = Region::from_rects(&[r(5, 5, 5, 9), r(1, 1, 0, 0)]);
        assert!(reg.is_empty());
    }
}

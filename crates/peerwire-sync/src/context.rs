//! Ranges of dots, per writer: doc.js's `Context`. A writer's ranges are sorted, disjoint and never adjacent
//! ([1, 4] and [5, 9] are kept as [1, 9]).

use crate::ordmap::OrdMap;

/// A writer: a PeerId and a random incarnation, 36 bytes (72 hex characters on the wire). Bytes order as the
/// lowercase hex does, so comparing them compares the writers as doc.js does.
pub(crate) type Writer = [u8; 36];

/// The index of the first range in `list` that ends at or after `n`.
pub(crate) fn seek(list: &[(u64, u64)], n: u64) -> usize {
    list.partition_point(|r| r.1 < n)
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Context {
    pub(crate) writers: OrdMap<Writer, Vec<(u64, u64)>>,
    /// Ranges in all.
    pub(crate) size: usize,
}

impl Context {
    pub(crate) fn has(&self, w: &Writer, n: u64) -> bool {
        let Some(l) = self.writers.get(w) else { return false };
        let i = seek(l, n);
        i < l.len() && l[i].0 <= n
    }

    /// Whether every dot from a to b has been seen.
    pub(crate) fn covers(&self, w: &Writer, a: u64, b: u64) -> bool {
        let Some(l) = self.writers.get(w) else { return false };
        let i = seek(l, a);
        i < l.len() && l[i].0 <= a && l[i].1 >= b
    }

    /// Whether adding [a, b] would join a range of w's rather than start one.
    pub(crate) fn touches(&self, w: &Writer, a: u64, b: u64) -> bool {
        let Some(l) = self.writers.get(w) else { return false };
        let i = seek(l, a - 1);
        i < l.len() && l[i].0 <= b + 1
    }

    pub(crate) fn add(&mut self, w: &Writer, a: u64, b: u64) {
        let l = self.writers.get_or_insert_with(w, Vec::new);
        let i = seek(l, a - 1);
        let mut j = i;
        let (mut lo, mut hi) = (a, b);
        while j < l.len() && l[j].0 <= b + 1 {
            lo = lo.min(l[j].0);
            hi = hi.max(l[j].1);
            j += 1;
        }
        l.splice(i..j, [(lo, hi)]);
        self.size = self.size + 1 - (j - i);
    }

    pub(crate) fn drop_writer(&mut self, w: &Writer) {
        if let Some(l) = self.writers.remove(w) {
            self.size -= l.len();
        }
    }

    /// The first of w's dots from n on that hasn't been seen.
    pub(crate) fn next(&self, w: &Writer, n: u64) -> u64 {
        let Some(l) = self.writers.get(w) else { return n };
        let i = seek(l, n);
        if i < l.len() && l[i].0 <= n {
            l[i].1 + 1
        } else {
            n
        }
    }

    /// A context of `ranges` ([w, a, b] in any order), added in order so each lands at the end of its writer's.
    pub(crate) fn of(ranges: &[(Writer, u64, u64)]) -> Context {
        let mut sorted = ranges.to_vec();
        sorted.sort_by(|x, y| x.0.cmp(&y.0).then(x.1.cmp(&y.1)));
        let mut c = Context::default();
        for (w, a, b) in sorted {
            c.add(&w, a, b);
        }
        c
    }

    /// Drops w's highest range.
    pub(crate) fn drop_last(&mut self, w: &Writer) {
        let Some(l) = self.writers.get_mut(w) else { return };
        l.pop();
        self.size -= 1;
        if l.is_empty() {
            self.writers.remove(w);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_merge_and_answer_as_doc_js() {
        let w = [1u8; 36];
        let mut c = Context::default();
        c.add(&w, 5, 9);
        c.add(&w, 1, 3);
        assert_eq!(c.size, 2);
        c.add(&w, 4, 4); // joins both
        assert_eq!((c.size, c.writers.get(&w).unwrap().clone()), (1, vec![(1, 9)]));
        c.add(&w, 20, 30);
        assert!(c.has(&w, 9) && !c.has(&w, 10) && c.has(&w, 20));
        assert!(c.covers(&w, 2, 9) && !c.covers(&w, 2, 10));
        assert!(c.touches(&w, 10, 12) && c.touches(&w, 15, 19) && !c.touches(&w, 12, 18));
        assert_eq!((c.next(&w, 1), c.next(&w, 10), c.next(&w, 25)), (10, 10, 31));
        c.drop_last(&w);
        assert_eq!(c.writers.get(&w).unwrap().as_slice(), &[(1, 9)]);
        let o = Context::of(&[([2; 36], 3, 3), (w, 7, 8), ([2; 36], 1, 1), (w, 1, 2)]);
        assert_eq!(o.writers.keys().copied().collect::<Vec<_>>(), [w, [2; 36]]);
        assert_eq!(o.size, 4);
    }
}

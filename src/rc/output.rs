//! Where the range encoders write their bytes.
//!
//! The encoders emit one byte per normalization step, and the 7z coder's
//! `ShiftLow` emits runs: a cached byte followed by any number of `0xFF`
//! bytes, all plus a carry. [`RangeOutput`] takes both, so a run is one
//! call with a count rather than a loop of stores.
//!
//! Two sinks:
//!
//! - `Vec<u8>`, which grows as needed (whole-buffer encodes inside the
//!   crate and its tests);
//! - [`Drain`], the step encoders' sink: the caller's output slice, and
//!   behind it a [`Pending`] queue for what one symbol emits past the
//!   slice's end. Every byte either sink receives is settled (a carry can
//!   no longer change it), so the caller may hand off whatever a call
//!   produced at once.

use crate::params::MAX_INPUT_PER_SYMBOL;

/// The byte sink a range encoder writes to. Never fails: a sink that has
/// no room keeps the bytes for later ([`Drain`]) or grows (`Vec`).
pub trait RangeOutput {
    /// Appends one byte.
    fn write_byte(&mut self, byte: u8);

    /// Appends `first`, then `count` copies of `fill`.
    #[inline]
    fn write_run(&mut self, first: u8, fill: u8, count: u64) {
        self.write_byte(first);
        for _ in 0..count {
            self.write_byte(fill);
        }
    }
}

impl RangeOutput for alloc_crate::vec::Vec<u8> {
    #[inline(always)]
    fn write_byte(&mut self, byte: u8) {
        self.push(byte);
    }

    fn write_run(&mut self, first: u8, fill: u8, count: u64) {
        self.push(first);
        let count = usize::try_from(count).unwrap_or(usize::MAX);
        self.resize(self.len().saturating_add(count), fill);
    }
}

impl<O: RangeOutput + ?Sized> RangeOutput for &mut O {
    #[inline(always)]
    fn write_byte(&mut self, byte: u8) {
        (**self).write_byte(byte);
    }

    #[inline]
    fn write_run(&mut self, first: u8, fill: u8, count: u64) {
        (**self).write_run(first, fill, count);
    }
}

/// Bytes one step emitted past the end of the caller's output, in order:
/// an optional head byte, a run of one fill byte, then a short tail.
///
/// A step (one symbol, the end marker, or the flush) starts only with an
/// empty queue, so at most one long run can land here per step: the first
/// emission's, which carries the `0xFF` bytes the 7z coder held back before
/// the step began. Everything the step emits after that is bounded by its
/// normalization count: at most two bytes per coder operation for the 7z
/// coder, four for the carry-less coder, and at most `order + 2` operations
/// per symbol, so [`MAX_INPUT_PER_SYMBOL`] covers it with room to spare.
/// The queue is therefore O(1) state, never a growing buffer.
#[derive(Clone, Debug)]
pub struct Pending {
    head: Option<u8>,
    fill: u8,
    run: u64,
    tail: [u8; TAIL],
    tail_start: usize,
    tail_len: usize,
    overflowed: bool,
}

const TAIL: usize = 2 * MAX_INPUT_PER_SYMBOL;

impl Default for Pending {
    fn default() -> Self {
        Self::new()
    }
}

impl Pending {
    /// An empty queue.
    pub const fn new() -> Self {
        Self {
            head: None,
            fill: 0,
            run: 0,
            tail: [0; TAIL],
            tail_start: 0,
            tail_len: 0,
            overflowed: false,
        }
    }

    /// Whether nothing is queued.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.head.is_none() && self.run == 0 && self.tail_start == self.tail_len
    }

    /// Bytes queued.
    pub fn len(&self) -> u64 {
        u64::from(self.head.is_some()) + self.run + (self.tail_len - self.tail_start) as u64
    }

    /// Whether a step emitted more than the tail can hold, which the bound
    /// above rules out. Reported as an error, never a panic.
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Empties the queue.
    pub fn clear(&mut self) {
        self.head = None;
        self.run = 0;
        self.tail_start = 0;
        self.tail_len = 0;
        self.overflowed = false;
    }

    #[inline]
    fn push_tail(&mut self, byte: u8) {
        if let Some(slot) = self.tail.get_mut(self.tail_len) {
            *slot = byte;
            self.tail_len += 1;
        } else {
            self.overflowed = true;
        }
    }

    /// Moves queued bytes into `out`; returns how many.
    pub fn drain_into(&mut self, out: &mut [u8]) -> usize {
        let mut n = 0;
        if let Some(head) = self.head {
            let Some(slot) = out.first_mut() else {
                return 0;
            };
            *slot = head;
            self.head = None;
            n = 1;
        }
        if self.run != 0 {
            let room = out.len() - n;
            let take = usize::try_from(self.run).map_or(room, |run| run.min(room));
            out[n..n + take].fill(self.fill);
            self.run -= take as u64;
            n += take;
            if self.run != 0 {
                return n;
            }
        }
        let queued = &self.tail[self.tail_start..self.tail_len];
        let take = queued.len().min(out.len() - n);
        out[n..n + take].copy_from_slice(&queued[..take]);
        self.tail_start += take;
        if self.tail_start == self.tail_len {
            self.tail_start = 0;
            self.tail_len = 0;
        }
        n + take
    }
}

/// The step encoders' sink: the caller's slice first, the [`Pending`] queue
/// once the slice is full.
///
/// The per-byte path is one comparison against `end`, which drops to the
/// write position as soon as anything is queued so that later bytes queue
/// behind it and order is kept.
pub struct Drain<'o, 'q> {
    out: &'o mut [u8],
    pos: usize,
    end: usize,
    queue: &'q mut Pending,
}

impl<'o, 'q> Drain<'o, 'q> {
    /// A sink over `out`, queueing into `queue`, which must be empty.
    pub fn new(out: &'o mut [u8], queue: &'q mut Pending) -> Self {
        debug_assert!(queue.is_empty());
        let end = out.len();
        Self {
            out,
            pos: 0,
            end,
            queue,
        }
    }

    /// Bytes written into the caller's slice.
    #[inline]
    pub fn written(&self) -> usize {
        self.pos
    }

    /// Whether the slice is full or anything is queued: the encoder starts
    /// no further step until the queue drains.
    #[inline]
    pub fn blocked(&self) -> bool {
        self.pos >= self.end
    }

    #[cold]
    #[inline(never)]
    fn queue_byte(&mut self, byte: u8) {
        self.end = self.pos;
        self.queue.push_tail(byte);
    }
}

impl RangeOutput for Drain<'_, '_> {
    #[inline(always)]
    fn write_byte(&mut self, byte: u8) {
        if self.pos < self.end {
            // `end <= out.len()`, so this never fails; `get_mut` keeps the
            // path free of a panic branch.
            if let Some(slot) = self.out.get_mut(self.pos) {
                *slot = byte;
                self.pos += 1;
                return;
            }
        }
        self.queue_byte(byte);
    }

    fn write_run(&mut self, first: u8, fill: u8, count: u64) {
        if self.pos >= self.end {
            if self.queue.is_empty() {
                self.end = self.pos;
                self.queue.head = Some(first);
                self.queue.fill = fill;
                self.queue.run = count;
                return;
            }
            self.queue.push_tail(first);
            for _ in 0..count.min(TAIL as u64 + 1) {
                self.queue.push_tail(fill);
            }
            return;
        }
        self.write_byte(first);
        let room = (self.end - self.pos) as u64;
        let direct = count.min(room) as usize;
        self.out[self.pos..self.pos + direct].fill(fill);
        self.pos += direct;
        let rest = count - direct as u64;
        if rest != 0 {
            self.end = self.pos;
            self.queue.fill = fill;
            self.queue.run = rest;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain_all(queue: &mut Pending) -> alloc_crate::vec::Vec<u8> {
        let mut all = alloc_crate::vec::Vec::new();
        let mut buf = [0u8; 3];
        while !queue.is_empty() {
            let n = queue.drain_into(&mut buf);
            all.extend_from_slice(&buf[..n]);
        }
        all
    }

    /// Whatever the slice size, the bytes come out in order.
    #[test]
    fn a_drain_keeps_order_across_the_slice_edge() {
        for room in 0..12 {
            let mut queue = Pending::new();
            let mut out = alloc_crate::vec![0u8; room];
            let mut sink = Drain::new(&mut out, &mut queue);
            sink.write_byte(1);
            sink.write_run(2, 0xFF, 5);
            sink.write_byte(3);
            sink.write_run(4, 0, 1);
            let n = sink.written();
            let mut got = out[..n].to_vec();
            got.extend(drain_all(&mut queue));
            assert_eq!(
                got,
                [1, 2, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 3, 4, 0],
                "room {room}"
            );
        }
    }

    #[test]
    fn a_long_run_is_a_count_not_a_buffer() {
        let mut queue = Pending::new();
        let mut out = [0u8; 2];
        let mut sink = Drain::new(&mut out, &mut queue);
        sink.write_run(9, 0xFF, 1 << 40);
        assert_eq!(sink.written(), 2);
        assert_eq!(queue.len(), (1 << 40) - 1);
        assert!(!queue.overflowed());
        let mut vec = alloc_crate::vec::Vec::new();
        vec.write_run(1, 2, 3);
        assert_eq!(vec, [1, 2, 2, 2]);
    }
}

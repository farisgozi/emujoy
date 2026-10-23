//! The event scheduler: the core's single source of time.
//!
//! Pattern (not code) borrowed from how accurate GBA cores are known to
//! work: one global cycle counter, subsystems book events at absolute
//! timestamps, the CPU loop advances to the next event instead of every
//! subsystem privately counting down. PS1 support later reuses this
//! exact type — only the `EventKind` tags differ per machine, which is
//! why kinds are a small closed enum here plus a `Subsystem` escape
//! hatch carrying an opaque tag.

use alloc::collections::BinaryHeap;
use alloc::vec::Vec;
use core::cmp::Ordering;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    VBlank,
    HBlank,
    TimerOverflow(u8),
    Dma(u8),
    AudioFifo(u8),
    /// Escape hatch for subsystems that do not exist yet.
    Subsystem(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Event {
    at: u64,
    seq: u64,
    kind: EventKind,
}

// BinaryHeap is a max-heap; reverse on `at` (then `seq`) so the earliest
// event — and for equal timestamps the earliest booked — pops first.
impl Ord for Event {
    fn cmp(&self, other: &Self) -> Ordering {
        other.at.cmp(&self.at).then_with(|| other.seq.cmp(&self.seq))
    }
}
impl PartialOrd for Event {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, Default)]
pub struct Scheduler {
    now: u64,
    next_seq: u64,
    heap: BinaryHeap<Event>,
}

impl Scheduler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn now(&self) -> u64 {
        self.now
    }

    /// Book `kind` to fire `delay` cycles from now.
    pub fn schedule(&mut self, delay: u64, kind: EventKind) {
        let at = self.now.saturating_add(delay);
        let seq = self.next_seq;
        self.next_seq += 1;
        self.heap.push(Event { at, seq, kind });
    }

    pub fn next_event_time(&self) -> Option<u64> {
        self.heap.peek().map(|e| e.at)
    }

    /// Move the clock forward. Never backwards; a request to do so is a
    /// bug in the caller and is clamped, not panicked on.
    pub fn advance_to(&mut self, t: u64) {
        if t > self.now {
            self.now = t;
        }
    }

    /// Pop the next event if its time has arrived.
    pub fn pop_due(&mut self) -> Option<EventKind> {
        match self.heap.peek() {
            Some(e) if e.at <= self.now => self.heap.pop().map(|e| e.kind),
            _ => None,
        }
    }

    pub fn pending(&self) -> usize {
        self.heap.len()
    }

    /// Snapshot the queue for a save state: (time, kind) pairs in the
    /// order they will fire. Sequence numbers are re-assigned on load.
    pub fn snapshot(&self) -> Vec<(u64, EventKind)> {
        let mut events: Vec<Event> = self.heap.clone().into_vec();
        events.sort_by_key(|e| (e.at, e.seq));
        events.into_iter().map(|e| (e.at, e.kind)).collect()
    }

    pub fn restore(&mut self, now: u64, events: &[(u64, EventKind)]) {
        self.now = now;
        self.heap.clear();
        self.next_seq = 0;
        for &(at, kind) in events {
            let seq = self.next_seq;
            self.next_seq += 1;
            self.heap.push(Event { at, seq, kind });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_fire_in_time_then_booking_order() {
        let mut s = Scheduler::new();
        s.schedule(100, EventKind::HBlank);
        s.schedule(50, EventKind::VBlank);
        s.schedule(50, EventKind::TimerOverflow(0));
        s.advance_to(50);
        assert_eq!(s.pop_due(), Some(EventKind::VBlank));
        assert_eq!(s.pop_due(), Some(EventKind::TimerOverflow(0)));
        assert_eq!(s.pop_due(), None);
        s.advance_to(100);
        assert_eq!(s.pop_due(), Some(EventKind::HBlank));
    }

    #[test]
    fn snapshot_restore_roundtrip() {
        let mut s = Scheduler::new();
        s.schedule(10, EventKind::Dma(1));
        s.schedule(20, EventKind::VBlank);
        let snap = s.snapshot();
        let mut restored = Scheduler::new();
        restored.restore(0, &snap);
        assert_eq!(restored.snapshot(), snap);
    }
}

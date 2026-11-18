//! What a platform's ESP-NOW gives the radio ([`Ether`]), and two parts of it every platform needs, sans-IO: frames
//! sent in order with retries ([`TxQueue`]), and ESP-NOW's peer table kept within its size ([`PeerTable`]).

use super::{Mac, BROADCAST};
use std::collections::VecDeque;
use std::task::Waker;
use std::time::{Duration, Instant};

/// ESP-NOW as a platform has it: frames to one MAC address or to all, and the frames heard. It never blocks.
pub trait Ether: Send {
    /// Queues `frame` for `to` ([`BROADCAST`] for everyone in range), to be tried up to `tries` times while the
    /// receiving radio doesn't acknowledge it, named `tag` in its verdict. False if it can't take it now: it is
    /// offered again later. Frames go out in the order queued, and each one queued gets one verdict from
    /// [`done`](Self::done), in that order. The radio hands over a few at a time, as it holds the rest under its caps.
    fn send(&mut self, to: Mac, frame: &[u8], tries: u8, tag: u32) -> bool;
    /// The verdict on the oldest frame queued that hasn't had one, with its tag: delivered (the receiving radio
    /// acknowledged it, or it was a broadcast and went) or not, after the platform's own tries. `None` while it is
    /// still going. The tag is what the radio matches it by, so a verdict late or lost is never taken for another
    /// frame's (NB135-15).
    fn done(&mut self) -> Option<(u32, bool)>;
    /// The next frame heard, and its sender.
    fn recv(&mut self) -> Option<(Mac, Vec<u8>)>;
    /// The longest frame this end sends and takes: 250 bytes on ESP-NOW v1, up to 1470 on v2.
    fn max_frame(&self) -> usize;
    /// Takes a waker to wake when a frame comes or a verdict is in.
    fn set_waker(&mut self, waker: Waker) {
        let _ = waker;
    }
}

/// Frames waiting to go on the air, one at a time and in order: the next goes only once the radio has said whether
/// the last arrived (ESP-NOW's send callback, which may come out of order with several frames in flight). A unicast
/// frame the radio couldn't deliver is tried again, up to its own number of tries in all; a broadcast once. Each frame
/// pushed gets one verdict ([`outcome`](Self::outcome)), in order, with the tag it was pushed with.
#[derive(Debug)]
pub struct TxQueue {
    /// Address, frame, tries, tag.
    frames: VecDeque<(Mac, Vec<u8>, u8, u32)>,
    cap: usize,
    /// Tries of the head frame so far, and when the last one went if it's on the air.
    tried: u8,
    since: Option<Instant>,
    outcomes: VecDeque<(u32, bool)>,
}

impl TxQueue {
    /// A queue of at most `cap` frames waiting or with a verdict not yet taken.
    pub fn new(cap: usize) -> TxQueue {
        TxQueue { frames: VecDeque::new(), cap, tried: 0, since: None, outcomes: VecDeque::new() }
    }

    /// Queues `frame` for `to`, to be tried up to `tries` times (at least once), its verdict named `tag`; false if the
    /// queue is full.
    pub fn push(&mut self, to: Mac, frame: &[u8], tries: u8, tag: u32) -> bool {
        if self.frames.len() + self.outcomes.len() >= self.cap {
            return false;
        }
        self.frames.push_back((to, frame.to_vec(), tries.max(1), tag));
        true
    }

    /// The frame to put on the air now, if none is on it: it is then on the air until [`verdict`](Self::verdict).
    pub fn next(&mut self, now: Instant) -> Option<(Mac, &[u8])> {
        if self.since.is_some() {
            return None;
        }
        let (to, frame, _, _) = self.frames.front()?;
        self.since = Some(now);
        self.tried += 1;
        Some((*to, frame))
    }

    /// Whether a frame is on the air.
    pub fn busy(&self) -> bool {
        self.since.is_some()
    }

    /// The radio's word on the frame on the air: delivered or not. One not delivered goes again while it has tries
    /// left. A verdict with no frame on the air is ignored.
    pub fn verdict(&mut self, delivered: bool) {
        if self.since.take().is_none() {
            return;
        }
        let Some((to, _, tries, tag)) = self.frames.front() else { return };
        if !delivered && *to != BROADCAST && self.tried < *tries {
            return; // the same frame again, at the next `next`
        }
        self.outcomes.push_back((*tag, delivered));
        self.frames.pop_front();
        self.tried = 0;
    }

    /// A frame on the air with no verdict after `after` failed that try (a send callback that never came).
    pub fn expire(&mut self, now: Instant, after: Duration) {
        if self.since.is_some_and(|t| now.saturating_duration_since(t) >= after) {
            self.verdict(false);
        }
    }

    /// The oldest verdict not yet taken, with its frame's tag.
    pub fn outcome(&mut self) -> Option<(u32, bool)> {
        self.outcomes.pop_front()
    }

    /// The address of the frame on the air, if one is: a verdict for another address isn't its (NB135-I1).
    pub fn on_air(&self) -> Option<Mac> {
        self.since.and(self.frames.front().map(|(to, _, _, _)| *to))
    }

    /// Whether a frame for `to` is waiting or on the air.
    pub fn has(&self, to: Mac) -> bool {
        self.frames.iter().any(|(m, _, _, _)| *m == to)
    }

    /// Frames waiting or on the air.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

/// ESP-NOW's peer table, which a unicast frame's receiver must be in: at most 20 entries on an ESP32, this keeps
/// `cap` of them, the most lately used. Broadcast isn't counted: the platform keeps it in for good.
#[derive(Debug)]
pub struct PeerTable {
    /// Most lately used last.
    macs: Vec<Mac>,
    cap: usize,
}

/// What [`PeerTable::touch`] asks of the platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Touch {
    /// It's in the table.
    Present,
    /// Add it, after removing `evict` if there is one.
    Add { evict: Option<Mac> },
}

impl PeerTable {
    pub fn new(cap: usize) -> PeerTable {
        PeerTable { macs: Vec::new(), cap: cap.max(1) }
    }

    /// `mac` is about to be sent to: in the table already, or what to add and evict.
    pub fn touch(&mut self, mac: Mac) -> Touch {
        self.touch_with(mac, |_| false)
    }

    /// [`touch`](Self::touch), never evicting an address `busy` says still has frames waiting, whose tries would
    /// then fail at once (NB135-I2): the least lately used of the others goes. With none of them, nothing goes, and
    /// the table holds one more than its size (ESP-NOW's own holds 20).
    pub fn touch_with(&mut self, mac: Mac, busy: impl Fn(&Mac) -> bool) -> Touch {
        if mac == BROADCAST {
            return Touch::Present;
        }
        if let Some(i) = self.macs.iter().position(|m| *m == mac) {
            let m = self.macs.remove(i);
            self.macs.push(m);
            return Touch::Present;
        }
        let idle = self.macs.iter().position(|m| !busy(m));
        let evict = idle.filter(|_| self.macs.len() >= self.cap).map(|i| self.macs.remove(i));
        self.macs.push(mac);
        Touch::Add { evict }
    }

    /// The platform couldn't add `mac` after all.
    pub fn forget(&mut self, mac: Mac) {
        self.macs.retain(|m| *m != mac);
    }

    pub fn len(&self) -> usize {
        self.macs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.macs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Mac = [2, 0, 0, 0, 0, 1];
    const B: Mac = [2, 0, 0, 0, 0, 2];

    #[test]
    fn frames_go_one_at_a_time_in_order_and_are_tried_again() {
        let t = Instant::now();
        let mut q = TxQueue::new(4);
        assert!(q.push(A, b"one", 3, 7) && q.push(B, b"two", 1, 8) && q.push(BROADCAST, b"all", 4, 9));
        assert_eq!(q.next(t), Some((A, &b"one"[..])));
        assert_eq!(q.next(t), None, "one on the air at a time");
        q.verdict(false);
        assert_eq!(q.outcome(), None, "tried again");
        assert_eq!(q.next(t), Some((A, &b"one"[..])));
        q.verdict(false);
        assert_eq!(q.next(t), Some((A, &b"one"[..])));
        q.verdict(false);
        assert_eq!(q.outcome(), Some((7, false)), "three tries, then given up");
        assert_eq!(q.on_air(), None);
        assert_eq!(q.next(t), Some((B, &b"two"[..])));
        assert_eq!(q.on_air(), Some(B));
        q.verdict(true);
        assert_eq!(q.next(t), Some((BROADCAST, &b"all"[..])));
        q.verdict(false);
        let outcomes = (q.outcome(), q.outcome(), q.outcome());
        assert_eq!(outcomes, (Some((8, true)), Some((9, false)), None), "a broadcast goes once, each by its tag");
        q.verdict(true);
        assert_eq!(q.outcome(), None, "a verdict with nothing on the air is ignored");
    }

    #[test]
    fn a_full_queue_refuses_and_a_lost_callback_expires() {
        let t = Instant::now();
        let mut q = TxQueue::new(2);
        assert!(q.push(A, b"1", 1, 1) && q.push(A, b"2", 1, 2));
        assert!(!q.push(A, b"3", 1, 3), "full");
        q.next(t);
        q.expire(t + Duration::from_millis(50), Duration::from_millis(100));
        assert!(q.busy());
        q.expire(t + Duration::from_millis(100), Duration::from_millis(100));
        assert!(!q.busy());
        assert!(!q.push(A, b"3", 1, 3), "a verdict not taken still counts");
        assert_eq!(q.outcome(), Some((1, false)));
        assert!(q.push(A, b"3", 1, 3));
    }

    #[test]
    fn the_peer_table_keeps_the_latest_within_its_size() {
        let mut p = PeerTable::new(2);
        assert_eq!(p.touch(BROADCAST), Touch::Present);
        assert_eq!(p.touch(A), Touch::Add { evict: None });
        assert_eq!(p.touch(B), Touch::Add { evict: None });
        assert_eq!(p.touch(A), Touch::Present);
        let c = [2, 0, 0, 0, 0, 3];
        assert_eq!(p.touch(c), Touch::Add { evict: Some(B) }, "the least lately used goes");
        assert_eq!(p.len(), 2);
        p.forget(c);
        assert_eq!(p.touch(B), Touch::Add { evict: None });
        // NB135-I2: an address with frames waiting stays; the next least lately used goes, or none.
        let d = [2, 0, 0, 0, 0, 4];
        assert_eq!(p.touch_with(d, |m| *m == A), Touch::Add { evict: Some(B) }, "A is busy: B goes");
        assert_eq!(p.touch_with(c, |_| true), Touch::Add { evict: None }, "all busy: none goes");
        assert_eq!(p.len(), 3);
    }
}

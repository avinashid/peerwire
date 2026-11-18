//! What a platform's ESP-NOW gives the radio ([`Ether`]), and two parts of it every platform needs, sans-IO: frames
//! sent in order with retries ([`TxQueue`]), and ESP-NOW's peer table kept within its size ([`PeerTable`]).

use super::{Mac, BROADCAST};
use std::collections::VecDeque;
use std::task::Waker;
use std::time::{Duration, Instant};

/// ESP-NOW as a platform has it: frames to one MAC address or to all, and the frames heard. It never blocks.
pub trait Ether: Send {
    /// Queues `frame` for `to` ([`BROADCAST`] for everyone in range). False if it can't take it now: it is offered
    /// again later. Frames go out in the order queued, and each one queued gets one verdict from [`done`](Self::done),
    /// in that order.
    fn send(&mut self, to: Mac, frame: &[u8]) -> bool;
    /// The verdict on the oldest frame queued that hasn't had one: delivered (the receiving radio acknowledged it, or
    /// it was a broadcast and went) or not, after the platform's own tries. `None` while it is still going.
    fn done(&mut self) -> Option<bool>;
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
/// frame the radio couldn't deliver is tried again, up to `tries` times in all; a broadcast once. Each frame pushed
/// gets one verdict ([`outcome`](Self::outcome)), in order.
#[derive(Debug)]
pub struct TxQueue {
    frames: VecDeque<(Mac, Vec<u8>)>,
    cap: usize,
    tries: u8,
    /// Tries of the head frame so far, and when the last one went if it's on the air.
    tried: u8,
    since: Option<Instant>,
    outcomes: VecDeque<bool>,
}

impl TxQueue {
    /// A queue of at most `cap` frames waiting or with a verdict not yet taken, each tried up to `tries` times.
    pub fn new(cap: usize, tries: u8) -> TxQueue {
        TxQueue { frames: VecDeque::new(), cap, tries: tries.max(1), tried: 0, since: None, outcomes: VecDeque::new() }
    }

    /// Queues `frame` for `to`; false if the queue is full.
    pub fn push(&mut self, to: Mac, frame: &[u8]) -> bool {
        if self.frames.len() + self.outcomes.len() >= self.cap {
            return false;
        }
        self.frames.push_back((to, frame.to_vec()));
        true
    }

    /// The frame to put on the air now, if none is on it: it is then on the air until [`verdict`](Self::verdict).
    pub fn next(&mut self, now: Instant) -> Option<(Mac, &[u8])> {
        if self.since.is_some() {
            return None;
        }
        let (to, frame) = self.frames.front()?;
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
        let Some((to, _)) = self.frames.front() else { return };
        if !delivered && *to != BROADCAST && self.tried < self.tries {
            return; // the same frame again, at the next `next`
        }
        self.frames.pop_front();
        self.tried = 0;
        self.outcomes.push_back(delivered);
    }

    /// A frame on the air with no verdict after `after` failed that try (a send callback that never came).
    pub fn expire(&mut self, now: Instant, after: Duration) {
        if self.since.is_some_and(|t| now.saturating_duration_since(t) >= after) {
            self.verdict(false);
        }
    }

    /// The oldest verdict not yet taken.
    pub fn outcome(&mut self) -> Option<bool> {
        self.outcomes.pop_front()
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
        if mac == BROADCAST {
            return Touch::Present;
        }
        if let Some(i) = self.macs.iter().position(|m| *m == mac) {
            let m = self.macs.remove(i);
            self.macs.push(m);
            return Touch::Present;
        }
        let evict = (self.macs.len() >= self.cap).then(|| self.macs.remove(0));
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
        let mut q = TxQueue::new(4, 3);
        assert!(q.push(A, b"one") && q.push(B, b"two") && q.push(BROADCAST, b"all"));
        assert_eq!(q.next(t), Some((A, &b"one"[..])));
        assert_eq!(q.next(t), None, "one on the air at a time");
        q.verdict(false);
        assert_eq!(q.outcome(), None, "tried again");
        assert_eq!(q.next(t), Some((A, &b"one"[..])));
        q.verdict(false);
        assert_eq!(q.next(t), Some((A, &b"one"[..])));
        q.verdict(false);
        assert_eq!(q.outcome(), Some(false), "three tries, then given up");
        assert_eq!(q.next(t), Some((B, &b"two"[..])));
        q.verdict(true);
        assert_eq!(q.next(t), Some((BROADCAST, &b"all"[..])));
        q.verdict(false);
        assert_eq!((q.outcome(), q.outcome(), q.outcome()), (Some(true), Some(false), None), "a broadcast goes once");
        q.verdict(true);
        assert_eq!(q.outcome(), None, "a verdict with nothing on the air is ignored");
    }

    #[test]
    fn a_full_queue_refuses_and_a_lost_callback_expires() {
        let t = Instant::now();
        let mut q = TxQueue::new(2, 1);
        assert!(q.push(A, b"1") && q.push(A, b"2"));
        assert!(!q.push(A, b"3"), "full");
        q.next(t);
        q.expire(t + Duration::from_millis(50), Duration::from_millis(100));
        assert!(q.busy());
        q.expire(t + Duration::from_millis(100), Duration::from_millis(100));
        assert!(!q.busy());
        assert!(!q.push(A, b"3"), "a verdict not taken still counts");
        assert_eq!(q.outcome(), Some(false));
        assert!(q.push(A, b"3"));
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
    }
}

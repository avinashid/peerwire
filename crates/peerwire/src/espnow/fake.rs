//! ESP-NOW in one process, for tests: a channel ([`FakeAir`]) and stations on it ([`FakeEther`]).

use super::{Ether, Mac, TxQueue, BROADCAST};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

/// Frames a station's inbox holds at most; more are dropped, as a full receive queue drops them on a board.
const INBOX: usize = 256;
/// Frames [`FakeAir::heard`] keeps.
const HEARD: usize = 1024;

/// One ESP-NOW channel in one process: [`ether`](FakeAir::ether) puts a station on it. A unicast frame reaches the
/// station with its address and a broadcast every other station; a frame over a station's longest is dropped, as a v1
/// radio drops a long v2 frame. Of every thousand tries, `loss` are lost (the sender's radio hears no acknowledgement
/// and tries again, as a real one does) and `ack_loss` arrive but go unacknowledged (so the receiver gets the try
/// again). Frames can be overheard ([`heard`](FakeAir::heard)) and forged ([`inject`](FakeAir::inject)).
#[derive(Clone)]
#[doc(hidden)]
pub struct FakeAir(Arc<Mutex<Air>>);

/// Frames a test makes vanish at the receiver ([`FakeAir::set_hole`]): from, to, the frame.
type Hole = Box<dyn Fn(Mac, Mac, &[u8]) -> bool + Send>;

struct Air {
    stations: Vec<Station>,
    loss: u32,
    ack_loss: u32,
    rng: u64,
    heard: VecDeque<(Mac, Mac, Vec<u8>)>,
    hole: Option<Hole>,
}

struct Station {
    mac: Mac,
    inbox: VecDeque<(Mac, Vec<u8>)>,
    off: bool,
    max_frame: usize,
}

impl Air {
    /// One in a thousand, `per_thousand` times.
    fn roll(&mut self, per_thousand: u32) -> bool {
        // xorshift64: deterministic, so a failing run fails again.
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng % 1000) < u64::from(per_thousand)
    }

    fn deliver(&mut self, from: Mac, i: usize, frame: &[u8]) {
        if self.hole.as_ref().is_some_and(|h| h(from, self.stations[i].mac, frame)) {
            return;
        }
        let s = &mut self.stations[i];
        if frame.len() <= s.max_frame && s.inbox.len() < INBOX {
            s.inbox.push_back((from, frame.to_vec()));
        }
    }

    /// One try of `frame` from `from` to `to`: whether the sender's radio hears it acknowledged.
    fn transmit(&mut self, from: Mac, to: Mac, frame: &[u8]) -> bool {
        if self.heard.len() >= HEARD {
            self.heard.pop_front();
        }
        self.heard.push_back((from, to, frame.to_vec()));
        if self.stations.iter().any(|s| s.mac == from && s.off) {
            return to == BROADCAST;
        }
        if to == BROADCAST {
            for i in 0..self.stations.len() {
                if self.stations[i].mac != from && !self.stations[i].off && !self.roll(self.loss) {
                    self.deliver(from, i, frame);
                }
            }
            return true;
        }
        let Some(i) = self.stations.iter().position(|s| s.mac == to && !s.off) else { return false };
        if self.roll(self.loss) {
            return false;
        }
        self.deliver(from, i, frame);
        !self.roll(self.ack_loss)
    }
}

impl Default for FakeAir {
    fn default() -> FakeAir {
        FakeAir::new()
    }
}

impl FakeAir {
    pub fn new() -> FakeAir {
        let air = Air {
            stations: Vec::new(),
            loss: 0,
            ack_loss: 0,
            rng: 0x9e37_79b9_7f4a_7c15,
            heard: VecDeque::new(),
            hole: None,
        };
        FakeAir(Arc::new(Mutex::new(air)))
    }

    fn lock(&self) -> MutexGuard<'_, Air> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A station with address `mac` that takes frames of up to `max_frame` bytes, its unicast frames tried as many
    /// times as the radio asks.
    pub fn ether(&self, mac: Mac, max_frame: usize) -> FakeEther {
        self.lock().stations.push(Station { mac, inbox: VecDeque::new(), off: false, max_frame });
        FakeEther { air: self.clone(), me: mac, tx: TxQueue::new(64), max_frame }
    }

    /// Tries lost, and tries that arrive unacknowledged, of every thousand.
    pub fn set_loss(&self, loss: u32, ack_loss: u32) {
        let mut air = self.lock();
        (air.loss, air.ack_loss) = (loss, ack_loss);
    }

    /// Frames for which `hole` (from, to, frame) says so are acknowledged and lost, as by a receiver that drops them
    /// itself; `None` stops.
    pub fn set_hole(&self, hole: Option<Hole>) {
        self.lock().hole = hole;
    }

    /// Turns station `mac` off or on: off, it hears nothing and nothing it sends arrives.
    pub fn set_off(&self, mac: Mac, off: bool) {
        let mut air = self.lock();
        for s in air.stations.iter_mut().filter(|s| s.mac == mac) {
            s.off = off;
            s.inbox.clear();
        }
    }

    /// A frame claiming to come from `from`, as anyone in range can send: to the station `to`, or every station.
    pub fn inject(&self, from: Mac, to: Mac, frame: &[u8]) {
        let mut air = self.lock();
        for i in 0..air.stations.len() {
            let s = &air.stations[i];
            if !s.off && (s.mac == to || to == BROADCAST) && s.mac != from {
                air.deliver(from, i, frame);
            }
        }
    }

    /// The frames tried on the air, oldest first (the last 1024): sender, receiver, frame.
    pub fn heard(&self) -> Vec<(Mac, Mac, Vec<u8>)> {
        self.lock().heard.iter().cloned().collect()
    }

    /// Frames delivered that no station has taken yet. With none, every frame tried on the air has reached its
    /// receiver's engine, so a frame forged now comes after all of them.
    pub fn waiting(&self) -> usize {
        self.lock().stations.iter().map(|s| s.inbox.len()).sum()
    }
}

/// A station on a [`FakeAir`].
#[doc(hidden)]
pub struct FakeEther {
    air: FakeAir,
    me: Mac,
    tx: TxQueue,
    max_frame: usize,
}

impl Ether for FakeEther {
    fn send(&mut self, to: Mac, frame: &[u8], tries: u8, tag: u32) -> bool {
        if !self.tx.push(to, frame, tries, tag) {
            return false;
        }
        // The radio is instant here: every try goes, and has its verdict, at once.
        let now = Instant::now();
        while let Some((to, frame)) = self.tx.next(now) {
            let frame = frame.to_vec();
            let ok = self.air.lock().transmit(self.me, to, &frame);
            self.tx.verdict(ok);
        }
        true
    }

    fn done(&mut self) -> Option<(u32, bool)> {
        self.tx.outcome()
    }

    fn recv(&mut self) -> Option<(Mac, Vec<u8>)> {
        let mut air = self.air.lock();
        air.stations.iter_mut().find(|s| s.mac == self.me)?.inbox.pop_front()
    }

    fn max_frame(&self) -> usize {
        self.max_frame
    }

    fn mac(&self) -> Mac {
        self.me
    }
}

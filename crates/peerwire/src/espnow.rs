//! ESP-NOW (feature `espnow`): a Bluetooth [`Radio`] over Espressif's ESP-NOW frames, so ESP32 boards in range link
//! with no Wi-Fi network and no Bluetooth (`docs/concepts/espnow.md`).
//!
//! ESP-NOW sends frames of up to 250 bytes (1470 with v2 at both ends) to a MAC address, acknowledged by the receiving
//! radio, or broadcast, with no connection. [`EspNow`] makes the radio a node needs of that: adverts go out as
//! broadcasts, and a connection is emulated with OPEN, ACCEPT, KEEPALIVE and CLOSE frames, on ids each end draws at
//! random, with GATT values split into frames and numbered so duplicates and stale frames are dropped. Everything on
//! top is the node's Bluetooth path, unchanged: the room proof, Noise IK, sealed signals and datagrams, and
//! [`PathKind::Bluetooth`](crate::PathKind::Bluetooth) below every UDP path.
//!
//! A platform gives the radio its ESP-NOW as an [`Ether`]; `peerwire-esp` has the ESP32's. The connection logic is
//! sans-IO ([`Engine`]), and `FakeAir` (feature `testing`) has ethers in one process for tests.
//!
//! Like every carrier, ESP-NOW is untrusted, and its frames aren't authenticated: anyone in range can forge one from
//! any address, with the ids it overheard. The forged frames the design looks at (`docs/concepts/espnow.md`) can close
//! a connection or lose some of its frames, as jamming the channel would, and the node connects again; and a link that
//! times out closes the radio connections bound to its peer, so one stuck while its radio says it's up gives way to a
//! new one (NB135-1). The state stays within its caps, and what crosses a connection is sealed and proven by the node.

mod engine;
mod ether;
#[cfg(any(test, feature = "testing"))]
mod fake;
pub mod frame;

pub use engine::{device_of, mac_of, Engine, CLOSE_CHECK, MAX_VALUE, RESYNC_AFTER, WINDOW};
pub use ether::{Ether, PeerTable, Touch, TxQueue};
#[cfg(any(test, feature = "testing"))]
pub use fake::{FakeAir, FakeEther};
pub use frame::{V1_FRAME, V2_FRAME};

use crate::ble::{ConnId, DeviceId, Limits, Radio, RadioEvent};
use rand_core::{OsRng, RngCore};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::Waker;
use std::time::{Duration, Instant};

/// A MAC address.
pub type Mac = [u8; 6];

/// The broadcast address: every ESP-NOW radio on the channel hears a frame sent to it.
pub const BROADCAST: Mac = [0xff; 6];

/// How an [`EspNow`] radio behaves; [`Settings::DEFAULT`], which the first board run kept (NB-138: an ESP32-C3 and an
/// ESP32 on channel 11, ESP-NOW v2 at both ends, at 1 Mbit/s): with Bluetooth off, no frame was lost, and round trips
/// took 4 to 28 ms (p50, 32 to 1470 B), 84 ms at p99 and 141 ms at most, far inside a 1 s keepalive, a 5 s timeout
/// and 4 tries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// What the radio holds at once ([`Radio::limits`]). The radio says its values can be forged
    /// ([`Limits::forgeable`]) whatever this says.
    pub limits: Limits,
    /// The ATT MTU the node is told: it sends values of `mtu - 3` bytes at most (512 at most), which the radio splits
    /// into frames as long as the connection takes.
    pub mtu: u16,
    /// How often an advert is broadcast.
    pub advert_every: Duration,
    /// How often an OPEN goes again while it isn't answered, and how long until a connect is given up; at once if no
    /// radio acknowledged its first OPEN (NB135-12).
    pub open_every: Duration,
    pub connect_within: Duration,
    /// A KEEPALIVE goes after this long with nothing sent on a connection, and a connection with nothing heard on it
    /// for `timeout` is gone.
    pub keepalive: Duration,
    pub timeout: Duration,
    /// Frames in a row the radio couldn't deliver (each after the ether's own tries), after which a connection is
    /// taken to be gone.
    pub max_failures: u32,
    /// Tries the ether gives a frame the receiving radio doesn't acknowledge: on a connection that has had one
    /// acknowledged, and to an address that hasn't (it may be made up: an OPEN to a replayed advert, an ACCEPT to a
    /// forged OPEN), so such frames hold the air for less (NB135-3). Broadcasts and refusals get one.
    pub tries: u8,
    pub first_tries: u8,
}

impl Settings {
    /// Four connections (two unbound), eight values queued on each, a new handshake per 5 s, values of up to 512
    /// bytes; an advert every 500 ms; an OPEN every 500 ms for 10 s; a keepalive after 1 s of quiet, gone after 5 s
    /// heard nothing or 8 frames undelivered in a row; 4 tries of a frame, 2 to an address that never acknowledged.
    pub const DEFAULT: Settings = Settings {
        limits: Limits {
            connections: 4,
            unbound: 2,
            centrals: 4,
            queue: 8,
            hs1_gap: Duration::from_secs(5),
            forgeable: true,
        },
        mtu: 515,
        advert_every: Duration::from_millis(500),
        open_every: Duration::from_millis(500),
        connect_within: Duration::from_secs(10),
        keepalive: Duration::from_secs(1),
        timeout: Duration::from_secs(5),
        max_failures: 8,
        tries: 4,
        first_tries: 2,
    };
}

impl Default for Settings {
    fn default() -> Settings {
        Settings::DEFAULT
    }
}

/// What an [`EspNow`] radio has seen, for the log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Connections now, open or opening.
    pub connections: usize,
    pub frames_in: u64,
    pub frames_out: u64,
    /// Frames the radio couldn't deliver, after the ether's tries.
    pub tx_failed: u64,
    pub values_in: u64,
    pub values_out: u64,
    /// Frames that weren't ours (another app's), or were ours and malformed.
    pub not_ours: u64,
    pub malformed: u64,
    /// Frames for no connection of this end's: a stale or forged id, or another address.
    pub unknown: u64,
    /// Data frames dropped as already taken or too far behind, and as too far ahead; and how often the receiver took
    /// up a sender's place anew after frames too far off in a row.
    pub old: u64,
    pub ahead: u64,
    pub resyncs: u64,
    /// Jumps in the numbers taken (frames missed, or still to come), and parts of a value that weren't the next one
    /// (its start or a part was lost) and so were dropped.
    pub gaps: u64,
    pub orphans: u64,
    /// Opens not taken (not advertising, or one over the limits already, which the node hasn't decided on), and CLOSEs
    /// sent for accepts no open waits for.
    pub opens_refused: u64,
    pub refusals: u64,
    /// Connects given up as no radio acknowledged their OPEN: an address no radio has, made up or gone (NB135-12).
    pub connects_unacked: u64,
    /// CLOSEs whose connection answered the probe that followed (forged, or stale), so stayed open.
    pub closes_denied: u64,
    /// Connections closed by a second ACCEPT with another id within 3 s of the first: one of the two was forged
    /// ([`RadioEvent::Dropped`]).
    pub accept_conflicts: u64,
    /// Connections closed by the other end (a CLOSE, and no answer to the probe), gone quiet, or failing.
    pub closed_by_peer: u64,
    pub closed_quiet: u64,
    pub closed_failing: u64,
    /// Adverts and values the node was too slow to take, and frames over the send queue's caps.
    pub events_dropped: u64,
    pub tx_dropped: u64,
    /// Values the node gave that were empty or too long.
    pub values_refused: u64,
    /// Verdicts whose tag names no frame waiting for one (a late verdict, for a frame taken as lost already), and frames
    /// whose verdict never came (in 5 s, or before a later frame's).
    pub stray_verdicts: u64,
    pub lost_verdicts: u64,
}

/// A Bluetooth [`Radio`] on ESP-NOW: an [`Engine`] driven by the time and an [`Ether`].
pub struct EspNow<E: Ether> {
    engine: Engine,
    ether: E,
    watch: Watch,
}

/// A radio's [`Stats`] as of its last poll, readable once the node owns the radio ([`EspNow::watch`]).
#[derive(Clone, Debug, Default)]
pub struct Watch(Arc<Mutex<Stats>>);

impl Watch {
    pub fn stats(&self) -> Stats {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<E: Ether> EspNow<E> {
    /// The radio on `ether`, with its ids drawn from the system's random numbers.
    pub fn new(ether: E, settings: Settings) -> EspNow<E> {
        let mut seed = [0u8; 32];
        OsRng.fill_bytes(&mut seed);
        EspNow::with_seed(ether, settings, seed)
    }

    /// [`new`](Self::new) with the seed of its ids: fresh and secret, as the ids are what tell a forged frame from a
    /// peer's.
    pub fn with_seed(ether: E, settings: Settings, seed: [u8; 32]) -> EspNow<E> {
        let engine = Engine::new(settings, ether.max_frame(), seed, Instant::now());
        EspNow { engine, ether, watch: Watch::default() }
    }

    pub fn stats(&self) -> Stats {
        self.engine.stats()
    }

    /// The radio's stats from wherever the node is: take it before the node takes the radio.
    pub fn watch(&self) -> Watch {
        self.watch.clone()
    }

    /// The longest frame this end sends and takes.
    pub fn max_frame(&self) -> usize {
        self.engine.max_frame()
    }

    pub fn ether(&self) -> &E {
        &self.ether
    }

    pub fn ether_mut(&mut self) -> &mut E {
        &mut self.ether
    }

    /// Moves frames and verdicts between the ether and the engine, runs the timers, and hands the ether what is due:
    /// a few frames at a time ([`Engine::peek`]), the next as a verdict frees a slot, so what waits for the air waits
    /// in the engine, under its caps (NB135-15).
    fn pump(&mut self) {
        let now = Instant::now();
        self.verdicts();
        // Bounded, so a flood can't hold the node: what is left waits for the next poll.
        for _ in 0..256 {
            let Some((from, frame)) = self.ether.recv() else { break };
            self.engine.handle(now, from, &frame);
        }
        self.engine.timers(now);
        while let Some((to, frame, tries, tag)) = self.engine.peek() {
            if !self.ether.send(to, frame, tries, tag) {
                break;
            }
            self.engine.commit(now);
            self.verdicts(); // an ether whose verdicts are in at once takes the next at once
        }
        *self.watch.0.lock().unwrap_or_else(PoisonError::into_inner) = self.engine.stats();
    }

    /// Hands the engine the verdicts the ether has, by their tags; bounded, as above.
    fn verdicts(&mut self) {
        for _ in 0..256 {
            let Some((tag, ok)) = self.ether.done() else { break };
            self.engine.outcome(tag, ok);
        }
    }
}

impl<E: Ether> Radio for EspNow<E> {
    fn advertise(&mut self, data: Option<&[u8]>) {
        self.engine.advertise(Instant::now(), data);
        self.pump();
    }

    fn scan(&mut self, on: bool) {
        self.engine.scan(on);
    }

    fn connect(&mut self, device: DeviceId) {
        self.engine.connect(Instant::now(), device);
        self.pump();
    }

    fn send(&mut self, conn: ConnId, value: &[u8]) -> bool {
        let taken = self.engine.send(Instant::now(), conn, value);
        self.pump();
        taken
    }

    fn disconnect(&mut self, conn: ConnId) {
        self.engine.disconnect(Instant::now(), conn);
        self.pump();
    }

    fn poll(&mut self) -> Option<RadioEvent> {
        if let Some(e) = self.engine.poll_event() {
            return Some(e);
        }
        self.pump();
        self.engine.poll_event()
    }

    /// The settings' limits, with [`Limits::forgeable`] on: anyone in range can forge an ESP-NOW frame (NB135-8).
    fn limits(&self) -> Limits {
        Limits { forgeable: true, ..self.engine.settings().limits }
    }

    /// The ether's address, which other radios report this one's adverts from ([`device_of`]): the node binds its
    /// advert's tag to it, so a copy sent from another address isn't taken for it (NB135-12).
    fn address(&self) -> Option<DeviceId> {
        Some(device_of(self.ether.mac()))
    }

    fn set_waker(&mut self, waker: Waker) {
        self.ether.set_waker(waker);
    }
}

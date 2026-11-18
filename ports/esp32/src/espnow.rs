//! ESP-NOW (feature `espnow`, NB-135): the ESP32's [`Ether`] for `peerwire::espnow`, so a node links with boards in
//! range over ESP-NOW frames, with no Wi-Fi network and no Bluetooth (docs/concepts/espnow.md).
//!
//! Wi-Fi must be started as a station first and stay started for as long as the radio runs: on no network
//! ([`wifi::start_on_channel`](crate::wifi::start_on_channel)), when every board in the room must be on the same
//! channel, or on one ([`wifi::join`](crate::wifi::join)), when ESP-NOW follows the network's channel. Frames go out
//! one at a time from a thread of their own, each tried as many times as the radio asks while the receiving radio
//! doesn't acknowledge it (on top of the radio's own retries: `Settings::tries`, fewer to an address that never
//! acknowledged, NB135-3), as ESP-NOW's send callback can't be matched to a frame with several in flight; frames
//! received wait in a queue of [`RX_QUEUE`] for the node, and more are dropped.
//!
//! ```no_run
//! use peerwire_esp::espnow;
//! use peerwire_esp::peerwire::espnow::Settings;
//! use peerwire_esp::peerwire::{Node, PathKind, Policy};
//!
//! # fn main() -> anyhow::Result<()> {
//! let p = esp_idf_svc::hal::peripherals::Peripherals::take()?;
//! let sysloop = esp_idf_svc::eventloop::EspSystemEventLoop::take()?;
//! let _wifi = peerwire_esp::wifi::start_on_channel(p.modem, sysloop, 11)?; // kept for as long as the radio runs
//! let radio = espnow::radio(true, Settings::DEFAULT)?;
//! let mut node = Node::builder()
//!     .config(peerwire_esp::config())
//!     .policy(Policy::new([PathKind::Bluetooth]))
//!     .bluetooth(radio)
//!     .bind("127.0.0.1:4040")?;
//! node.join_room("my-app", "kitchen"); // links with boards nearby on channel 11
//! # Ok(())
//! # }
//! ```

use std::collections::VecDeque;
use std::ffi::c_int;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::task::Waker;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use esp_idf_svc::sys;
use log::{info, warn};
use peerwire::espnow::{frame, EspNow, Ether, Mac, PeerTable, Settings, Touch, TxQueue, BROADCAST, V1_FRAME, V2_FRAME};

/// Frames waiting to go, or with a verdict the node hasn't taken.
pub const TX_QUEUE: usize = 32;
/// Frames received waiting for the node at most, and their bytes: 16 KB, whoever sends (NB135-2).
pub const RX_QUEUE: usize = 32;
pub const RX_BYTES: usize = 16 * 1024;
/// Entries this driver keeps in ESP-NOW's peer table (20 at most, with the broadcast address).
pub const PEERS: usize = 16;
/// A frame with no send callback after this long failed that try.
const VERDICT_WITHIN: Duration = Duration::from_millis(200);
const TX_STACK: usize = 4 * 1024;

/// What the callbacks, the TX thread and the node share. ESP-NOW's callbacks take no argument: it is a static.
struct Shared {
    tx: Mutex<TxQueue>,
    /// Wakes the TX thread: a frame queued, or a verdict in.
    more: Condvar,
    rx: Mutex<Rx>,
    waker: Mutex<Option<Waker>>,
    /// Frames dropped as the receive queue was full, or as not ours (another app's, or too long to be ours); tries
    /// esp_now_send refused; send callbacks for no frame on the air (NB135-I1).
    rx_dropped: AtomicU32,
    not_ours: AtomicU32,
    refused: AtomicU32,
    stray: AtomicU32,
}

/// Frames received, waiting for the node, and their bytes.
#[derive(Default)]
struct Rx {
    frames: VecDeque<(Mac, Vec<u8>)>,
    bytes: usize,
}

static SHARED: OnceLock<Shared> = OnceLock::new();
static STARTED: AtomicBool = AtomicBool::new(false);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn wake(&self) {
        if let Some(w) = &*lock(&self.waker) {
            w.wake_by_ref();
        }
    }
}

/// ESP-NOW on this chip, as `peerwire::espnow` takes it. One at a time: ESP-NOW has one set of callbacks.
pub struct EspNowEther {
    shared: &'static Shared,
    peers: PeerTable,
    max_frame: usize,
    version: u32,
}

impl EspNowEther {
    /// Starts ESP-NOW on the started Wi-Fi station, with frames of up to 1470 bytes if `v2` and the chip has ESP-NOW
    /// v2 (a connection takes the shorter of its two ends'), else 250. Fails if ESP-NOW is started already.
    pub fn start(v2: bool) -> anyhow::Result<EspNowEther> {
        if STARTED.swap(true, Relaxed) {
            bail!("ESP-NOW is started already");
        }
        let shared = SHARED.get_or_init(|| Shared {
            tx: Mutex::new(TxQueue::new(TX_QUEUE)),
            more: Condvar::new(),
            rx: Mutex::new(Rx::default()),
            waker: Mutex::new(None),
            rx_dropped: AtomicU32::new(0),
            not_ours: AtomicU32::new(0),
            refused: AtomicU32::new(0),
            stray: AtomicU32::new(0),
        });
        sys::esp!(unsafe { sys::esp_now_init() })?;
        sys::esp!(unsafe { sys::esp_now_register_recv_cb(Some(on_recv)) })?;
        sys::esp!(unsafe { sys::esp_now_register_send_cb(Some(on_send)) })?;
        add_peer(BROADCAST)?;
        let mut version = 0u32;
        sys::esp!(unsafe { sys::esp_now_get_version(&mut version) })?;
        let max_frame = if v2 && version >= 2 { V2_FRAME } else { V1_FRAME };
        std::thread::Builder::new()
            .name("espnow-tx".into())
            .stack_size(TX_STACK)
            .spawn(move || transmit(shared))
            .map_err(|e| anyhow!("the ESP-NOW thread: {e}"))?;
        info!("ESP-NOW v{version} up, frames of up to {max_frame} B");
        Ok(EspNowEther { shared, peers: PeerTable::new(PEERS), max_frame, version })
    }

    /// ESP-NOW's version on this chip: 1, or 2 with frames of up to 1470 bytes.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// [`dropped`].
    pub fn dropped(&self) -> (u32, u32, u32, u32) {
        dropped()
    }
}

impl Ether for EspNowEther {
    fn send(&mut self, to: Mac, frame: &[u8], tries: u8) -> bool {
        // Never evicting an address with frames still waiting, whose tries would fail at once (NB135-I2).
        let touch = {
            let tx = lock(&self.shared.tx);
            self.peers.touch_with(to, |m| tx.has(*m))
        };
        if let Touch::Add { evict } = touch {
            if let Some(old) = evict {
                unsafe { sys::esp_now_del_peer(old.as_ptr()) };
            }
            // A frame to an address ESP-NOW won't take still goes in the queue, and fails there: the engine hears so.
            if let Err(e) = add_peer(to) {
                warn!("ESP-NOW: {e}");
                self.peers.forget(to);
            }
        }
        let taken = lock(&self.shared.tx).push(to, frame, tries);
        if taken {
            self.shared.more.notify_one();
        }
        taken
    }

    fn done(&mut self) -> Option<bool> {
        lock(&self.shared.tx).outcome()
    }

    fn recv(&mut self) -> Option<(Mac, Vec<u8>)> {
        let mut rx = lock(&self.shared.rx);
        let (from, frame) = rx.frames.pop_front()?;
        rx.bytes -= frame.len();
        Some((from, frame))
    }

    fn max_frame(&self) -> usize {
        self.max_frame
    }

    fn set_waker(&mut self, waker: Waker) {
        *lock(&self.shared.waker) = Some(waker);
    }
}

/// Since ESP-NOW started: frames dropped as the receive queue was full, frames dropped as not ours, tries
/// esp_now_send refused, and send callbacks that named no frame on the air.
pub fn dropped() -> (u32, u32, u32, u32) {
    let Some(s) = SHARED.get() else { return (0, 0, 0, 0) };
    (s.rx_dropped.load(Relaxed), s.not_ours.load(Relaxed), s.refused.load(Relaxed), s.stray.load(Relaxed))
}

/// The radio for a node: [`EspNowEther::start`]`(v2)` under `peerwire::espnow::EspNow`.
pub fn radio(v2: bool, settings: Settings) -> anyhow::Result<EspNow<EspNowEther>> {
    Ok(EspNow::new(EspNowEther::start(v2)?, settings))
}

fn add_peer(mac: Mac) -> anyhow::Result<()> {
    let mut peer: sys::esp_now_peer_info_t = unsafe { std::mem::zeroed() };
    peer.peer_addr = mac;
    // 0: whatever channel the station is on, so a peer stays reachable when the station joins a network.
    peer.channel = 0;
    peer.ifidx = sys::wifi_interface_t_WIFI_IF_STA;
    // Unencrypted: the node seals what crosses (and ESP-NOW encrypts for a handful of peers only).
    peer.encrypt = false;
    match unsafe { sys::esp_now_add_peer(&peer) } {
        sys::ESP_OK | sys::ESP_ERR_ESPNOW_EXIST => Ok(()),
        e => Err(anyhow!("esp_now_add_peer: {e}")),
    }
}

/// The TX thread: the next frame on the air once the last has its verdict, and a try with no callback expired.
fn transmit(shared: &'static Shared) {
    let mut q = lock(&shared.tx);
    loop {
        let now = Instant::now();
        q.expire(now, VERDICT_WITHIN);
        let Some((to, frame)) = q.next(now) else {
            q = shared.more.wait_timeout(q, VERDICT_WITHIN / 2).unwrap_or_else(PoisonError::into_inner).0;
            continue;
        };
        // Not under the lock: the send callback, on the Wi-Fi task, takes it.
        let frame = frame.to_vec();
        drop(q);
        let r = unsafe { sys::esp_now_send(to.as_ptr(), frame.as_ptr(), frame.len()) };
        q = lock(&shared.tx);
        if r != sys::ESP_OK {
            shared.refused.fetch_add(1, Relaxed);
            q.verdict(false);
            drop(q);
            shared.wake();
            if r == sys::ESP_ERR_ESPNOW_NO_MEM {
                // ESP-NOW's own queue is full: let the Wi-Fi task drain it.
                std::thread::sleep(Duration::from_millis(2));
            }
            q = lock(&shared.tx);
        }
    }
}

/// ESP-NOW's receive callback, on the Wi-Fi task: queues the frame for the node, or drops it. Anything that isn't
/// one of ours by its first bytes and length goes before it is copied, and a frame that doesn't fit the queue's
/// frames or bytes goes too, so anyone in range can make the board hold [`RX_BYTES`] at most (NB135-2).
unsafe extern "C" fn on_recv(info: *const sys::esp_now_recv_info_t, data: *const u8, len: c_int) {
    let Some(shared) = SHARED.get() else { return };
    if info.is_null() || data.is_null() || len <= 0 {
        return;
    }
    let src = unsafe { (*info).src_addr };
    if src.is_null() {
        return;
    }
    let frame = unsafe { std::slice::from_raw_parts(data, len as usize) };
    if !frame::may_be_ours(frame) {
        shared.not_ours.fetch_add(1, Relaxed);
        return;
    }
    let from: Mac = unsafe { *(src as *const Mac) };
    let mut rx = lock(&shared.rx);
    if rx.frames.len() >= RX_QUEUE || rx.bytes + frame.len() > RX_BYTES {
        shared.rx_dropped.fetch_add(1, Relaxed);
        let (frames, bytes) = (rx.frames.len(), rx.bytes);
        drop(rx);
        check::full(frames, bytes);
        return;
    }
    rx.bytes += frame.len();
    rx.frames.push_back((from, frame.to_vec()));
    drop(rx);
    shared.wake();
}

/// ESP-NOW's send callback, on the Wi-Fi task: the verdict on the frame on the air, and the TX thread woken. Only a
/// callback that names the address of the frame on the air is its verdict: a late one, for a try already given up,
/// or one for a frame sent outside the queue, isn't (NB135-I1).
unsafe extern "C" fn on_send(info: *const sys::esp_now_send_info_t, status: sys::esp_now_send_status_t) {
    let Some(shared) = SHARED.get() else { return };
    let to = if info.is_null() { std::ptr::null() } else { unsafe { (*info).des_addr.cast_const() } };
    let to: Option<Mac> = (!to.is_null()).then(|| unsafe { *(to as *const Mac) });
    let ok = status == sys::esp_now_send_status_t_ESP_NOW_SEND_SUCCESS;
    if to.is_some_and(|to| check::sent(to, ok)) {
        return;
    }
    let mut q = lock(&shared.tx);
    if to.is_some() && to == q.on_air() {
        q.verdict(ok);
    } else {
        shared.stray.fetch_add(1, Relaxed);
    }
    drop(q);
    shared.more.notify_one();
    shared.wake();
}

/// Board checks for NB-135's security review, for the `espnow_mesh` example: how long one failed try holds the air,
/// and the heap with the receive queue full. Not for apps: they send outside the radio's queue.
pub mod check {
    use super::{add_peer, lock, Mac};
    use esp_idf_svc::sys;
    use peerwire::espnow::frame::{Frame, LONGEST};
    use peerwire::espnow::BROADCAST;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// The address a [`failed_try`] sends to, and when its callback came and what it said.
    static TRY: Mutex<Option<(Mac, Option<(Instant, bool)>)>> = Mutex::new(None);
    /// The heap the first time the receive queue was full in a second, and how often it was.
    static FULL: Mutex<Option<RxFull>> = Mutex::new(None);

    /// The receive queue full: how often it was (a snapshot at most once a second), its frames and bytes then, and the
    /// heap's free bytes and largest block then.
    #[derive(Clone, Copy, Debug)]
    pub struct RxFull {
        pub count: u32,
        pub frames: usize,
        pub bytes: usize,
        pub free: u32,
        pub largest: u32,
        at: Instant,
    }

    /// Puts a frame of `len` bytes to `to`, an address no radio in range has, on the air outside the radio's queue, and
    /// gives how long ESP-NOW took to report it (after the radio's own retries), and whether it was acknowledged: the
    /// time one failed try holds the air. ESP-NOW must be started; a frame of the radio's on the air delays it.
    pub fn failed_try(to: Mac, len: usize, within: Duration) -> anyhow::Result<(Duration, bool)> {
        add_peer(to)?;
        *lock(&TRY) = Some((to, None));
        let frame = vec![0x5a; len.clamp(1, LONGEST)];
        let start = Instant::now();
        let r = unsafe { sys::esp_now_send(to.as_ptr(), frame.as_ptr(), frame.len()) };
        let mut done = None;
        while r == sys::ESP_OK && done.is_none() && start.elapsed() < within {
            std::thread::sleep(Duration::from_millis(1));
            done = lock(&TRY).and_then(|(_, d)| d);
        }
        *lock(&TRY) = None;
        unsafe { sys::esp_now_del_peer(to.as_ptr()) };
        sys::esp!(r)?;
        let (at, ok) = done.ok_or_else(|| anyhow::anyhow!("no send callback within {within:?}"))?;
        Ok((at.saturating_duration_since(start), ok))
    }

    /// The send callback's word for `to`: taken if a [`failed_try`] waits for it.
    pub(super) fn sent(to: Mac, ok: bool) -> bool {
        let mut t = lock(&TRY);
        match &mut *t {
            Some((m, done @ None)) if *m == to => {
                *done = Some((Instant::now(), ok));
                true
            }
            _ => false,
        }
    }

    /// The receive queue was full with `frames` and `bytes`: a snapshot of the heap, at most once a second.
    pub(super) fn full(frames: usize, bytes: usize) {
        let mut f = lock(&FULL);
        let count = f.map_or(0, |f| f.count) + 1;
        if f.is_some_and(|f| f.at.elapsed() < Duration::from_secs(1)) {
            if let Some(f) = f.as_mut() {
                f.count = count;
            }
            return;
        }
        let (free, largest) = unsafe {
            (sys::esp_get_free_heap_size(), sys::heap_caps_get_largest_free_block(sys::MALLOC_CAP_DEFAULT) as u32)
        };
        *f = Some(RxFull { count, frames, bytes, free, largest, at: Instant::now() });
    }

    /// The last [`RxFull`] snapshot, if the receive queue has been full.
    pub fn rx_full() -> Option<RxFull> {
        *lock(&FULL)
    }

    /// Broadcasts `n` frames of `len` bytes (at most the longest the parser takes) that every board's receive path
    /// queues but no connection takes, outside the radio's queue: a burst to fill a board's receive queue. How many
    /// ESP-NOW took.
    pub fn burst(n: usize, len: usize) -> usize {
        let len = len.clamp(32, LONGEST);
        let payload = vec![0x5a; len - peerwire::espnow::frame::DATA_HEADER];
        let total = payload.len() as u16;
        let mut sent = 0;
        for i in 0..n {
            let f = Frame::Data {
                to: 0x0b75_7257_0000_0000 | i as u64,
                seq: 0,
                first: true,
                last: true,
                total,
                payload: &payload,
            };
            let f = f.encode();
            for _ in 0..20 {
                match unsafe { sys::esp_now_send(BROADCAST.as_ptr(), f.as_ptr(), f.len()) } {
                    sys::ESP_OK => {
                        sent += 1;
                        break;
                    }
                    sys::ESP_ERR_ESPNOW_NO_MEM => std::thread::sleep(Duration::from_millis(1)),
                    _ => break,
                }
            }
        }
        sent
    }
}

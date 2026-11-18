//! ESP-NOW between two boards (NB-135): round trips, loss and frames a second, with no access point, on one, and
//! with Bluetooth LE on, measured before ESP-NOW carries Peerwire links.
//!
//! Both boards run it. Each starts Wi-Fi as a station on no network (`wifi::start_on_channel`), on the channel it
//! hears its hotspot on (wifi.sh's record in the last flash sector; channel 11 if it hears none), and broadcasts
//! hellos until it hears the other board's: no addresses are typed in. Node 1 (the `PWN` record, as the mesh example
//! reads it) initiates and node 2 answers; with no record, the board with the lower MAC initiates. Every frame but a
//! hello carries the run's group, a random number the initiator draws at boot and gives in its hellos, so two pairs of
//! boards nearby keep apart, and nothing a listener could test a room's name against goes on the air (NB135-I4).
//!
//! In each mode the initiator measures, at 32, 128, 250 and (ESP-NOW v2 on both boards) 1470 B:
//!
//! - round trips: 300 pings, one at a time, each answered by the responder with the same bytes; one unanswered in
//!   200 ms is lost. `p50_us` and the rest are from the send to the answer in the main task, `cb_` ones to the answer
//!   in ESP-NOW's receive callback;
//! - a flood: 2 s of frames one way, up to 16 not yet acknowledged by the radio at once; the responder counts them in
//!   its receive callback, and `fps` is the frames it got over the time between the first and the last.
//!
//! The modes, in order: `a` no access point; `a54` the same at 54 Mbit/s (802.11g) rather than ESP-NOW's default 1;
//! `b1` the initiator on the hotspot, the responder not; `b2` both on it; `c2` both on it with Bluetooth LE
//! advertising and scanning (the port's own NimBLE driver, as a Peerwire node runs it); `c` both off it again, with
//! Bluetooth still on. A board that can't join stays off the hotspot, and the result lines say which boards were on
//! it (`ap=initiator/responder`) and which had Bluetooth on (`ble=`).
//!
//! It prints one `ESPNOW_RESULT` line per mode and size on the initiator, an `ESPNOW_RX` line per flood on the
//! responder, and `ESPNOW_DONE` on both at the end:
//!
//! ```text
//! ESPNOW_RESULT mode=a ch=11 size=250 n=300 ok=300 loss=0.00% p50_us=.. p90_us=.. p99_us=.. max_us=.. cb_p50_us=..
//!   cb_p99_us=.. fps=.. kbps=.. flood_sent=.. flood_rcvd=.. flood_loss=..% tx_fail=.. nomem=.. dup=.. ooo=..
//!   rssi=.. peer_rssi=.. ap=1/0 ble=0/0 txp=.. heap=..
//! ```
//!
//! Build it with `FEATURES=ble EXAMPLE=espnow_probe ./build.sh` (`CHIP=esp32` for the original ESP32): Bluetooth
//! needs ESP-IDF built with NimBLE. The NB-135 kit writes the config sector and runs both boards.

use std::ffi::c_int;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{anyhow, ensure};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::sys;
use log::{info, warn};
use peerwire_esp::ble::Nimble;
use peerwire_esp::heap;
use peerwire_esp::peerwire::ble::{Radio, RadioEvent};
use peerwire_esp::wifi::{self, Wifi};

/// The config sector, as the mesh example reads it: b"PWN" and the node number at +0, wifi.sh's record at +0x40.
const CFG_ADDR: u32 = 0x3F_F000;
const WIFI_OFFSET: usize = 0x40;
/// The channel when the hotspot isn't heard: the hotspot's own on the NB-135 boards.
const DEFAULT_CHANNEL: u8 = 11;

const MAGIC: [u8; 4] = *b"PWE1";
/// Magic, kind, mode, group and sequence number.
const HEADER: usize = 14;
/// The smallest frame: the header and a kind's own fields fit.
const MIN_FRAME: usize = 32;
const SIZES: [usize; 4] = [32, 128, 250, 1470];
/// What ESP-NOW v1 carries; v2 carries up to 1470 B.
const V1_MAX: usize = 250;
const PINGS: usize = 300;
const PING_TIMEOUT: Duration = Duration::from_millis(200);
/// Between one answered ping and the next.
const PING_GAP_MS: u32 = 2;
const FLOOD_FOR: Duration = Duration::from_secs(2);
/// Frames counted in a flood at most: the receiver's bitmap of those seen.
const FLOOD_MAX: u32 = 16 * 1024;
/// Frames sent and not yet reported by the send callback, at most.
const WINDOW: u32 = 16;
const JOIN_WITHIN: Duration = Duration::from_secs(40);
/// How long the initiator waits for the responder to take up a mode (it may be joining the hotspot).
const SYNC_WITHIN: Duration = Duration::from_secs(120);
/// A paired responder that hears nothing from its initiator this long ends.
const LOST_AFTER: Duration = Duration::from_secs(300);
const HELLO_EVERY: Duration = Duration::from_millis(300);
/// A responder that hears nothing from its initiator this long broadcasts hellos again. Longer than a flood and its
/// report, whose frames the main task doesn't see.
const QUIET: Duration = Duration::from_secs(10);
const QUEUE: usize = 32;
const BROADCAST: [u8; 6] = [0xff; 6];

const HELLO: u8 = 1;
const PING: u8 = 2;
const PONG: u8 = 3;
const FLOOD: u8 = 4;
const FEND: u8 = 5;
const FREP: u8 = 6;
const PHASE: u8 = 7;
const PACK: u8 = 8;
const DONE: u8 = 9;
const DACK: u8 = 10;

/// A board's state, in the byte after the header of hellos and answers.
const ON_AP: u8 = 1;
const BLE_ON: u8 = 2;

/// What one mode asks of the boards.
struct Mode {
    name: &'static str,
    /// 54 Mbit/s 802.11g to the peer, rather than ESP-NOW's 1 Mbit/s.
    fast: bool,
    /// On the hotspot: the initiator, the responder.
    ap: [bool; 2],
    ble: bool,
}

const MODES: [Mode; 6] = [
    Mode { name: "a", fast: false, ap: [false, false], ble: false },
    Mode { name: "a54", fast: true, ap: [false, false], ble: false },
    Mode { name: "b1", fast: false, ap: [true, false], ble: false },
    Mode { name: "b2", fast: false, ap: [true, true], ble: false },
    Mode { name: "c2", fast: false, ap: [true, true], ble: true },
    Mode { name: "c", fast: false, ap: [false, false], ble: true },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Initiator,
    Responder,
}

impl Role {
    fn other(self) -> Role {
        match self {
            Role::Initiator => Role::Responder,
            Role::Responder => Role::Initiator,
        }
    }

    fn code(role: Option<Role>) -> u8 {
        match role {
            None => 0,
            Some(Role::Initiator) => 1,
            Some(Role::Responder) => 2,
        }
    }

    fn from_code(code: u8) -> Option<Role> {
        match code {
            1 => Some(Role::Initiator),
            2 => Some(Role::Responder),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Role::Initiator => "initiator",
            Role::Responder => "responder",
        }
    }
}

/// The header of one of our frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Header {
    kind: u8,
    mode: u8,
    seq: u32,
}

/// A frame of `kind` and `size` bytes (at least [`MIN_FRAME`]), its body a pattern of `seq`.
fn frame(kind: u8, mode: u8, group: u32, seq: u32, size: usize) -> Vec<u8> {
    let size = size.max(MIN_FRAME);
    let mut f = Vec::with_capacity(size);
    f.extend_from_slice(&MAGIC);
    f.extend_from_slice(&[kind, mode]);
    f.extend_from_slice(&group.to_le_bytes());
    f.extend_from_slice(&seq.to_le_bytes());
    f.extend((HEADER..size).map(|i| (seq as u8).wrapping_add(i as u8)));
    f
}

/// The header of `data` if it is one of our frames in `group`, or a hello (which carries group 0, and its sender's
/// group in its body).
fn parse(data: &[u8], group: u32) -> Option<Header> {
    if data.len() < MIN_FRAME || data[..4] != MAGIC {
        return None;
    }
    if data[6..10] != group.to_le_bytes() && !(data[4] == HELLO && data[6..10] == [0; 4]) {
        return None;
    }
    Some(Header { kind: data[4], mode: data[5], seq: u32::from_le_bytes(data[10..14].try_into().ok()?) })
}

fn put_u32(f: &mut [u8], at: usize, v: u32) {
    f[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn get_u32(f: &[u8], at: usize) -> u32 {
    f.get(at..at + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
}

/// The nearest-rank `p`th percentile of `sorted`, or -1 if it's empty.
fn pct(sorted: &[u32], p: usize) -> i64 {
    if sorted.is_empty() {
        return -1;
    }
    let rank = (p * sorted.len()).div_ceil(100).max(1);
    i64::from(sorted[rank - 1])
}

/// A frame from the radio, as the receive callback queues it for the main task.
struct Rx {
    from: [u8; 6],
    /// When the callback ran, in µs since boot.
    at: i64,
    rssi: i8,
    data: Vec<u8>,
}

static RX: OnceLock<SyncSender<Rx>> = OnceLock::new();
static GROUP: AtomicU32 = AtomicU32::new(0);
/// Frames dropped as the queue was full, and frames not ours.
static RX_DROPPED: AtomicU32 = AtomicU32::new(0);
static FOREIGN: AtomicU32 = AtomicU32::new(0);
/// Unicast frames the radio reported sent (acknowledged) and failed.
static TX_OK: AtomicU32 = AtomicU32::new(0);
static TX_FAIL: AtomicU32 = AtomicU32::new(0);
static COUNTS: Counts = Counts::new();

/// A flood's frames, counted in the receive callback, so the count is the radio's and not the main task's. Only the
/// Wi-Fi task writes it; the main task reads it once the flood's end has come through the queue after its frames. The
/// C3 has no 64-bit atomics: times are the low 32 bits of µs since boot, which wrap every 71 minutes.
struct Counts {
    burst: AtomicU32,
    size: AtomicU32,
    count: AtomicU32,
    unique: AtomicU32,
    ooo: AtomicU32,
    /// The highest frame number seen, plus one; 0 before any.
    max_seq: AtomicU32,
    first: AtomicU32,
    last: AtomicU32,
    rssi: AtomicI32,
    seen: [AtomicU32; (FLOOD_MAX / 32) as usize],
}

/// What [`Counts`] holds for one flood.
#[derive(Clone, Copy, Debug, Default)]
struct Report {
    count: u32,
    unique: u32,
    ooo: u32,
    span_us: u32,
    rssi: i8,
}

impl Counts {
    const fn new() -> Counts {
        Counts {
            burst: AtomicU32::new(u32::MAX),
            size: AtomicU32::new(0),
            count: AtomicU32::new(0),
            unique: AtomicU32::new(0),
            ooo: AtomicU32::new(0),
            max_seq: AtomicU32::new(0),
            first: AtomicU32::new(0),
            last: AtomicU32::new(0),
            rssi: AtomicI32::new(0),
            seen: [const { AtomicU32::new(0) }; (FLOOD_MAX / 32) as usize],
        }
    }

    fn count(&self, burst: u8, seq: u32, size: usize, at: i64, rssi: i8) {
        let at = at as u32;
        if self.burst.load(Relaxed) != u32::from(burst) {
            for w in &self.seen {
                w.store(0, Relaxed);
            }
            for c in [&self.count, &self.unique, &self.ooo] {
                c.store(0, Relaxed);
            }
            self.max_seq.store(0, Relaxed);
            self.rssi.store(0, Relaxed);
            self.first.store(at, Relaxed);
            self.size.store(size as u32, Relaxed);
            self.burst.store(u32::from(burst), Relaxed);
        }
        self.count.fetch_add(1, Relaxed);
        self.last.store(at, Relaxed);
        self.rssi.fetch_add(i32::from(rssi), Relaxed);
        if seq >= FLOOD_MAX {
            return;
        }
        let (word, bit) = (&self.seen[(seq / 32) as usize], 1 << (seq % 32));
        if word.fetch_or(bit, Relaxed) & bit == 0 {
            self.unique.fetch_add(1, Relaxed);
            if seq < self.max_seq.load(Relaxed).saturating_sub(1) {
                self.ooo.fetch_add(1, Relaxed);
            } else {
                self.max_seq.store(seq + 1, Relaxed);
            }
        }
    }

    fn report(&self, burst: u8) -> Report {
        if self.burst.load(Relaxed) != u32::from(burst) {
            return Report::default();
        }
        let count = self.count.load(Relaxed);
        Report {
            count,
            unique: self.unique.load(Relaxed),
            ooo: self.ooo.load(Relaxed),
            span_us: self.last.load(Relaxed).wrapping_sub(self.first.load(Relaxed)),
            rssi: (self.rssi.load(Relaxed) / count.max(1) as i32) as i8,
        }
    }
}

/// ESP-NOW's receive callback, on the Wi-Fi task: counts flood frames, and queues our other frames for the main task
/// (dropping them when the queue is full) without blocking.
unsafe extern "C" fn on_recv(info: *const sys::esp_now_recv_info_t, data: *const u8, len: c_int) {
    let at = unsafe { sys::esp_timer_get_time() };
    if info.is_null() || data.is_null() || len <= 0 {
        return;
    }
    let info = unsafe { &*info };
    if info.src_addr.is_null() {
        return;
    }
    let data = unsafe { std::slice::from_raw_parts(data, len as usize) };
    let Some(h) = parse(data, GROUP.load(Relaxed)) else {
        FOREIGN.fetch_add(1, Relaxed);
        return;
    };
    let rssi = if info.rx_ctrl.is_null() { 0 } else { unsafe { (*info.rx_ctrl).rssi() as i8 } };
    if h.kind == FLOOD {
        COUNTS.count(data[HEADER], h.seq, data.len(), at, rssi);
        return;
    }
    let from = unsafe { *(info.src_addr as *const [u8; 6]) };
    if let Some(tx) = RX.get() {
        if tx.try_send(Rx { from, at, rssi, data: data.to_vec() }).is_err() {
            RX_DROPPED.fetch_add(1, Relaxed);
        }
    }
}

/// ESP-NOW's send callback, on the Wi-Fi task: counts unicast frames acknowledged and failed.
unsafe extern "C" fn on_send(info: *const sys::esp_now_send_info_t, status: sys::esp_now_send_status_t) {
    let to = if info.is_null() { std::ptr::null() } else { unsafe { (*info).des_addr.cast_const() } };
    if !to.is_null() && unsafe { *(to as *const [u8; 6]) } == BROADCAST {
        return;
    }
    if status == sys::esp_now_send_status_t_ESP_NOW_SEND_SUCCESS {
        TX_OK.fetch_add(1, Relaxed);
    } else {
        TX_FAIL.fetch_add(1, Relaxed);
    }
}

fn add_peer(mac: [u8; 6]) -> anyhow::Result<()> {
    let mut peer: sys::esp_now_peer_info_t = unsafe { std::mem::zeroed() };
    peer.peer_addr = mac;
    // 0: whatever channel the station is on, so a peer stays reachable when the station joins the hotspot.
    peer.channel = 0;
    peer.ifidx = sys::wifi_interface_t_WIFI_IF_STA;
    peer.encrypt = false;
    match unsafe { sys::esp_now_add_peer(&peer) } {
        sys::ESP_OK | sys::ESP_ERR_ESPNOW_EXIST => Ok(()),
        e => Err(anyhow!("esp_now_add_peer: {e}")),
    }
}

/// Sends `frame` to `to`; ESP-IDF's error if it didn't take it.
fn send(to: &[u8; 6], frame: &[u8]) -> Result<(), sys::esp_err_t> {
    match unsafe { sys::esp_now_send(to.as_ptr(), frame.as_ptr(), frame.len()) } {
        sys::ESP_OK => Ok(()),
        e => Err(e),
    }
}

/// Blocks for `ms` ticks (1 ms each at the port's 1000 Hz), at least one, so the idle task runs (NB-133).
fn delay(ms: u32) {
    unsafe { sys::vTaskDelay(ms.max(1)) };
}

fn now_us() -> i64 {
    unsafe { sys::esp_timer_get_time() }
}

fn mac_tail(mac: &[u8; 6]) -> String {
    format!("{:02x}:{:02x}:{:02x}", mac[3], mac[4], mac[5])
}

/// The transmit power ESP-IDF is set to, in dBm.
fn tx_power() -> f32 {
    let mut p = 0i8;
    unsafe { sys::esp_wifi_get_max_tx_power(&mut p) };
    f32::from(p) / 4.0
}

/// What the last flash sector says: the node number and wifi.sh's network.
struct Config {
    node: Option<u8>,
    wifi: Option<(String, String)>,
}

fn config() -> Config {
    let mut b = [0u8; 0x110];
    let r = unsafe { sys::esp_flash_read(std::ptr::null_mut(), b.as_mut_ptr().cast(), CFG_ADDR, b.len() as u32) };
    if r != sys::ESP_OK {
        return Config { node: None, wifi: None };
    }
    let node = (b[..3] == *b"PWN" && (1..=16).contains(&b[3])).then_some(b[3]);
    Config { node, wifi: wifi_config(&b[WIFI_OFFSET..]) }
}

/// wifi.sh's record: b"PWW", the SSID's length and the SSID, the password's length and the password.
fn wifi_config(b: &[u8]) -> Option<(String, String)> {
    let (&len, rest) = b.strip_prefix(b"PWW")?.split_first()?;
    let ssid = rest.get(..len as usize).filter(|_| (1..=32).contains(&len))?;
    let (&len, rest) = rest[ssid.len()..].split_first()?;
    let password = rest.get(..len as usize).filter(|_| len <= 64)?;
    Some((String::from_utf8(ssid.to_vec()).ok()?, String::from_utf8(password.to_vec()).ok()?))
}

/// The frame codec and the percentiles, checked on the chip before any radio (so QEMU runs it too).
fn selftest() -> anyhow::Result<()> {
    for size in SIZES {
        for kind in [HELLO, PING, PONG, FLOOD, FEND, FREP, PHASE, PACK, DONE, DACK] {
            let f = frame(kind, 3, 0xdead_beef, 0x0102_0304, size);
            ensure!(f.len() == size, "frame size {} for {size}", f.len());
            ensure!(parse(&f, 0xdead_beef) == Some(Header { kind, mode: 3, seq: 0x0102_0304 }), "parse");
            ensure!(parse(&f, 0xdead_beee).is_none(), "another group parsed");
            ensure!(parse(&f[..MIN_FRAME - 1], 0xdead_beef).is_none(), "a short frame parsed");
        }
    }
    let sorted: Vec<u32> = (1..=200).collect();
    ensure!(pct(&sorted, 50) == 100 && pct(&sorted, 99) == 198 && pct(&sorted, 100) == 200, "percentiles");
    ensure!(pct(&[], 50) == -1 && pct(&[7], 99) == 7, "percentiles of 0 and 1");
    info!("ESPNOW_SELFTEST ok");
    Ok(())
}

/// The other board, once heard.
#[derive(Clone, Copy)]
struct Peer {
    mac: [u8; 6],
    tag: u32,
    version: u8,
}

/// Round trips at one size.
#[derive(Default)]
struct Pings {
    rtt: Vec<u32>,
    cb: Vec<u32>,
    lost: u32,
    send_err: u32,
    corrupt: u32,
    rssi: i32,
    state: u8,
}

/// A flood at one size, from the sender's side and the receiver's report.
#[derive(Default)]
struct Flood {
    sent: u32,
    ok: u32,
    fail: u32,
    nomem: u32,
    err: u32,
    report: Option<Report>,
}

struct Probe {
    wifi: Wifi,
    sysloop: EspSystemEventLoop,
    creds: Option<(String, String)>,
    rx: Receiver<Rx>,
    me: [u8; 6],
    node: u8,
    /// The role the flash gives, if any, and the one taken.
    cfg_role: Option<Role>,
    role: Role,
    group: u32,
    /// A random number of this boot, so the other board can tell a restart.
    tag: u32,
    channel: u8,
    version: u8,
    peer: Option<Peer>,
    mode: usize,
    on_ap: bool,
    fast: bool,
    ble: Option<Nimble>,
    ble_seen: u32,
    seq: u32,
    burst: u8,
}

fn main() {
    peerwire_esp::run(run)
}

fn run() -> anyhow::Result<()> {
    let chip = if cfg!(esp32) { "esp32" } else { "esp32c3" };
    let cfg = config();
    // The run's group: this board's own, or the initiator's once it hears its hello.
    let group = loop {
        let g = unsafe { sys::esp_random() };
        if g != 0 {
            break g;
        }
    };
    let cfg_role = match cfg.node {
        Some(1) => Some(Role::Initiator),
        Some(_) => Some(Role::Responder),
        None => None,
    };
    info!(
        "ESPNOW_BOOT chip={chip} node={} role={} wifi_record={} {}",
        cfg.node.unwrap_or(0),
        cfg_role.map_or("by-mac", Role::name),
        if cfg.wifi.is_some() { "yes" } else { "no" },
        heap()
    );
    selftest()?;

    let peripherals = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;
    let mut wifi = wifi::start_on_channel(peripherals.modem, sysloop.clone(), DEFAULT_CHANNEL)?;
    // The hotspot's channel, so the boards are on it before either joins it. The log doesn't name the network.
    let hotspot = cfg.wifi.as_ref().and_then(|(ssid, _)| match wifi.scan() {
        Ok(aps) => {
            let best = aps.iter().filter(|ap| ap.ssid == ssid.as_str()).max_by_key(|ap| ap.signal_strength);
            match best {
                Some(ap) => info!("ESPNOW_HOTSPOT channel={} rssi={}", ap.channel, ap.signal_strength),
                None => warn!("ESPNOW_HOTSPOT not heard among {} networks: channel {DEFAULT_CHANNEL}", aps.len()),
            }
            best.map(|ap| ap.channel)
        }
        Err(e) => {
            warn!("ESPNOW_HOTSPOT scan failed: {e}");
            None
        }
    });
    let channel = hotspot.filter(|c| (1..=13).contains(c)).unwrap_or(DEFAULT_CHANNEL);
    wifi::set_channel(channel)?;

    GROUP.store(group, Relaxed);
    let (tx, rx) = mpsc::sync_channel(QUEUE);
    RX.set(tx).map_err(|_| anyhow!("receive queue set twice"))?;
    sys::esp!(unsafe { sys::esp_now_init() })?;
    sys::esp!(unsafe { sys::esp_now_register_recv_cb(Some(on_recv)) })?;
    sys::esp!(unsafe { sys::esp_now_register_send_cb(Some(on_send)) })?;
    add_peer(BROADCAST)?;
    let mut version = 0u32;
    sys::esp!(unsafe { sys::esp_now_get_version(&mut version) })?;
    let mut me = [0u8; 6];
    sys::esp!(unsafe { sys::esp_wifi_get_mac(sys::wifi_interface_t_WIFI_IF_STA, me.as_mut_ptr()) })?;
    let tag = unsafe { sys::esp_random() };
    info!(
        "ESPNOW_INFO chip={chip} mac=..:{} channel={channel} hotspot_ch={} espnow_ver={version} txp={} {}",
        mac_tail(&me),
        hotspot.map_or(0, i32::from),
        tx_power(),
        heap()
    );

    let mut probe = Probe {
        wifi,
        sysloop,
        creds: cfg.wifi,
        rx,
        me,
        node: cfg.node.unwrap_or(0),
        cfg_role,
        role: cfg_role.unwrap_or(Role::Responder),
        group,
        tag,
        channel,
        version: version.min(255) as u8,
        peer: None,
        mode: 0,
        on_ap: false,
        fast: false,
        ble: None,
        ble_seen: 0,
        seq: 0,
        // Flood numbers start anew on each boot, so a responder can't take a restarted initiator's for its last run's.
        burst: tag as u8,
    };
    match cfg_role {
        // A responder with its role from the flash waits in its loop, broadcasting hellos.
        Some(Role::Responder) => {}
        _ => probe.pair(),
    }
    match probe.role {
        Role::Initiator => probe.initiate(),
        Role::Responder => probe.respond(),
    }
}

impl Probe {
    fn state(&self) -> u8 {
        (if self.on_ap { ON_AP } else { 0 }) | (if self.ble.is_some() { BLE_ON } else { 0 })
    }

    fn frame(&mut self, kind: u8, size: usize) -> Vec<u8> {
        self.seq = self.seq.wrapping_add(1);
        frame(kind, self.mode as u8, self.group, self.seq, size)
    }

    /// A hello: group 0 in its header, so any board takes it, and this board's group in its body.
    fn hello(&mut self, to: [u8; 6]) {
        let mut f = self.frame(HELLO, MIN_FRAME);
        let role = if self.peer.is_some() || self.cfg_role.is_some() { Some(self.role) } else { None };
        (f[HEADER], f[HEADER + 1], f[HEADER + 2], f[HEADER + 3]) =
            (Role::code(role), self.node, self.version, self.state());
        put_u32(&mut f, HEADER + 4, self.tag);
        put_u32(&mut f, HEADER + 8, self.group);
        put_u32(&mut f, 6, 0);
        let _ = send(&to, &f);
    }

    /// Takes the initiator's group from its hello.
    fn take_group(&mut self, hello: &[u8]) {
        let group = get_u32(hello, HEADER + 8);
        if group != 0 && group != self.group {
            self.group = group;
            GROUP.store(group, Relaxed);
            info!("ESPNOW_GROUP {group:08x}, the initiator's");
        }
    }

    /// The next frame queued before `until`.
    fn wait(&mut self, until: Instant) -> Option<Rx> {
        self.rx.recv_timeout(until.saturating_duration_since(Instant::now())).ok()
    }

    fn drain_ble(&mut self) {
        if let Some(radio) = &mut self.ble {
            while let Some(event) = radio.poll() {
                if let RadioEvent::Seen { .. } = event {
                    self.ble_seen += 1;
                }
            }
        }
    }

    /// Takes `mac` as the other board.
    fn adopt(&mut self, mac: [u8; 6], tag: u32, version: u8) {
        if let Err(e) = add_peer(mac) {
            warn!("ESPNOW_PEER {e}");
            return;
        }
        let known = self.peer.is_some_and(|p| p.mac == mac && p.tag == tag);
        if !known {
            let again = self.peer.is_some_and(|p| p.mac == mac);
            info!(
                "ESPNOW_PAIRED role={} peer=..:{} peer_ver={version} {}",
                self.role.name(),
                mac_tail(&mac),
                if again { "(it restarted)" } else { "" }
            );
        }
        self.peer = Some(Peer { mac, tag, version });
    }

    /// Broadcasts hellos until it hears the other board's, and takes a role.
    fn pair(&mut self) {
        let start = Instant::now();
        let (mut next_hello, mut next_log) = (start, start + Duration::from_secs(10));
        loop {
            let now = Instant::now();
            if now >= next_hello {
                self.hello(BROADCAST);
                let jitter = Duration::from_millis(u64::from(unsafe { sys::esp_random() } % 100));
                next_hello = now + HELLO_EVERY + jitter;
            }
            if now >= next_log {
                info!("ESPNOW_WAIT no board heard after {} s on channel {}", start.elapsed().as_secs(), self.channel);
                next_log += Duration::from_secs(10);
            }
            let Some(rx) = self.wait(next_hello) else { continue };
            let Some(h) = parse(&rx.data, self.group) else { continue };
            if h.kind != HELLO {
                continue;
            }
            let theirs = Role::from_code(rx.data[HEADER]);
            let role = match (self.cfg_role, theirs) {
                (Some(a), Some(b)) if a == b => {
                    warn!("ESPNOW_CONFLICT both boards are {}s: give them nodes 1 and 2", a.name());
                    continue;
                }
                (Some(a), _) => a,
                (None, Some(b)) => b.other(),
                (None, None) if self.me < rx.from => Role::Initiator,
                (None, None) => Role::Responder,
            };
            self.role = role;
            if role == Role::Responder {
                self.take_group(&rx.data);
            }
            self.adopt(rx.from, get_u32(&rx.data, HEADER + 4), rx.data[HEADER + 2]);
            self.hello(rx.from);
            info!("ESPNOW_ROLE {} after {} ms", role.name(), start.elapsed().as_millis());
            return;
        }
    }

    /// Makes this board's part of mode `m` so: the rate to the peer, the hotspot, Bluetooth.
    fn apply(&mut self, m: usize) {
        let mode = &MODES[m];
        let side = usize::from(self.role == Role::Responder);
        if mode.fast != self.fast {
            if let Some(peer) = self.peer {
                self.fast = mode.fast;
                if let Err(e) = set_rate(peer.mac, mode.fast) {
                    warn!("ESPNOW_RATE {e}");
                }
            }
        }
        if mode.ap[side] && !self.on_ap {
            match self.creds.clone() {
                None => warn!("ESPNOW_AP no Wi-Fi record in flash: staying off the hotspot"),
                Some((ssid, password)) => {
                    let t = Instant::now();
                    match wifi::connect(&mut self.wifi, &self.sysloop, &ssid, &password, JOIN_WITHIN) {
                        Ok(()) => {
                            self.on_ap = true;
                            let ch = wifi::channel().unwrap_or(0);
                            info!(
                                "ESPNOW_AP joined in {} ms, channel {ch}, rssi {}, txp {}",
                                t.elapsed().as_millis(),
                                wifi::rssi().map_or(0, i32::from),
                                tx_power()
                            );
                            if ch != self.channel {
                                warn!(
                                    "ESPNOW_AP the hotspot is on channel {ch}, not {}: the boards may miss each other",
                                    self.channel
                                );
                            }
                        }
                        Err(e) => {
                            warn!("ESPNOW_AP join failed: {e:#}");
                            if let Err(e) = wifi::set_channel(self.channel) {
                                warn!("ESPNOW_AP back to channel {}: {e:#}", self.channel);
                            }
                        }
                    }
                }
            }
        }
        if !mode.ap[side] && self.on_ap {
            if let Err(e) = self.wifi.disconnect() {
                warn!("ESPNOW_AP leaving: {e}");
            }
            self.on_ap = false;
            match wifi::set_channel(self.channel) {
                Ok(()) => info!("ESPNOW_AP left, back on channel {}", self.channel),
                Err(e) => warn!("ESPNOW_AP left, but channel {}: {e:#}", self.channel),
            }
        }
        if mode.ble && self.ble.is_none() {
            match Nimble::new() {
                Ok(mut radio) => {
                    // Any service data: only the radio's load matters here.
                    let mut data = [0u8; 9];
                    unsafe { sys::esp_fill_random(data.as_mut_ptr().cast(), data.len()) };
                    radio.advertise(Some(&data));
                    radio.scan(true);
                    self.ble = Some(radio);
                    info!("ESPNOW_BLE advertising and scanning, {}", heap());
                }
                Err(e) => warn!("ESPNOW_BLE didn't start: {e:#}"),
            }
        }
        self.mode = m;
    }

    // The initiator.

    fn initiate(&mut self) -> anyhow::Result<()> {
        let started = Instant::now();
        let (mut results, mut skipped) = (0, 0);
        for m in 0..MODES.len() {
            let name = MODES[m].name;
            info!("ESPNOW_MODE {name}: starting");
            self.apply(m);
            let Some(peer_state) = self.sync(m) else {
                warn!("ESPNOW_MODE {name}: the responder didn't answer in {} s", SYNC_WITHIN.as_secs());
                for size in SIZES {
                    info!("ESPNOW_RESULT mode={name} size={size} skipped=no_sync");
                    skipped += 1;
                }
                continue;
            };
            for size in SIZES {
                let version = self.peer.map_or(1, |p| p.version).min(self.version);
                if size > V1_MAX && version < 2 {
                    info!("ESPNOW_RESULT mode={name} size={size} skipped=espnow_v{version}");
                    skipped += 1;
                    continue;
                }
                let pings = self.pings(size);
                let flood = self.flood(size);
                self.result(name, size, &pings, &flood, peer_state);
                results += 1;
            }
        }
        // The responder ends too.
        let until = Instant::now() + Duration::from_secs(3);
        let mut acked = false;
        while !acked && Instant::now() < until {
            if let Some(peer) = self.peer {
                let f = self.frame(DONE, MIN_FRAME);
                let _ = send(&peer.mac, &f);
            }
            let wait = Instant::now() + Duration::from_millis(200);
            while let Some(rx) = self.wait(wait) {
                if parse(&rx.data, self.group).is_some_and(|h| h.kind == DACK) {
                    acked = true;
                    break;
                }
            }
        }
        info!(
            "ESPNOW_DONE role=initiator results={results} skipped={skipped} responder_acked={} took_s={} rx_dropped={} \
             foreign={} {}",
            u8::from(acked),
            started.elapsed().as_secs(),
            RX_DROPPED.load(Relaxed),
            FOREIGN.load(Relaxed),
            heap()
        );
        loop {
            self.drain_ble();
            while self.rx.try_recv().is_ok() {}
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    /// Pings until the responder answers in mode `m`, telling it the mode while it doesn't. The responder's state,
    /// or `None` if it didn't answer in time.
    fn sync(&mut self, m: usize) -> Option<u8> {
        let start = Instant::now();
        let mut next = start;
        while start.elapsed() < SYNC_WITHIN {
            self.drain_ble();
            if Instant::now() >= next {
                next = Instant::now() + Duration::from_millis(250);
                if let Some(peer) = self.peer {
                    // The group, again: the responder takes nothing else until it has it.
                    self.hello(peer.mac);
                    if m > 0 {
                        let mut f = self.frame(PHASE, MIN_FRAME);
                        f[HEADER] = m as u8;
                        let _ = send(&peer.mac, &f);
                    }
                    let f = self.frame(PING, MIN_FRAME);
                    let _ = send(&peer.mac, &f);
                }
            }
            let Some(rx) = self.wait(next) else { continue };
            match parse(&rx.data, self.group) {
                Some(h) if h.kind == PONG && usize::from(h.mode) == m => {
                    let state = rx.data[HEADER];
                    info!(
                        "ESPNOW_MODE {}: in step after {} ms, responder ap={} ble={}",
                        MODES[m].name,
                        start.elapsed().as_millis(),
                        state & ON_AP,
                        (state & BLE_ON) >> 1
                    );
                    return Some(state);
                }
                Some(h) if h.kind == HELLO => self.heard_hello(&rx),
                _ => {}
            }
        }
        None
    }

    /// A hello in the middle of the run: the responder restarted, and is in mode `a` again.
    fn heard_hello(&mut self, rx: &Rx) {
        if Role::from_code(rx.data[HEADER]) == Some(self.role) {
            return;
        }
        self.adopt(rx.from, get_u32(&rx.data, HEADER + 4), rx.data[HEADER + 2]);
    }

    fn pings(&mut self, size: usize) -> Pings {
        let mut out = Pings { rtt: Vec::with_capacity(PINGS), cb: Vec::with_capacity(PINGS), ..Pings::default() };
        let Some(peer) = self.peer else { return out };
        for _ in 0..PINGS {
            self.drain_ble();
            let f = self.frame(PING, size);
            let seq = self.seq;
            let t0 = now_us();
            if send(&peer.mac, &f).is_err() {
                out.send_err += 1;
                out.lost += 1;
                delay(5);
                continue;
            }
            let deadline = Instant::now() + PING_TIMEOUT;
            loop {
                let Some(rx) = self.wait(deadline) else {
                    out.lost += 1;
                    break;
                };
                let t1 = now_us();
                match parse(&rx.data, self.group) {
                    Some(h) if h.kind == PONG && h.seq == seq => {
                        if rx.data.len() != f.len() || rx.data[HEADER + 1..] != f[HEADER + 1..] {
                            out.corrupt += 1;
                        }
                        out.rtt.push((t1 - t0).clamp(0, i64::from(u32::MAX)) as u32);
                        out.cb.push((rx.at - t0).clamp(0, i64::from(u32::MAX)) as u32);
                        out.rssi += i32::from(rx.rssi);
                        out.state = rx.data[HEADER];
                        break;
                    }
                    Some(h) if h.kind == HELLO => self.heard_hello(&rx),
                    _ => {}
                }
            }
            delay(PING_GAP_MS);
        }
        out.rtt.sort_unstable();
        out.cb.sort_unstable();
        out
    }

    fn flood(&mut self, size: usize) -> Flood {
        let mut out = Flood::default();
        let Some(peer) = self.peer else { return out };
        self.burst = self.burst.wrapping_add(1);
        let mut f = self.frame(FLOOD, size);
        f[HEADER] = self.burst;
        let (ok0, fail0) = (TX_OK.load(Relaxed), TX_FAIL.load(Relaxed));
        let done = || TX_OK.load(Relaxed).wrapping_sub(ok0) + TX_FAIL.load(Relaxed).wrapping_sub(fail0);
        let start = Instant::now();
        let mut yielded = start;
        while start.elapsed() < FLOOD_FOR && out.sent < FLOOD_MAX {
            // Never long without a block, so the idle task runs on the C3's one core.
            if out.sent.saturating_sub(done()) >= WINDOW || yielded.elapsed() >= Duration::from_millis(5) {
                delay(1);
                yielded = Instant::now();
                continue;
            }
            put_u32(&mut f, 10, out.sent);
            match send(&peer.mac, &f) {
                Ok(()) => out.sent += 1,
                Err(sys::ESP_ERR_ESPNOW_NO_MEM) => {
                    out.nomem += 1;
                    delay(1);
                    yielded = Instant::now();
                }
                Err(_) => {
                    out.err += 1;
                    delay(1);
                    yielded = Instant::now();
                }
            }
        }
        // The last frames' callbacks.
        let wait = Instant::now();
        while done() < out.sent && wait.elapsed() < Duration::from_secs(1) {
            delay(1);
        }
        out.ok = TX_OK.load(Relaxed).wrapping_sub(ok0);
        out.fail = TX_FAIL.load(Relaxed).wrapping_sub(fail0);
        // The responder's count.
        let until = Instant::now() + Duration::from_secs(2);
        let mut next = Instant::now();
        while out.report.is_none() && Instant::now() < until {
            if Instant::now() >= next {
                next = Instant::now() + Duration::from_millis(300);
                let mut end = self.frame(FEND, MIN_FRAME);
                end[HEADER] = self.burst;
                put_u32(&mut end, HEADER + 1, out.sent);
                let _ = send(&peer.mac, &end);
            }
            let Some(rx) = self.wait(next.min(until)) else { continue };
            if parse(&rx.data, self.group).is_some_and(|h| h.kind == FREP) && rx.data[HEADER] == self.burst {
                let d = &rx.data;
                out.report = Some(Report {
                    count: get_u32(d, HEADER + 1),
                    unique: get_u32(d, HEADER + 5),
                    ooo: get_u32(d, HEADER + 9),
                    span_us: get_u32(d, HEADER + 13),
                    rssi: d[HEADER + 17] as i8,
                });
            }
        }
        out
    }

    fn result(&self, name: &str, size: usize, p: &Pings, f: &Flood, peer_state: u8) {
        let ok = p.rtt.len();
        let loss = 100.0 * f64::from(p.lost) / PINGS as f64;
        let r = f.report.unwrap_or_default();
        let fps =
            if r.unique > 1 && r.span_us > 0 { f64::from(r.unique - 1) * 1e6 / f64::from(r.span_us) } else { 0.0 };
        let flood_loss = if f.sent > 0 { 100.0 * (1.0 - f64::from(r.unique) / f64::from(f.sent)) } else { 0.0 };
        let rssi = if ok > 0 { p.rssi / ok as i32 } else { 0 };
        let state = if ok > 0 { p.state } else { peer_state };
        info!(
            "ESPNOW_RESULT mode={name} ch={} size={size} n={PINGS} ok={ok} loss={loss:.2}% p50_us={} p90_us={} \
             p99_us={} max_us={} cb_p50_us={} cb_p99_us={} fps={fps:.0} kbps={:.0} flood_sent={} flood_rcvd={} \
             flood_loss={flood_loss:.2}% tx_fail={} nomem={} send_err={} dup={} ooo={} corrupt={} rssi={rssi} \
             peer_rssi={} ap={}/{} ble={}/{} txp={} ble_seen={} report={} {}",
            wifi::channel().unwrap_or(0),
            pct(&p.rtt, 50),
            pct(&p.rtt, 90),
            pct(&p.rtt, 99),
            pct(&p.rtt, 100),
            pct(&p.cb, 50),
            pct(&p.cb, 99),
            fps * size as f64 * 8.0 / 1000.0,
            f.sent,
            r.unique,
            f.fail,
            f.nomem,
            p.send_err + f.err,
            r.count.saturating_sub(r.unique),
            r.ooo,
            p.corrupt,
            r.rssi,
            u8::from(self.on_ap),
            state & ON_AP,
            u8::from(self.ble.is_some()),
            (state & BLE_ON) >> 1,
            tx_power(),
            self.ble_seen,
            u8::from(f.report.is_some()),
            heap()
        );
    }

    // The responder.

    fn respond(&mut self) -> anyhow::Result<()> {
        let mut heard = Instant::now();
        let mut next_hello = Instant::now();
        let mut done = false;
        // The last flood logged: the initiator sends a flood's end again until it hears the report.
        let mut logged = None;
        loop {
            self.drain_ble();
            let now = Instant::now();
            // Hellos until the initiator is heard, and again whenever it goes quiet.
            if now >= next_hello && (self.peer.is_none() || heard.elapsed() >= QUIET) && !done {
                self.hello(BROADCAST);
                next_hello = now + HELLO_EVERY;
            }
            if self.peer.is_some() && !done && heard.elapsed() >= LOST_AFTER {
                info!(
                    "ESPNOW_DONE role=responder status=lost quiet_s={} rx_dropped={} foreign={} {}",
                    heard.elapsed().as_secs(),
                    RX_DROPPED.load(Relaxed),
                    FOREIGN.load(Relaxed),
                    heap()
                );
                done = true;
            }
            let Some(rx) = self.wait(Instant::now() + Duration::from_millis(100)) else { continue };
            let Some(h) = parse(&rx.data, self.group) else { continue };
            let from_peer = self.peer.is_some_and(|p| p.mac == rx.from);
            match h.kind {
                HELLO => {
                    if Role::from_code(rx.data[HEADER]) == Some(Role::Responder) {
                        warn!("ESPNOW_CONFLICT another responder at ..:{}", mac_tail(&rx.from));
                        continue;
                    }
                    // Another pair's initiator nearby doesn't move this board to its group.
                    if self.peer.is_none() || from_peer {
                        self.take_group(&rx.data);
                    }
                    let tag = get_u32(&rx.data, HEADER + 4);
                    // Tag 0: taken up from a ping, before its hello.
                    if from_peer && self.peer.is_some_and(|p| p.tag != 0 && p.tag != tag) {
                        // The initiator restarted and begins at mode `a` again.
                        self.apply(0);
                        logged = None;
                    }
                    self.adopt(rx.from, tag, rx.data[HEADER + 2]);
                    heard = Instant::now();
                }
                _ if !from_peer && self.peer.is_some() => continue,
                PING => {
                    if self.peer.is_none() {
                        self.adopt(rx.from, 0, 1);
                    }
                    heard = Instant::now();
                    let mut f = rx.data;
                    (f[4], f[5], f[HEADER]) = (PONG, self.mode as u8, self.state());
                    let _ = send(&rx.from, &f);
                }
                FEND => {
                    heard = Instant::now();
                    let burst = rx.data[HEADER];
                    let r = COUNTS.report(burst);
                    let mut f = self.frame(FREP, MIN_FRAME);
                    f[HEADER] = burst;
                    put_u32(&mut f, HEADER + 1, r.count);
                    put_u32(&mut f, HEADER + 5, r.unique);
                    put_u32(&mut f, HEADER + 9, r.ooo);
                    put_u32(&mut f, HEADER + 13, r.span_us);
                    f[HEADER + 17] = r.rssi as u8;
                    let _ = send(&rx.from, &f);
                    if logged != Some(burst) {
                        logged = Some(burst);
                        info!(
                            "ESPNOW_RX mode={} size={} sent={} count={} unique={} ooo={} span_us={} rssi={}",
                            MODES[self.mode].name,
                            COUNTS.size.load(Relaxed),
                            get_u32(&rx.data, HEADER + 1),
                            r.count,
                            r.unique,
                            r.ooo,
                            r.span_us,
                            r.rssi
                        );
                    }
                }
                PHASE => {
                    if self.peer.is_none() {
                        self.adopt(rx.from, 0, 1);
                    }
                    heard = Instant::now();
                    let m = usize::from(rx.data[HEADER]);
                    let mut f = self.frame(PACK, MIN_FRAME);
                    f[HEADER] = m as u8;
                    let _ = send(&rx.from, &f);
                    if m > self.mode && m < MODES.len() {
                        info!("ESPNOW_MODE {}: starting", MODES[m].name);
                        self.apply(m);
                        info!("ESPNOW_MODE {}: ready, {}", MODES[m].name, heap());
                        heard = Instant::now();
                    }
                }
                DONE => {
                    let f = self.frame(DACK, MIN_FRAME);
                    let _ = send(&rx.from, &f);
                    if !done {
                        info!(
                            "ESPNOW_DONE role=responder status=ok rx_dropped={} foreign={} {}",
                            RX_DROPPED.load(Relaxed),
                            FOREIGN.load(Relaxed),
                            heap()
                        );
                        done = true;
                    }
                }
                _ => {}
            }
        }
    }
}

/// 54 Mbit/s 802.11g to `peer`, or back to ESP-NOW's default rate (a peer entry made anew has it).
fn set_rate(peer: [u8; 6], fast: bool) -> anyhow::Result<()> {
    if fast {
        let mut rate: sys::esp_now_rate_config_t = unsafe { std::mem::zeroed() };
        rate.phymode = sys::wifi_phy_mode_t_WIFI_PHY_MODE_11G;
        rate.rate = sys::wifi_phy_rate_t_WIFI_PHY_RATE_54M;
        sys::esp!(unsafe { sys::esp_now_set_peer_rate_config(peer.as_ptr(), &mut rate) })?;
        info!("ESPNOW_RATE 54 Mbit/s 802.11g to ..:{}", mac_tail(&peer));
    } else {
        sys::esp!(unsafe { sys::esp_now_del_peer(peer.as_ptr()) })?;
        add_peer(peer)?;
        info!("ESPNOW_RATE default to ..:{}", mac_tail(&peer));
    }
    Ok(())
}

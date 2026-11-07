//! The Linux radio driver (feature `ble-bluez`): BlueZ over D-Bus through `bluer`, on a thread of its own with a
//! one-thread tokio runtime, behind the non-blocking [`Radio`] the node polls.
//!
//! As the peripheral it takes one central at a time (BlueZ sends a characteristic's notifications to every subscribed
//! central, BLE-R8): a central counts as connected once it subscribes to `tx`, which BlueZ hands over as a socket with
//! the subscriber's address (`AcquireNotify`), and only that device's plain writes without response at offset 0 reach
//! the node (BLE-R7, NB110-8). As the central it connects, finds the service and takes the `rx` writer and `tx`
//! notifications as sockets. bluer's sockets take values of up to 5 bytes under the ATT MTU, so the node is told an
//! MTU that keeps its values to that, and a connection whose socket takes fewer than 20 bytes (an ATT MTU of 23 or 24)
//! isn't used. Nothing is paired. BlueZ picks the addresses it advertises and connects from, and its GAP name is the
//! adapter's alias: see [`BlueZ::alias`] and `docs/concepts/bluetooth.md`.
//!
//! It keeps the radio up (NB-112). A part that fails (the adapter, the GATT service, the advert, the scan) is logged
//! through the `log` crate and tried again 1, 2, 4 … up to 30 s later, until it works; an advert that isn't on the air
//! is reported ([`RadioEvent::Advertising`]). When the adapter goes (a USB reset brings it back as another), stops
//! answering (bluetoothd crashed) or comes again (bluetoothd restarted, forgetting what was set up on it), or another
//! becomes the default, the links on it are reported closed and the driver sets up on the default adapter. One powered
//! off has its links reported closed too, and is set up again once powered on: the driver powers an adapter on only
//! at start, never one its user turned off (BlueZ's `AutoEnable` powers on one it finds, as after a USB reset).
//!
//! A link that drops is reported closed at once, whoever dropped it: as the central's, once BlueZ says the device
//! isn't connected or shuts the notification socket, which a read sees as an empty value (NB-114). A connect that
//! fails is logged with the step it failed at and BlueZ's error, and a link that goes down with why; a device is named
//! by a hash that holds for the run alone. The driver's connects make their links one at a time, as the kernel does,
//! and the scan is held off for each, once it has run 2 s: with it on, Linux starts a connect only at the scan's next
//! report of the device, up to 10.24 s on ([`Discovery`], NB-116). One whose link isn't up 3 s later fails, and is
//! disconnected before the scan starts again, which the kernel would start behind it (NB116-2).
//!
//! BlueZ's input plugin takes a device that serves HID as an input device, on the driver's links too, and asks to pair
//! with one that isn't paired first, unless `/etc/bluetooth/input.conf` sets `LEAutoSecurity` false, as the driver
//! logs at start if not. So a device that lists HID isn't used, and one BlueZ had neither paired nor trusted before
//! the driver connected to it is removed from BlueZ once the last of the driver's connects and links on it ends
//! ([`Strangers`], NB110-13).

use crate::ble::{tag, ConnId, DeviceId, Limits, Radio, RadioEvent, RX_UUID, SERVICE_UUID, TX_UUID};
use bluer::adv::{Advertisement, AdvertisementHandle, Type};
use bluer::gatt::local::{
    characteristic_control, Application, ApplicationHandle, Characteristic, CharacteristicControl,
    CharacteristicControlEvent, CharacteristicControlHandle, CharacteristicNotify, CharacteristicNotifyMethod,
    CharacteristicWrite, CharacteristicWriteMethod, CharacteristicWriteRequest, ReqError, Service,
};
use bluer::gatt::{CharacteristicWriter, WriteOp};
use bluer::{
    Adapter, AdapterEvent, AdapterProperty, Address, DeviceEvent, DeviceProperty, DiscoveryFilter, DiscoveryTransport,
    Session, SessionEvent, Uuid,
};
use futures::future::BoxFuture;
use futures::stream::{BoxStream, LocalBoxStream, Stream};
use futures::{FutureExt, StreamExt};
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::task::Waker;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// Values queued per connection in the driver; beyond that, [`Radio::send`] says no and the node keeps them.
const QUEUE: usize = 32;
/// How long [`BlueZ::new`] waits for the adapter.
const START: Duration = Duration::from_secs(10);
/// A connect not set up this long after it was asked for (its turn ([`Ctx::turn`]), the link, the service found, both
/// sockets) fails, and its link drops if it was asked for: so a device that stalls it can't hold one of the node's
/// connects for bluer's 120 s a step, nor connects queued ahead of it hold it past the node's wait (NB116-4); the
/// node, which gives one up after 12 s, hears why first, as a link not made is disconnected, `ABORT` at most, before
/// it is reported (NB110-11).
const SETUP: Duration = Duration::from_secs(10);
/// A connect whose link isn't up this long after the scan is held off for it fails ([`linked`]): time for a device that
/// advertises each 1.28 s, as BlueZ does by default, to be heard twice, its link up 0.13 s after it is (NB116-2).
const LINK: Duration = Duration::from_secs(3);
/// How long a connect whose link wasn't made waits for BlueZ to disconnect it, which stops the kernel's attempt, before
/// the next connect's turn and the scan: BlueZ answers at once for a device that isn't connected (NB116-2).
const ABORT: Duration = Duration::from_secs(1);
/// How long a driver that closes waits at most for BlueZ to remove the strangers it was linked to ([`close`]): one
/// linked is removed in about 2 s, as BlueZ drops a link 2 s after it is asked to.
const CLOSE: Duration = Duration::from_secs(3);
/// A connect holds the scan off this long at most ([`Discovery`]): more than `LINK` and `ABORT`, so that it is the
/// connect, given up and disconnected, that ends a pause, unless BlueZ doesn't answer.
const PAUSE: Duration = Duration::from_secs(5);
/// The scan runs this long at least each time it starts, before a connect holds it off: more than 1.28 s, so that it
/// hears each device in range each time, however many connects there are and however long they take (NB116-1).
const RUN: Duration = Duration::from_secs(2);
/// A part that failed is tried again this long after, doubled at each failure in a row, up to `RETRY_MAX`.
const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);
/// How often the adapter in use is asked whether it is powered: a bluetoothd that crashes says nothing.
const CHECK: Duration = Duration::from_secs(10);
/// Devices the scan follows at once ([`Watching`]).
const WATCH: usize = 256;
/// How often at most the scan lists BlueZ's devices again, for those it left out ([`Watching`]).
const RELIST: Duration = Duration::from_secs(30);
/// The HID service, 0x1812, which BlueZ's input plugin takes as an input device.
const HID_UUID: u128 = 0x0000_1812_0000_1000_8000_0080_5f9b_34fb;
/// The input plugin's settings, `LEAutoSecurity` among them ([`le_auto_security`]).
const INPUT_CONF: &str = "/etc/bluetooth/input.conf";

/// The Linux driver: the default BlueZ adapter, powered on, serving the Peerwire GATT service.
///
/// ```no_run
/// use peerwire::ble::BlueZ;
/// use peerwire::Node;
///
/// let radio = BlueZ::new()?;
/// let mut node = Node::builder().bluetooth(radio).bind("0.0.0.0:0")?;
/// node.join_room("my-app", "kitchen"); // links with members nearby, over Bluetooth if there is no network
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// Dropped, it closes its links, and waits up to 3 s for BlueZ to remove the devices among them that it had neither
/// paired nor trusted (`docs/concepts/bluetooth.md`).
pub struct BlueZ {
    cmds: mpsc::UnboundedSender<Cmd>,
    events: std_mpsc::Receiver<RadioEvent>,
    shared: Arc<Shared>,
    alias: String,
    /// Disconnected once the driver's thread has ended, its runtime dropped.
    done: std_mpsc::Receiver<()>,
}

enum Cmd {
    Advertise(Option<Vec<u8>>),
    Scan(bool),
    Connect(DeviceId),
    Disconnect(ConnId),
}

/// What the driver's thread and the node's side share.
struct Shared {
    /// Where each connection's values go out.
    senders: Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>,
    next: AtomicU32,
    /// The node's, woken with each event ([`Radio::set_waker`]).
    waker: Mutex<Option<Waker>>,
}

/// Where the driver's thread reports to the node: each event queued, and the node woken.
#[derive(Clone)]
struct Events {
    tx: std_mpsc::Sender<RadioEvent>,
    shared: Arc<Shared>,
}

impl Events {
    fn send(&self, e: RadioEvent) {
        if self.tx.send(e).is_ok() {
            if let Some(w) = self.shared.waker.lock().unwrap().as_ref() {
                w.wake_by_ref();
            }
        }
    }
}

impl BlueZ {
    /// The default adapter, powered on, serving the GATT service. Fails if BlueZ isn't running, there is no adapter,
    /// or this process may not use it over D-Bus. Once it runs, what fails is logged and tried again, and it follows
    /// the default adapter (`docs/concepts/bluetooth.md`).
    pub fn new() -> io::Result<BlueZ> {
        let (cmds, rx) = mpsc::unbounded_channel();
        let (events_tx, events) = std_mpsc::channel();
        let shared =
            Arc::new(Shared { senders: Mutex::new(HashMap::new()), next: AtomicU32::new(1), waker: Mutex::new(None) });
        let (ready_tx, ready) = std_mpsc::channel();
        let (done_tx, done) = std_mpsc::channel::<()>();
        let s = shared.clone();
        std::thread::Builder::new().name("peerwire-bluez".into()).spawn(move || {
            // Dropped last, after the runtime.
            let _done = done_tx;
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => return drop(ready_tx.send(Err(e))),
            };
            let events = Events { tx: events_tx, shared: s.clone() };
            rt.block_on(Driver::run(rx, events, s, ready_tx));
        })?;
        let alias = ready.recv_timeout(START).map_err(|_| io::Error::other("BlueZ didn't answer in time"))??;
        if le_auto_security(&std::fs::read_to_string(INPUT_CONF).unwrap_or_default()) {
            log::info!(
                "Bluetooth: LEAutoSecurity isn't false in {INPUT_CONF}, so BlueZ asks to pair with a device that \
                 serves HID, on this driver's links too, to take it as an input device: set it to false under \
                 [General] and restart bluetoothd (docs/concepts/bluetooth.md)"
            );
        }
        Ok(BlueZ { cmds, events, shared, alias, done })
    }

    /// The adapter's alias, which is its GAP name: what a central that connects, or a phone's device picker, reads.
    /// BlueZ's default is the host name; `bluetoothctl system-alias Peerwire` gives it one that names no one
    /// (BLE-R1). This driver doesn't rename the adapter, which is the whole machine's.
    pub fn alias(&self) -> &str {
        &self.alias
    }

    /// Whether the [`alias`](BlueZ::alias) is this machine's host name, which any central nearby can then read: an app
    /// should tell its user how to set another (NB110-5).
    pub fn names_this_host(&self) -> bool {
        let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
        same_name(&self.alias, &host)
    }
}

impl Drop for BlueZ {
    fn drop(&mut self) {
        // The driver closes as its commands end, and this is their one sender ([`close`]).
        self.cmds = mpsc::unbounded_channel().0;
        let _ = self.done.recv_timeout(CLOSE);
    }
}

/// Whether an adapter alias is a host name, ignoring case and the white space around it.
fn same_name(alias: &str, host: &str) -> bool {
    let host = host.trim();
    !host.is_empty() && alias.trim().eq_ignore_ascii_case(host)
}

/// Whether BlueZ's input plugin, given `conf` as its `input.conf`, asks to pair with a device that serves HID on a
/// link that isn't paired, to take it as an input device (`LEAutoSecurity`, true unless set false). As GLib reads
/// the file: the last `LEAutoSecurity` in a `[General]` group, false if it is `false` or `0`, white space aside; and
/// true if a line isn't a comment, a group or a key with a valid name, or a key comes before any group, which fails
/// the whole file.
fn le_auto_security(conf: &str) -> bool {
    // GLib's white space is ASCII's, without the vertical tab, as Rust's is; and it drops a `\r` before a `\n`.
    let space = |c: char| c.is_ascii_whitespace();
    let group_name = |g: &str| !g.is_empty() && !g.contains('[') && !g.contains(|c: char| c.is_ascii_control());
    // A name and, in brackets, a locale: `Name[en_GB]`.
    let key_name = |k: &str| {
        let (name, locale) = k.split_once('[').map_or((k, None), |(n, l)| (n, Some(l)));
        let locale_ok = |l: &str| l.chars().all(|c| c.is_alphanumeric() || "-_.@".contains(c));
        !name.is_empty() && !name.contains(']') && locale.is_none_or(|l| l.strip_suffix(']').is_some_and(locale_ok))
    };
    let (mut group, mut value) = (None, None);
    for line in conf.lines().map(|l| l.trim_start_matches(space)) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[') {
            match name.split_once(']') {
                Some((name, rest)) if rest.trim_matches([' ', '\t']).is_empty() && group_name(name) => {
                    group = Some(name)
                }
                _ => return true,
            }
        } else if let Some((key, v)) = line.split_once('=') {
            let key = key.trim_end_matches(space);
            if group.is_none() || !key_name(key) {
                return true;
            }
            if group == Some("General") && key == "LEAutoSecurity" {
                value = Some(v.trim_matches(space));
            }
        } else {
            return true;
        }
    }
    !matches!(value, Some("false" | "0"))
}

impl Radio for BlueZ {
    fn advertise(&mut self, data: Option<&[u8]>) {
        let _ = self.cmds.send(Cmd::Advertise(data.map(<[u8]>::to_vec)));
    }

    fn scan(&mut self, on: bool) {
        let _ = self.cmds.send(Cmd::Scan(on));
    }

    fn connect(&mut self, device: DeviceId) {
        let _ = self.cmds.send(Cmd::Connect(device));
    }

    fn send(&mut self, conn: ConnId, value: &[u8]) -> bool {
        let senders = self.shared.senders.lock().unwrap();
        match senders.get(&conn.0) {
            Some(tx) => !matches!(tx.try_send(value.to_vec()), Err(mpsc::error::TrySendError::Full(_))),
            None => true, // closed: dropped, as the node will hear
        }
    }

    fn disconnect(&mut self, conn: ConnId) {
        self.shared.senders.lock().unwrap().remove(&conn.0);
        let _ = self.cmds.send(Cmd::Disconnect(conn));
    }

    fn poll(&mut self) -> Option<RadioEvent> {
        self.events.try_recv().ok()
    }

    fn limits(&self) -> Limits {
        Limits::DESKTOP
    }

    fn set_waker(&mut self, waker: Waker) {
        *self.shared.waker.lock().unwrap() = Some(waker);
    }
}

fn uuid(u: u128) -> Uuid {
    Uuid::from_u128(u)
}

/// The ATT MTU to tell the node for a socket that takes values of up to `len` bytes (bluer keeps them 5 under the
/// real one), so that its values fit: `len + 3`. `None` under the codec's least value, 20 bytes.
fn att_mtu(len: usize) -> Option<u16> {
    (len >= usize::from(crate::ble::MIN_ATT_MTU) - 3).then(|| u16::try_from(len + 3).unwrap_or(u16::MAX))
}

/// Closes the link to `addr`, unless this radio connected to it as a central.
async fn drop_link(bluez: &impl Devices, centrals: &Centrals, addr: Address) {
    if centrals.lock().unwrap().values().any(|l| l.addr == addr) {
        return;
    }
    let _ = bluez.disconnect(addr).await;
}

fn device_id(a: Address) -> DeviceId {
    let mut b = [0u8; 8];
    b[2..].copy_from_slice(&a.0);
    DeviceId(u64::from_be_bytes(b))
}

/// `s` with each Bluetooth address in it masked (`AA:BB:CC:DD:EE:FF`, or `AA_BB_CC_DD_EE_FF` in a D-Bus path), for
/// the log: BlueZ's errors may name a device (NB-114).
fn masked(s: &str) -> String {
    let address_at = |b: &[u8]| {
        let hex = |k: usize| b[3 * k].is_ascii_hexdigit() && b[3 * k + 1].is_ascii_hexdigit();
        b.len() >= 17 && (0..6).all(hex) && (0..5).all(|k| matches!(b[3 * k + 2], b':' | b'_'))
    };
    let (mut out, mut rest) = (String::with_capacity(s.len()), s);
    while let Some(c) = rest.chars().next() {
        let skip = if address_at(rest.as_bytes()) { 17 } else { c.len_utf8() };
        out.push_str(if skip == 17 { "<address>" } else { &rest[..skip] });
        rest = &rest[skip..];
    }
    out
}

fn address(d: DeviceId) -> Address {
    let b = d.0.to_be_bytes();
    Address::new([b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// A connection this radio made as the central: the tasks writing and reading its sockets, and the device.
struct Link {
    write: JoinHandle<()>,
    read: JoinHandle<()>,
    addr: Address,
    /// Its device's use, if it is a stranger ([`Strangers`]): it ends as the link leaves the books.
    _touch: Option<Touch>,
}

/// The central connections, by number.
type Centrals = Arc<Mutex<HashMap<u32, Link>>>;

/// The devices the driver connects to as the central that BlueZ had neither paired nor trusted then, each with how
/// many of the driver's connects and links are on it (NB110-13). Each is removed from BlueZ once none is
/// ([`remove_stranger`]), so that nothing BlueZ set up on a link the driver made stays: a bond, an input device (its
/// input plugin takes a device that serves HID as one), or the device itself, stored for good. One whose removal
/// failed is kept, with none, to be removed again ([`Driver::sweep`]). A device paired or trusted before the driver
/// first connected to it is the user's, and never removed; nor is one whose link was up as a connect to it started,
/// unless it is one already, as the driver didn't make that link (NB110-16).
#[derive(Default)]
struct Strangers(HashMap<Address, usize>);

impl Strangers {
    /// A connect to `addr` starts, which is `known` if BlueZ has paired or trusted it, or its link is up already:
    /// whether it counts, as a stranger now or one already.
    fn touch(&mut self, addr: Address, known: bool) -> bool {
        match self.0.get_mut(&addr) {
            Some(n) => *n += 1,
            None if known => return false,
            None => drop(self.0.insert(addr, 1)),
        }
        true
    }

    /// One of `addr`'s connects or links ended: whether none is left now, and it is to be removed.
    fn release(&mut self, addr: Address) -> bool {
        self.0.get_mut(&addr).is_some_and(|n| {
            *n = n.saturating_sub(1);
            *n == 0
        })
    }

    /// Whether one of the driver's connects or links is on `addr`.
    fn in_use(&self, addr: Address) -> bool {
        self.0.get(&addr).is_some_and(|n| *n > 0)
    }

    /// Whether one of the driver's connects or links is on any of them.
    fn any_in_use(&self) -> bool {
        self.0.values().any(|n| *n > 0)
    }

    /// `addr` was removed from BlueZ, or BlueZ didn't have it: no stranger now, unless a connect to it started since.
    fn removed(&mut self, addr: Address) {
        if self.0.get(&addr) == Some(&0) {
            self.0.remove(&addr);
        }
    }

    /// Those to remove: none of the driver's connects or links is on them.
    fn idle(&self) -> Vec<Address> {
        self.0.iter().filter(|(_, n)| **n == 0).map(|(a, _)| *a).collect()
    }
}

/// A use of a stranger ([`Strangers`]): one of the driver's connects to it, from its start to its link's end, or its
/// task's abort. `remove` runs once the last one on the device ends.
struct Touch {
    addr: Address,
    strangers: Arc<Mutex<Strangers>>,
    remove: Option<Box<dyn FnOnce() + Send>>,
}

impl Touch {
    /// A connect to `addr` starts, `known` or not ([`Strangers::touch`]): `None` if it isn't a stranger.
    fn new(
        strangers: &Arc<Mutex<Strangers>>,
        addr: Address,
        known: bool,
        remove: impl FnOnce() + Send + 'static,
    ) -> Option<Touch> {
        let counts = strangers.lock().unwrap().touch(addr, known);
        counts.then(|| Touch { addr, strangers: strangers.clone(), remove: Some(Box::new(remove)) })
    }
}

impl Drop for Touch {
    fn drop(&mut self) {
        let last = self.strangers.lock().unwrap().release(self.addr);
        if let Some(remove) = self.remove.take().filter(|_| last) {
            remove();
        }
    }
}

/// What the driver asks BlueZ of a device, by its address: the adapter's, or a test's fake.
trait Devices: Clone + Send + Sync + 'static {
    /// `Device1.Disconnect`.
    fn disconnect(&self, addr: Address) -> BoxFuture<'static, bluer::Result<()>>;
    /// `Adapter1.RemoveDevice`.
    fn remove(&self, addr: Address) -> BoxFuture<'static, bluer::Result<()>>;
}

impl Devices for Adapter {
    fn disconnect(&self, addr: Address) -> BoxFuture<'static, bluer::Result<()>> {
        let dev = self.device(addr);
        async move { dev?.disconnect().await }.boxed()
    }

    fn remove(&self, addr: Address) -> BoxFuture<'static, bluer::Result<()>> {
        let adapter = self.clone();
        async move { adapter.remove_device(addr).await }.boxed()
    }
}

/// Removes `addr`, a stranger none of the driver's connects or links is on, from BlueZ ([`Strangers`]), counted in
/// [`Ctx::removals`] until it ends (`_removal`). Its link is closed first, which also stops at once a connect BlueZ
/// has under way to it, as `RemoveDevice` may not; then the device goes, with its bond and what BlueZ stored of it.
/// Not if a connect to it started meanwhile, whose end removes it.
async fn remove_stranger(bluez: impl Devices, strangers: Arc<Mutex<Strangers>>, addr: Address, _removal: Counted) {
    let _ = bluez.disconnect(addr).await;
    if strangers.lock().unwrap().in_use(addr) {
        return;
    }
    let device = tag(device_id(addr));
    match bluez.remove(addr).await {
        Err(e) if !matches!(e.kind, bluer::ErrorKind::DoesNotExist) => {
            let why = masked(&e.to_string());
            return log::info!("Bluetooth: {device} wasn't removed from BlueZ, {why}; it is once an adapter is set up");
        }
        _ => log::debug!("Bluetooth: {device} removed from BlueZ"),
    }
    strangers.lock().unwrap().removed(addr);
}

/// What runs once the last use of stranger `addr` ends ([`Touch`]): [`remove_stranger`], in a task of its own,
/// counted from now.
fn removes(bluez: &impl Devices, ctx: &Ctx, addr: Address) -> impl FnOnce() + Send + 'static {
    let (bluez, strangers, removals) = (bluez.clone(), ctx.strangers.clone(), ctx.removals.clone());
    move || {
        // None as the runtime shuts down, its tasks dropped.
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(remove_stranger(bluez, strangers, addr, Counted::new(&removals)));
        }
    }
}

/// The one central this radio serves, if any.
#[derive(Default)]
struct Peripheral {
    conn: Option<u32>,
    /// Its address, from its subscription to `tx`.
    from: Option<Address>,
}

/// What the driver's tasks share.
#[derive(Clone)]
struct Ctx {
    events: Events,
    shared: Arc<Shared>,
    peripheral: Arc<Mutex<Peripheral>>,
    centrals: Centrals,
    /// A connect's turn to make its link. The kernel makes one LE link at a time, from the devices asked for, whichever
    /// it hears first: so one it can't make, as the device went silent after an advert, would hold up the others, and
    /// one asked for after a member's could take its turn (NB116-2).
    turn: Arc<tokio::sync::Semaphore>,
    /// Whether a connect is making its link (one at a time): the scan is held off for it ([`Discovery`]).
    linking: watch::Sender<usize>,
    /// How many discovery sessions run ([`Discovery`]): a connect's link is given `LINK` once none does ([`linked`]).
    scans: watch::Sender<usize>,
    /// How many discovery sessions are held off ([`Discovery`]): the next connect's turn waits for none to be
    /// ([`Making::release`]).
    held: watch::Sender<usize>,
    strangers: Arc<Mutex<Strangers>>,
    /// How many strangers' removals are under way ([`remove_stranger`]): a driver that closes waits for them.
    removals: watch::Sender<usize>,
}

/// One part of the radio that should be up, and when to try it again: each failure is logged, and the first success
/// after them.
struct Retry {
    what: &'static str,
    failures: u32,
    at: Option<Instant>,
}

impl Retry {
    fn new(what: &'static str) -> Retry {
        Retry { what, failures: 0, at: None }
    }

    /// Whether to try it at `now`.
    fn due(&self, now: Instant) -> bool {
        self.at.is_none_or(|at| now >= at)
    }

    /// Tries it next time whatever the wait, as what it is for changed.
    fn now(&mut self) {
        self.at = None;
    }

    /// It failed at `now`, for `why`: tried again `RETRY_MIN` later, doubled at each failure in a row, up to
    /// `RETRY_MAX`.
    fn failed(&mut self, now: Instant, why: &str) {
        let wait = RETRY_MIN.saturating_mul(1 << self.failures.min(5)).min(RETRY_MAX);
        self.failures = self.failures.saturating_add(1);
        self.at = Some(now + wait);
        log::warn!("Bluetooth: {}: {why}; trying again in {} s", self.what, wait.as_secs());
    }

    /// It works.
    fn up(&mut self) {
        if self.failures > 0 {
            let s = if self.failures == 1 { "" } else { "s" };
            log::info!("Bluetooth: {} works again, after {} failure{s}", self.what, self.failures);
        }
        (self.failures, self.at) = (0, None);
    }
}

/// The adapter in use, and what the driver has set up on it.
struct On {
    adapter: Adapter,
    /// Its own events, which end when it goes.
    changes: LocalBoxStream<'static, AdapterEvent>,
    powered: bool,
    /// The GATT service, and the task serving the subscriptions to `tx` it hands over.
    app: Option<(ApplicationHandle, JoinHandle<()>)>,
    advert: Option<AdvertisementHandle>,
    scan: Option<JoinHandle<()>>,
}

impl Drop for On {
    fn drop(&mut self) {
        self.app.iter().for_each(|(_, t)| t.abort());
        self.scan.iter().for_each(JoinHandle::abort);
    }
}

/// What happened on the adapter in use.
enum News {
    /// One of its events; `None` once it is gone.
    Changed(Option<AdapterEvent>),
    /// Its scan ended.
    ScanEnded,
}

struct Driver {
    session: Session,
    ctx: Ctx,
    /// Connects under way, which fail if the adapter goes.
    connects: Vec<(DeviceId, JoinHandle<()>)>,
    /// What the node asked for: the advert's data, and whether to scan.
    advert: Option<Vec<u8>>,
    scanning: bool,
    /// The node was told the advert isn't on the air.
    off_air: bool,
    on: Option<On>,
    retry_adapter: Retry,
    retry_app: Retry,
    retry_advert: Retry,
    retry_scan: Retry,
}

impl Driver {
    async fn run(
        mut cmds: mpsc::UnboundedReceiver<Cmd>,
        events: Events,
        shared: Arc<Shared>,
        ready: std_mpsc::Sender<io::Result<String>>,
    ) {
        let session = async {
            let session = Session::new().await?;
            let adapters = session.events().await?;
            Ok::<_, bluer::Error>((session, adapters))
        };
        let (session, adapters) = match session.await {
            Ok(s) => s,
            Err(e) => return drop(ready.send(Err(io::Error::other(e)))),
        };
        let peripheral = Arc::new(Mutex::new(Peripheral::default()));
        let centrals = Arc::new(Mutex::new(HashMap::new()));
        let strangers = Arc::new(Mutex::new(Strangers::default()));
        let turn = Arc::new(tokio::sync::Semaphore::new(1));
        let (linking, scans, held) = (watch::Sender::new(0), watch::Sender::new(0), watch::Sender::new(0));
        let removals = watch::Sender::new(0);
        let ctx = Ctx { events, shared, peripheral, centrals, turn, linking, scans, held, strangers, removals };
        let mut d = Driver {
            session,
            ctx,
            connects: Vec::new(),
            advert: None,
            scanning: false,
            off_air: false,
            on: None,
            retry_adapter: Retry::new("finding the adapter"),
            retry_app: Retry::new("serving the GATT service"),
            retry_advert: Retry::new("advertising"),
            retry_scan: Retry::new("scanning"),
        };
        match d.start().await {
            Ok(alias) => {
                let _ = ready.send(Ok(alias));
            }
            Err(e) => return drop(ready.send(Err(io::Error::other(e)))),
        }
        let mut adapters = adapters.boxed_local();
        let mut check = tokio::time::interval_at(Instant::now() + CHECK, CHECK);
        check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            d.settle().await;
            let wake = d.next_try();
            tokio::select! {
                cmd = cmds.recv() => match cmd {
                    Some(cmd) => d.command(cmd).await,
                    None => {
                        // The node closed it: nothing is advertised, served or scanned for meanwhile.
                        d.on = None;
                        return close(&d.ctx, &mut d.connects, CLOSE).await;
                    }
                },
                e = adapters.next() => match e {
                    Some(e) => d.adapters_changed(e).await,
                    None => {
                        log::warn!("Bluetooth: BlueZ's adapter events ended");
                        adapters = futures::stream::pending().boxed_local();
                    }
                },
                news = news(&mut d.on) => match news {
                    News::Changed(e) => d.changed(e),
                    News::ScanEnded => d.scan_ended(),
                },
                () = until(wake) => {}
                _ = check.tick() => d.check().await,
            }
        }
    }

    /// Sets up on the default adapter at start: its alias, or why not. A missing adapter or GATT service fails
    /// [`BlueZ::new`]; later, a part that fails is tried again.
    async fn start(&mut self) -> Result<String, String> {
        let mut on = self.find(true).await?;
        let alias = on.adapter.alias().await.map_err(|e| e.to_string())?;
        on.app = Some(serve(&on.adapter, &self.ctx).await.map_err(|e| e.to_string())?);
        self.on = Some(on);
        Ok(alias)
    }

    /// The default adapter: powered on if `power` (at start), else as it is, as its user may have turned it off.
    async fn find(&self, power: bool) -> Result<On, String> {
        let adapter = self.session.default_adapter().await.map_err(|e| e.to_string())?;
        if power {
            adapter.set_powered(true).await.map_err(|e| format!("powering {} on: {e}", adapter.name()))?;
        }
        // Its events before its state, so that it being powered on after it answers is heard.
        let changes = adapter.events().await.map_err(|e| e.to_string())?.boxed_local();
        let powered = adapter.is_powered().await.map_err(|e| e.to_string())?;
        log::info!("Bluetooth: on adapter {}{}", adapter.name(), if powered { "" } else { ", powered off" });
        Ok(On { adapter, changes, powered, app: None, advert: None, scan: None })
    }

    /// Brings up what is down and due: the adapter, then on it the GATT service, the advert and the scan.
    async fn settle(&mut self) {
        let now = Instant::now();
        if self.on.is_none() {
            if !self.retry_adapter.due(now) {
                return;
            }
            match self.find(false).await {
                Ok(on) => {
                    self.retry_adapter.up();
                    self.on = Some(on);
                    self.retry_app.now();
                    self.retry_advert.now();
                    self.retry_scan.now();
                    self.sweep();
                }
                Err(e) => return self.retry_adapter.failed(now, &e),
            }
        }
        let Some(on) = self.on.as_mut().filter(|on| on.powered) else { return };
        if on.app.is_none() && self.retry_app.due(now) {
            match serve(&on.adapter, &self.ctx).await {
                Ok(app) => {
                    on.app = Some(app);
                    self.retry_app.up();
                }
                Err(e) => self.retry_app.failed(now, &e.to_string()),
            }
        }
        if let Some(data) = self.advert.as_deref().filter(|_| on.advert.is_none() && self.retry_advert.due(now)) {
            let adv = Advertisement {
                advertisement_type: Type::Peripheral,
                service_data: [(uuid(SERVICE_UUID), data.to_vec())].into_iter().collect(),
                discoverable: Some(true),
                ..Default::default()
            };
            match on.adapter.advertise(adv).await {
                Ok(handle) => {
                    on.advert = Some(handle);
                    self.retry_advert.up();
                    if std::mem::take(&mut self.off_air) {
                        self.ctx.events.send(RadioEvent::Advertising(true));
                    }
                }
                Err(e) => {
                    self.retry_advert.failed(now, &e.to_string());
                    if !std::mem::replace(&mut self.off_air, true) {
                        self.ctx.events.send(RadioEvent::Advertising(false));
                    }
                }
            }
        }
        if self.scanning && on.scan.is_none() && self.retry_scan.due(now) {
            match scan(&on.adapter, &self.ctx).await {
                Ok(task) => {
                    on.scan = Some(task);
                    self.retry_scan.up();
                }
                Err(e) => self.retry_scan.failed(now, &e),
            }
        }
    }

    /// When the next part that is down is due to be tried again, if any is.
    fn next_try(&self) -> Option<Instant> {
        let Some(on) = &self.on else { return self.retry_adapter.at };
        let parts = [
            (on.app.is_none(), &self.retry_app),
            (self.advert.is_some() && on.advert.is_none(), &self.retry_advert),
            (self.scanning && on.scan.is_none(), &self.retry_scan),
        ];
        parts.into_iter().filter(|(down, _)| *down && on.powered).filter_map(|(_, r)| r.at).min()
    }

    async fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Advertise(data) => {
                self.advert = data;
                if let Some(on) = &mut self.on {
                    on.advert = None; // dropping the handle stops the old advert
                }
                self.retry_advert.now();
            }
            Cmd::Scan(scanning) => {
                self.scanning = scanning;
                if let Some(t) = self.on.as_mut().and_then(|on| on.scan.take()) {
                    t.abort();
                }
                self.retry_scan.now();
            }
            Cmd::Connect(device) => {
                self.connects.retain(|(_, t)| !t.is_finished());
                match self.on.as_ref().filter(|on| on.powered) {
                    Some(on) => self
                        .connects
                        .push((device, tokio::spawn(connect(on.adapter.clone(), self.ctx.clone(), device)))),
                    None => self.ctx.events.send(RadioEvent::ConnectFailed { device }),
                }
            }
            Cmd::Disconnect(conn) => self.disconnect(conn),
        }
    }

    /// Closes `conn`, as the node did: in the books at once, and at BlueZ in a task of its own (`drop_later`). Not
    /// `async`, so the loop never waits on a remote link (NB110-12).
    fn disconnect(&mut self, conn: ConnId) {
        if let (Some(addr), Some(on)) = (forget(&self.ctx, conn.0), &self.on) {
            drop_later(&on.adapter, &self.ctx.centrals, [addr]);
        }
    }

    /// An adapter came or went. The one in use going is left; one coming is set up on if it is the one in use again
    /// (bluetoothd restarted, and forgot what was set up on it) or the default now (a USB reset brought the adapter
    /// back as another, which is the only one, say), or if there is none in use.
    async fn adapters_changed(&mut self, e: SessionEvent) {
        let ours = self.on.as_ref().map(|on| on.adapter.name().to_owned());
        match e {
            SessionEvent::AdapterRemoved(name) if ours.as_ref() == Some(&name) => self.leave(&format!("{name} went")),
            SessionEvent::AdapterAdded(name) => {
                let default = self.session.default_adapter().await.ok().map(|a| a.name().to_owned());
                match ours {
                    None => self.retry_adapter.now(),
                    Some(o) if o == name => self.leave(&format!("{name} came again")),
                    Some(o) if default.as_deref().is_some_and(|d| d != o) => {
                        self.leave(&format!("{name} came, the default now"))
                    }
                    Some(_) => {}
                }
            }
            _ => {}
        }
    }

    /// The adapter in use changed: powered off or on, or gone (`None`).
    fn changed(&mut self, e: Option<AdapterEvent>) {
        let Some(name) = self.on.as_ref().map(|on| on.adapter.name().to_owned()) else { return };
        match e {
            None => self.leave(&format!("{name} went")),
            Some(AdapterEvent::PropertyChanged(AdapterProperty::Powered(powered))) => self.powered(powered),
            Some(_) => {}
        }
    }

    /// The adapter in use is powered on or off. Off, it holds no links, and nothing is set up on it until it is on
    /// again: the driver leaves that to its user, who may have turned it off.
    fn powered(&mut self, powered: bool) {
        let Some(on) = &mut self.on else { return };
        if on.powered == powered {
            return;
        }
        on.powered = powered;
        let name = on.adapter.name().to_owned();
        if powered {
            log::info!("Bluetooth: {name} powered on");
            self.retry_app.now();
            self.retry_advert.now();
            self.retry_scan.now();
            self.sweep();
        } else {
            self.lose(&format!("{name} powered off"));
        }
    }

    /// Asks the adapter in use whether it is powered: a bluetoothd that crashed says nothing.
    async fn check(&mut self) {
        let Some(on) = &self.on else { return };
        let name = on.adapter.name().to_owned();
        let powered = on.adapter.is_powered().await;
        match powered {
            Ok(powered) => self.powered(powered),
            Err(e) => self.leave(&format!("{name} doesn't answer ({e})")),
        }
    }

    /// Removes from BlueZ again, once the adapter in use is powered, each stranger whose removal failed, as the adapter
    /// was off or gone ([`Strangers`]).
    fn sweep(&self) {
        let Some(on) = self.on.as_ref().filter(|on| on.powered) else { return };
        for addr in self.ctx.strangers.lock().unwrap().idle() {
            removes(&on.adapter, &self.ctx, addr)();
        }
    }

    fn scan_ended(&mut self) {
        if let Some(on) = &mut self.on {
            on.scan = None;
        }
        self.retry_scan.failed(Instant::now(), "it ended");
    }

    /// Leaves the adapter in use, for `why`, and finds the default one at once.
    fn leave(&mut self, why: &str) {
        self.lose(why);
        self.on = None;
        self.retry_adapter.now();
    }

    /// The adapter in use holds no links now, for `why`: each is reported closed, and connects under way failed, and
    /// on an adapter still there they are closed and stopped at BlueZ; the GATT service, the advert and the scan are
    /// down.
    fn lose(&mut self, why: &str) {
        let (closed, addrs) = drop_links(&self.ctx, &mut self.connects);
        log::warn!("Bluetooth: {why}; {closed} links closed");
        if let Some(on) = &mut self.on {
            on.advert = None;
            for t in on.app.take().map(|(_, t)| t).into_iter().chain(on.scan.take()) {
                t.abort();
            }
            drop_later(&on.adapter, &self.ctx.centrals, addrs);
        }
        if self.advert.is_some() && !std::mem::replace(&mut self.off_air, true) {
            self.ctx.events.send(RadioEvent::Advertising(false));
        }
    }
}

/// The next thing to happen on the adapter in use, if any.
async fn news(on: &mut Option<On>) -> News {
    let Some(on) = on else { return std::future::pending().await };
    let (changes, scan) = (&mut on.changes, &mut on.scan);
    let ended = async {
        match scan {
            Some(t) => {
                let _ = t.await;
            }
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        e = changes.next() => News::Changed(e),
        () = ended => News::ScanEnded,
    }
}

/// Waits until `at`, or for ever.
async fn until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Reports every link closed and every connect under way failed: each connect still under way stops, each connection
/// closes. How many links there were, and the devices to close at BlueZ: theirs, and those of the connects, which
/// BlueZ may have under way still.
fn drop_links(ctx: &Ctx, connects: &mut Vec<(DeviceId, JoinHandle<()>)>) -> (usize, Vec<Address>) {
    let mut addrs = Vec::new();
    // One that hasn't finished hasn't said `Connected` (nothing between that and its end waits), nor `ConnectFailed`.
    for (device, t) in connects.drain(..) {
        if !t.is_finished() {
            t.abort();
            ctx.events.send(RadioEvent::ConnectFailed { device });
            addrs.push(address(device));
        }
    }
    let links: Vec<(u32, Link)> = ctx.centrals.lock().unwrap().drain().collect();
    let mut closed = links.len();
    for (conn, link) in links {
        link.write.abort();
        link.read.abort();
        ctx.shared.senders.lock().unwrap().remove(&conn);
        ctx.events.send(RadioEvent::Disconnected { conn: ConnId(conn) });
        addrs.push(link.addr);
    }
    // Its task ends as its queue closes.
    let p = std::mem::take(&mut *ctx.peripheral.lock().unwrap());
    if let Some(conn) = p.conn {
        ctx.shared.senders.lock().unwrap().remove(&conn);
        ctx.events.send(RadioEvent::Disconnected { conn: ConnId(conn) });
        closed += 1;
    }
    addrs.extend(p.from);
    (closed, addrs)
}

/// The node closed the driver: each link closes and each connect under way stops, as when the adapter goes
/// ([`drop_links`]), and it waits, `bound` at most, for BlueZ to remove the strangers among their devices
/// ([`Strangers`]), as the runtime drops its tasks once it ends: until no connect or link is on one, and no removal is
/// under way, those started earlier included. One whose removal failed earlier isn't tried again.
async fn close(ctx: &Ctx, connects: &mut Vec<(DeviceId, JoinHandle<()>)>, bound: Duration) {
    drop_links(ctx, connects);
    let mut removals = ctx.removals.subscribe();
    // An aborted connect's use ends as its task is dropped, and the last use on a device starts its removal.
    let removed = async {
        while ctx.strangers.lock().unwrap().any_in_use() || *removals.borrow_and_update() > 0 {
            if removals.changed().await.is_err() {
                return;
            }
        }
    };
    let _ = tokio::time::timeout(bound, removed).await;
}

/// Takes `conn` out of the driver's books, as the node closed it: a central connection's tasks stop, or the one
/// central's slot frees. The device, to close the link to.
fn forget(ctx: &Ctx, conn: u32) -> Option<Address> {
    let central = ctx.centrals.lock().unwrap().remove(&conn);
    if let Some(link) = central {
        link.write.abort();
        link.read.abort();
        return Some(link.addr);
    }
    // The central of ours: its notify session ends with its link, which BlueZ can close given its address.
    let mut p = ctx.peripheral.lock().unwrap();
    (p.conn == Some(conn)).then(|| std::mem::take(&mut *p).from).flatten()
}

/// Closes the links to `addrs` ([`drop_link`]), each in a task of its own: BlueZ answers a disconnect only once the
/// link is down, 2 s or more later, so nothing waits behind it (NB110-12).
fn drop_later(bluez: &impl Devices, centrals: &Centrals, addrs: impl IntoIterator<Item = Address>) {
    for a in addrs {
        let (bluez, centrals) = (bluez.clone(), centrals.clone());
        tokio::spawn(async move { drop_link(&bluez, &centrals, a).await });
    }
}

/// Serves the GATT service on `adapter`: the app, and the task that serves its subscriptions to `tx`.
async fn serve(adapter: &Adapter, ctx: &Ctx) -> bluer::Result<(ApplicationHandle, JoinHandle<()>)> {
    let (control, handle) = characteristic_control();
    let app = adapter.serve_gatt_application(application(adapter, ctx, handle)).await?;
    Ok((app, tokio::spawn(subscriptions(adapter.clone(), ctx.clone(), control))))
}

/// The service: `rx` takes plain writes without response at offset 0 from the one central, and `tx` notifies it over
/// the socket its subscription hands over (`control`'s, see [`subscriptions`]). Another device that writes while one
/// is served is refused, as the node refuses a central over its limit: its link closes.
fn application(adapter: &Adapter, ctx: &Ctx, control: CharacteristicControlHandle) -> Application {
    let (events, peripheral) = (ctx.events.clone(), ctx.peripheral.clone());
    let (adapter, centrals) = (adapter.clone(), ctx.centrals.clone());
    let write: Box<dyn Fn(Vec<u8>, CharacteristicWriteRequest) -> _ + Send + Sync> =
        Box::new(move |value: Vec<u8>, req: CharacteristicWriteRequest| {
            let ok = req.op_type == WriteOp::Command && req.offset == 0;
            let p = peripheral.lock().unwrap();
            let (taken, stranger) = match (p.conn, p.from) {
                (Some(conn), Some(from)) if from == req.device_address => {
                    if ok {
                        events.send(RadioEvent::Value { conn: ConnId(conn), value });
                    }
                    (ok, false)
                }
                (Some(_), _) => (false, true),
                _ => (false, false),
            };
            drop(p);
            let (adapter, centrals, addr) = (adapter.clone(), centrals.clone(), req.device_address);
            async move {
                if stranger {
                    log::debug!("Bluetooth: a write from {} refused: another central is served", tag(device_id(addr)));
                    tokio::spawn(async move { drop_link(&adapter, &centrals, addr).await });
                }
                if taken {
                    Ok(())
                } else {
                    Err(ReqError::NotPermitted)
                }
            }
            .boxed()
        });
    let rx = Characteristic {
        uuid: uuid(RX_UUID),
        write: Some(CharacteristicWrite {
            write_without_response: true,
            method: CharacteristicWriteMethod::Fun(write),
            ..Default::default()
        }),
        ..Default::default()
    };
    let tx = Characteristic {
        uuid: uuid(TX_UUID),
        notify: Some(CharacteristicNotify {
            notify: true,
            method: CharacteristicNotifyMethod::Io,
            ..Default::default()
        }),
        control_handle: control,
        ..Default::default()
    };
    let service =
        Service { uuid: uuid(SERVICE_UUID), primary: true, characteristics: vec![rx, tx], ..Default::default() };
    Application { services: vec![service], ..Default::default() }
}

/// Serves each subscription to `tx`, as BlueZ hands it over with its socket and the subscriber's address.
async fn subscriptions(adapter: Adapter, ctx: Ctx, mut control: CharacteristicControl) {
    while let Some(e) = control.next().await {
        if let CharacteristicControlEvent::Notify(writer) = e {
            let central = Central { adapter: adapter.clone(), ctx: ctx.clone() };
            tokio::spawn(central.serve(writer));
        }
    }
}

/// Starts the scan on `adapter`: every LE advert, each heard again as it comes, and the service's reported
/// ([`follow`]). A filter on the service UUID would have BlueZ drop the rest, but crashes bluetoothd 5.87 (NB-112).
async fn scan(adapter: &Adapter, ctx: &Ctx) -> Result<JoinHandle<()>, String> {
    let filter = DiscoveryFilter { transport: DiscoveryTransport::Le, duplicate_data: true, ..Default::default() };
    adapter.set_discovery_filter(filter).await.map_err(|e| format!("the filter: {e}"))?;
    let discover = |adapter: Adapter| async move {
        let found = adapter.discover_devices().await.map_err(|e| format!("discovery: {e}"))?;
        Ok::<Reports, String>(found.boxed())
    };
    let found = discover(adapter.clone()).await?;
    let a = adapter.clone();
    // In a task of its own, so that a scan stopped as it starts again can't stop it half way, with BlueZ discovering
    // for a session no one holds.
    let start: Start = Box::new(move || {
        let start = tokio::spawn(discover(a.clone()));
        async move { start.await.map_err(|e| e.to_string())? }.boxed()
    });
    let discovery = Discovery::new(found, start, ctx, RUN, PAUSE);
    Ok(tokio::spawn(follow(adapter.clone(), ctx.events.clone(), discovery)))
}

/// Reports the service's adverts among what the scan finds, until it ends.
async fn follow(adapter: Adapter, events: Events, mut discovery: Discovery) {
    let mut watching = Watching::default();
    loop {
        if !discovery.settle().await {
            return;
        }
        let again = watching.relist_at(Instant::now());
        tokio::select! {
            next = discovery.next() => match next {
                // A session starting again lists BlueZ's devices again: one followed already is left as it is.
                Next::Found(AdapterEvent::DeviceAdded(addr)) if !watching.following(addr) => {
                    let Ok(dev) = adapter.device(addr) else { continue };
                    let ours = matches!(dev.service_data().await, Ok(Some(data)) if seen(&events, addr, &data));
                    watch(&mut watching, &events, dev, ours).await;
                }
                Next::Found(AdapterEvent::DeviceRemoved(addr)) => watching.remove(addr),
                Next::Found(_) | Next::Settle => {}
                Next::Ended => return,
            },
            () = until(again) => list_again(&adapter, &events, &mut watching).await,
        }
    }
}

/// What a discovery session reports: BlueZ's devices as it starts, then each one BlueZ adds or removes.
type Reports = BoxStream<'static, AdapterEvent>;

/// Starts a discovery session.
type Start = Box<dyn FnMut() -> BoxFuture<'static, Result<Reports, String>> + Send>;

/// The scan's discovery session, held off while one of the driver's connects makes its link (NB-116, NB116-2). Linux
/// starts a connect to an LE device once it hears the device advertise: while a discovery runs, from its reports alone,
/// which come once a device each time the discovery starts (every 10.24 s, as the kernel ends it and bluetoothd starts
/// it again), and the report that drew the connect was that one. With no discovery, the kernel scans for the connect
/// itself, and it starts at the device's next advert. The session is held off only once it has run `run` since it
/// started, so that it hears each device in range each time it starts, however many connects there are (NB116-1); and
/// until the link is up, or the connect has failed and its device been disconnected ([`linked`]), which stops the
/// kernel's attempt: a session started meanwhile would wait behind it, or fail. The next connect's turn comes once it
/// has started again ([`Making::release`]). It starts again after `pause` at most, should BlueZ not answer. With a link
/// being made still as it starts again, it reports that device again, which starts it, and is held off again once it
/// has run `run`.
struct Discovery {
    /// The session, while it runs, counted in [`Ctx::scans`].
    found: Option<(Reports, Counted)>,
    /// While the session is held off, counted in [`Ctx::held`].
    off: Option<Counted>,
    start: Start,
    /// Whether a connect is making its link ([`Ctx::linking`]).
    linking: watch::Receiver<usize>,
    scans: watch::Sender<usize>,
    held: watch::Sender<usize>,
    /// When the session started, or was held off.
    since: Instant,
    /// How long it runs at least before a connect holds it off, and how long it is held off at most.
    run: Duration,
    pause: Duration,
}

/// What came next in the scan.
enum Next {
    Found(AdapterEvent),
    /// A connect started or ended making its link, or the session ran `run` with one making it, or was held off for
    /// `pause`: [`Discovery::settle`] is due.
    Settle,
    /// The session ended, or the driver is gone.
    Ended,
}

impl Discovery {
    /// Follows `found`, a session that has just started, counted in `scans`.
    fn new(found: Reports, start: Start, ctx: &Ctx, run: Duration, pause: Duration) -> Self {
        let (linking, scans, held) = (ctx.linking.subscribe(), ctx.scans.clone(), ctx.held.clone());
        let found = Some((found, Counted::new(&scans)));
        Discovery { found, off: None, start, linking, scans, held, since: Instant::now(), run, pause }
    }

    /// Stops the session for a connect making its link once it has run `run`, and starts it again once none is, or
    /// it was held off for `pause`: whether it runs, or is held off, as it should be; not once it can't start again.
    async fn settle(&mut self) -> bool {
        let linking = *self.linking.borrow_and_update() > 0;
        let now = Instant::now();
        if self.found.is_some() {
            if linking && now >= self.since + self.run {
                (self.found, self.off, self.since) = (None, Some(Counted::new(&self.held)), now);
                log::debug!("Bluetooth: the scan paused for a connect");
            }
        } else if !linking || now >= self.since + self.pause {
            match (self.start)().await {
                Ok(found) => {
                    let found = Some((found, Counted::new(&self.scans)));
                    (self.found, self.off, self.since) = (found, None, Instant::now());
                    let still = if linking { ", with a link being made still" } else { "" };
                    log::debug!("Bluetooth: the scan resumed{still}");
                }
                Err(e) => {
                    self.off = None;
                    log::info!("Bluetooth: the scan didn't resume: {}", masked(&e));
                    return false;
                }
            }
        }
        true
    }

    /// What the session reports next, or that [`settle`](Discovery::settle) is due. It can be dropped while it
    /// waits.
    async fn next(&mut self) -> Next {
        // Held off once it has run `run` with a link being made, and started again once held off for `pause`.
        let due = match self.found {
            Some(_) => (*self.linking.borrow() > 0).then_some(self.since + self.run),
            None => Some(self.since + self.pause),
        };
        let due = until(due);
        let (found, linking) = (&mut self.found, &mut self.linking);
        let found = async {
            match found {
                Some((found, _)) => found.next().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            e = found => e.map_or(Next::Ended, Next::Found),
            changed = linking.changed() => if changed.is_ok() { Next::Settle } else { Next::Ended },
            () = due => Next::Settle,
        }
    }
}

/// One of what a count in [`Ctx`] counts (a connect making its link, a discovery session running or held off, a
/// stranger's removal), from its start to its end, or its task's abort.
struct Counted(watch::Sender<usize>);

impl Counted {
    fn new(count: &watch::Sender<usize>) -> Counted {
        count.send_modify(|n| *n += 1);
        Counted(count.clone())
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.send_modify(|n| *n -= 1);
    }
}

/// A connect's turn ([`Ctx::turn`]), and the scan held off for it ([`Ctx::linking`]), while it makes its link.
struct Making {
    turn: tokio::sync::OwnedSemaphorePermit,
    linking: Counted,
}

impl Making {
    fn new(turn: tokio::sync::OwnedSemaphorePermit, ctx: &Ctx) -> Making {
        Making { turn, linking: Counted::new(&ctx.linking) }
    }

    /// Ends it, its link up, or given up and its device disconnected: the scan may start again, and the next connect's
    /// turn comes once no session is held off (`held`, [`Ctx::held`]), at once with none, `ABORT` after at most should
    /// BlueZ not start it (NB116-3). So the next link is asked for with the scan running, not as it starts, which would
    /// wait behind it or fail (NB116-2), and the scan runs `RUN` between two connects however many there are.
    async fn release(self, held: watch::Sender<usize>) {
        let Making { turn, linking } = self;
        drop(linking);
        let mut held = held.subscribe();
        // Not held as it waits: the session writes it.
        let _ = tokio::time::timeout(ABORT, async { drop(held.wait_for(|n| *n == 0).await) }).await;
        drop(turn);
    }
}

/// [`Making::release`]s `making`, if any, in a task of its own: a connect's task goes on with its set-up, or ends with
/// its report (`drop_links`), meanwhile.
fn release_later(making: Option<Making>, ctx: &Ctx) {
    if let Some(making) = making {
        tokio::spawn(making.release(ctx.held.clone()));
    }
}

/// The turn of a connect to `device`, if it comes `within`: else the connect is reported failed, with nothing to
/// disconnect, as BlueZ wasn't asked to connect, so that a connect queued behind others fails by the time the driver
/// says, and doesn't make its link after the node has given it up (NB116-4).
async fn take_turn(ctx: &Ctx, device: DeviceId, within: Duration) -> Option<Making> {
    match tokio::time::timeout(within, ctx.turn.clone().acquire_owned()).await {
        Ok(Ok(turn)) => Some(Making::new(turn, ctx)),
        // Closed: it never is.
        Ok(Err(_)) => None,
        Err(_) => {
            let s = within.as_secs();
            log::info!("Bluetooth: the connect to {} failed, waiting for its turn: not done after {s} s", tag(device));
            ctx.events.send(RadioEvent::ConnectFailed { device });
            None
        }
    }
}

/// Follows `dev`, which has advertised the service if `ours`, if `watching` has room for it: each advert of its with
/// the service's data is reported.
async fn watch(watching: &mut Watching, events: &Events, dev: bluer::Device, ours: bool) {
    let addr = dev.address();
    if !watching.room(addr, ours) {
        return;
    }
    let Ok(mut changes) = dev.events().await else { return };
    let (events, ours) = (events.clone(), Arc::new(AtomicBool::new(ours)));
    let flag = ours.clone();
    let task = tokio::spawn(async move {
        while let Some(DeviceEvent::PropertyChanged(p)) = changes.next().await {
            if let DeviceProperty::ServiceData(data) = p {
                if seen(&events, addr, &data) {
                    flag.store(true, Ordering::Relaxed);
                }
            }
        }
    });
    watching.add(addr, task, ours);
}

/// Lists BlueZ's devices again, for those the scan left out: each one it doesn't follow is reported if it advertises
/// the service, and followed as there is room, the service's first (NB112-1).
async fn list_again(adapter: &Adapter, events: &Events, watching: &mut Watching) {
    let Ok(addrs) = adapter.device_addresses().await else {
        watching.listed = Some(Instant::now());
        return;
    };
    let mut found = Vec::new();
    for addr in addrs.into_iter().filter(|a| !watching.follows(*a)) {
        let Ok(dev) = adapter.device(addr) else { continue };
        let ours = matches!(dev.service_data().await, Ok(Some(data)) if seen(events, addr, &data));
        found.push((addr, ours));
    }
    for (addr, ours) in watching.listed(Instant::now(), found) {
        if let Ok(dev) = adapter.device(addr) {
            watch(watching, events, dev, ours).await;
        }
    }
}

/// Reports an advert's service data, if it has the service's: whether it had.
fn seen(events: &Events, addr: Address, data: &HashMap<Uuid, Vec<u8>>) -> bool {
    let Some(d) = data.get(&uuid(SERVICE_UUID)) else { return false };
    events.send(RadioEvent::Seen { device: device_id(addr), data: d.clone() });
    true
}

/// The devices the scan follows for changes to their adverts, at most `WATCH`: under that, each one BlueZ adds; at it,
/// only one that advertises the service already, in place of the device followed longest that never has. One that has
/// is never dropped for another, as any device can put the service in its adverts: a burst of them would push the
/// members out, unfollowed once it stops (NB112-1). One left out is missed: once a device of the service could be
/// followed, BlueZ's devices are listed again, at most every `RELIST` ([`list_again`]). One BlueZ removes (unheard
/// for its `TemporaryTimeout`, 30 s by default) is dropped, and all of them with the scan.
#[derive(Default)]
struct Watching {
    devices: HashMap<Address, Watched>,
    added: u64,
    /// A device BlueZ has was left out, for want of room.
    missed: bool,
    /// When BlueZ's devices were last listed again.
    listed: Option<Instant>,
}

struct Watched {
    task: JoinHandle<()>,
    added: u64,
    /// It has advertised the service.
    ours: Arc<AtomicBool>,
}

impl Watching {
    /// Whether to follow `addr`, which advertises the service if `ours`, making room for it if need be: else it is
    /// missed.
    fn room(&mut self, addr: Address, ours: bool) -> bool {
        self.remove(addr);
        self.devices.retain(|_, w| !w.task.is_finished());
        if self.devices.len() < WATCH {
            return true;
        }
        let Some(other) = self.oldest_other().filter(|_| ours) else {
            self.missed = true;
            return false;
        };
        self.remove(other);
        true
    }

    /// The device followed longest that never advertised the service, if any.
    fn oldest_other(&self) -> Option<Address> {
        let others = self.live().filter(|(_, w)| !w.ours.load(Ordering::Relaxed));
        others.min_by_key(|(_, w)| w.added).map(|(a, _)| *a)
    }

    fn live(&self) -> impl Iterator<Item = (&Address, &Watched)> {
        self.devices.iter().filter(|(_, w)| !w.task.is_finished())
    }

    fn follows(&self, addr: Address) -> bool {
        self.devices.contains_key(&addr)
    }

    /// Whether `addr` is followed still: BlueZ hasn't removed it since.
    fn following(&self, addr: Address) -> bool {
        self.devices.get(&addr).is_some_and(|w| !w.task.is_finished())
    }

    /// When to list BlueZ's devices again, as of `now`: once one was missed and a device of the service could be
    /// followed (there is room, or one followed never advertised it), `RELIST` after the last listing.
    fn relist_at(&self, now: Instant) -> Option<Instant> {
        let room = self.live().count() < WATCH || self.oldest_other().is_some();
        (self.missed && room).then(|| self.listed.map_or(now, |t| t + RELIST))
    }

    /// BlueZ's devices were listed at `now`: of them (`found`, each with whether it advertises the service), those
    /// not followed, in the order to follow them, the service's first.
    fn listed(&mut self, now: Instant, mut found: Vec<(Address, bool)>) -> Vec<(Address, bool)> {
        (self.missed, self.listed) = (false, Some(now));
        found.retain(|(a, _)| !self.follows(*a));
        found.sort_by_key(|(_, ours)| !ours);
        found
    }

    fn add(&mut self, addr: Address, task: JoinHandle<()>, ours: Arc<AtomicBool>) {
        self.added += 1;
        if let Some(old) = self.devices.insert(addr, Watched { task, added: self.added, ours }) {
            old.task.abort();
        }
    }

    fn remove(&mut self, addr: Address) {
        if let Some(w) = self.devices.remove(&addr) {
            w.task.abort();
        }
    }
}

impl Drop for Watching {
    fn drop(&mut self) {
        self.devices.values().for_each(|w| w.task.abort());
    }
}

/// Connects to `device` as a central: the service's `tx` notifications as a socket to read, its `rx` as one to write
/// without response. One that isn't set up within `SETUP`, or fails, is reported failed ([`connect_failed`]), and
/// its link closed if BlueZ was asked to make it, and only then: a device whose link is up already, its own to this
/// one or another program's, isn't asked to connect, and its link is left (NB110-16). This task ends with the report,
/// so one still under way has said nothing (`drop_links`). It waits for its turn ([`Ctx::turn`]), and fails, with
/// nothing to disconnect, if that doesn't come within `SETUP` either ([`take_turn`], NB116-4); the scan is held off
/// while it makes its link ([`Discovery`]), and a link not made `LINK` after that fails ([`linked`]) (NB116-2). Its
/// link up or given up, its turn passes on once the scan has started again ([`Making::release`], NB116-3). A device
/// that lists HID among its services isn't connected to, or its connect fails or its link closes as soon as BlueZ
/// lists it, whenever that is (NB110-14); one BlueZ had neither paired nor trusted is removed from it once this
/// connect, or its link, ends, if the driver made its link ([`Strangers`], NB110-13).
async fn connect(adapter: Adapter, ctx: Ctx, device: DeviceId) {
    let addr = address(device);
    let Ok(dev) = adapter.device(addr) else {
        log::info!("Bluetooth: the connect to {} failed: BlueZ doesn't have it", tag(device));
        return ctx.events.send(RadioEvent::ConnectFailed { device });
    };
    // `SETUP` counts from here, its wait for its turn included, as the node's own wait does (NB116-4).
    let asked = Instant::now();
    let Some(making) = take_turn(&ctx, device, SETUP).await else { return };
    let mut making = Some(making);
    // Whether BlueZ was asked to make its link, which is then the driver's to close.
    let mut ours = false;
    let mut step = "reading it";
    let setup = async {
        // Read before the connect, which may pair it.
        let known = dev.is_paired().await? || dev.is_trusted().await?;
        no_hid(&dev).await?;
        // Its events before the connect, so that its link coming up, or going down, or it listing HID, is heard from
        // here on: the rest of the set-up is given up for the last two, and then its link closed (NB-114, NB110-14).
        step = "following it";
        let mut changes = dev.events().await?.boxed();
        // Read once its events are taken, so that a link up now that goes is heard of.
        step = "reading it";
        let up = dev.is_connected().await?;
        // One up already isn't the driver's: its device a stranger only if it is one already (NB110-16).
        let touch = Touch::new(&ctx.strangers, addr, known || up, removes(&adapter, &ctx, addr));
        if up {
            // Not asked to connect: BlueZ would answer at once, or connect over BR/EDR to a device that has it too.
            release_later(making.take(), &ctx);
        } else {
            step = "connecting";
            ours = true;
            let connecting = dev.connect();
            tokio::pin!(connecting);
            let answered = linked(connecting.as_mut(), &mut changes, ctx.scans.subscribe(), LINK).await?;
            // Its link is up: the scan may run again, and the next connect's turn comes once it does, as this set-up
            // goes on.
            release_later(making.take(), &ctx);
            // BlueZ answers once it has read the device's services, and its input plugin taken HID as an input
            // device, which a disconnect asked for detaches at once; or at once, if the link came up meanwhile (the
            // device connected to this one), and reads them after.
            if !answered {
                unless_gone(connecting, &mut changes).await?;
            }
        }
        step = "reading its services";
        no_hid(&dev).await?;
        let sockets = async {
            step = "finding the service";
            let mut rx = None;
            let mut tx = None;
            let services = dev.services().await?;
            // Resolved now, BlueZ lists the services it read: HID among them gets no signal if it had them stored, from
            // before bluetoothd restarted, as the list it holds doesn't change (NB110-I13).
            no_hid(&dev).await?;
            for s in services {
                if s.uuid().await? != uuid(SERVICE_UUID) {
                    continue;
                }
                for c in s.characteristics().await? {
                    match c.uuid().await? {
                        u if u == uuid(RX_UUID) => rx = Some(c),
                        u if u == uuid(TX_UUID) => tx = Some(c),
                        _ => {}
                    }
                }
            }
            let (Some(rx), Some(tx)) = (rx, tx) else {
                let message = "it has no Peerwire service".into();
                return Err(bluer::Error { kind: bluer::ErrorKind::ServicesUnresolved, message });
            };
            step = "subscribing to tx";
            let reader = tx.notify_io().await?;
            step = "taking rx";
            Ok((reader, rx.write_io().await?))
        };
        let (reader, writer) = unless_gone(sockets, &mut changes).await?;
        if !dev.is_connected().await? {
            return Err(bluer::Error { kind: bluer::ErrorKind::Failed, message: "its link went down".into() });
        }
        Ok::<_, bluer::Error>((reader, writer, changes, touch))
    };
    let set_up = match tokio::time::timeout_at(asked + SETUP, setup).await {
        Ok(Ok((r, w, changes, touch))) => {
            let len = w.mtu().min(r.mtu());
            att_mtu(len).map(|mtu| (r, w, changes, touch, mtu)).ok_or_else(|| format!("its sockets take {len} bytes"))
        }
        Ok(Err(e)) => Err(format!("{step}: {e}")),
        Err(_) => Err(format!("{step}: not done {} s after it was asked for", SETUP.as_secs())),
    };
    let (reader, writer, changes, touch, mtu) = match set_up {
        Ok(s) => s,
        Err(why) => {
            log::info!("Bluetooth: the connect to {} failed, {}", tag(device), masked(&why));
            return connect_failed(&adapter, &ctx, device, ours, making).await;
        }
    };
    // Nothing from here on waits, so a connect still under way hasn't said `Connected` (`drop_links`).
    log::debug!("Bluetooth: connected to {} in {} ms", tag(device), asked.elapsed().as_millis());
    let conn = ctx.shared.next.fetch_add(1, Ordering::Relaxed);
    let (out, mut queue) = mpsc::channel::<Vec<u8>>(QUEUE);
    ctx.shared.senders.lock().unwrap().insert(conn, out);
    ctx.events.send(RadioEvent::Connected { conn: ConnId(conn), device: Some(device), mtu });
    let write = tokio::spawn(async move {
        while let Some(v) = queue.recv().await {
            if writer.send(&v).await.is_err() {
                break;
            }
        }
    });
    let c = ctx.clone();
    let read = tokio::spawn(async move {
        let values = futures::stream::unfold(reader, |r| async move { Some((r.recv().await, r)) });
        if read_link(&c, conn, values, link_gone(changes)).await {
            let _ = dev.disconnect().await;
        }
    });
    ctx.centrals.lock().unwrap().insert(conn, Link { write, read, addr, _touch: touch });
}

/// A connect to `device` failed: it is reported, and its turn and its hold on the scan (`making`, while it made its
/// link) released, after the report and in a task of its own ([`release_later`]). Its link is closed only if BlueZ
/// was asked to make it (`ours`), which also stops a connect BlueZ still has under way: one up before, the device's
/// own or another program's, is left, and so is one never asked for, as nothing is under way to stop (NB110-16). Not
/// made, it holds its turn and the scan off until BlueZ has disconnected it, `ABORT` at most, which BlueZ answers at
/// once, as that stops the kernel's attempt, which the next link and the scan's start would wait behind (NB116-2);
/// made, it is reported first, as BlueZ answers a disconnect only once the link is down, 2 s or more later
/// (NB110-12).
async fn connect_failed(bluez: &impl Devices, ctx: &Ctx, device: DeviceId, ours: bool, making: Option<Making>) {
    let addr = address(device);
    let made = making.is_none();
    if ours && !made {
        let _ = tokio::time::timeout(ABORT, drop_link(bluez, &ctx.centrals, addr)).await;
    }
    ctx.events.send(RadioEvent::ConnectFailed { device });
    release_later(making, ctx);
    if ours && made {
        drop_later(bluez, &ctx.centrals, [addr]);
    }
}

/// Fails if BlueZ lists HID among `dev`'s services, from its adverts or, once connected, its GATT database: BlueZ's
/// input plugin takes a device that serves it as an input device, asking to pair with one that isn't paired
/// (`LEAutoSecurity`), and no Peerwire peer serves it (NB110-13).
async fn no_hid(dev: &bluer::Device) -> bluer::Result<()> {
    if dev.uuids().await?.is_some_and(|u| u.contains(&uuid(HID_UUID))) {
        let message = "it serves HID".into();
        return Err(bluer::Error { kind: bluer::ErrorKind::NotPermitted, message });
    }
    Ok(())
}

/// `step`, of a connect's set-up, given up if `changes` (its device's) say first that its link went down or it lists
/// HID ([`link_gone`]): a device can stall a step, up to `SETUP`, with HID listed meanwhile (NB110-14).
async fn unless_gone<T>(
    step: impl Future<Output = bluer::Result<T>>,
    changes: &mut BoxStream<'static, DeviceEvent>,
) -> bluer::Result<T> {
    tokio::select! {
        biased;
        why = link_gone(changes) => Err(bluer::Error { kind: bluer::ErrorKind::Failed, message: why.into() }),
        r = step => r,
    }
}

/// Waits for `connecting` (`Device1.Connect`) to make its link: until `changes` (the device's) say it is connected, or
/// BlueZ answers, as it does at once if the link is up already. Whether it has answered. Given up `within` the scan
/// being held off for it (none of `scans` running), as the kernel then makes the link at the device's next advert: a
/// device that went silent after one would hold the kernel's one LE connect, and the next and the scan's start behind
/// it, until `SETUP` (NB116-2).
async fn linked(
    mut connecting: Pin<&mut impl Future<Output = bluer::Result<()>>>,
    changes: &mut BoxStream<'static, DeviceEvent>,
    mut scans: watch::Receiver<usize>,
    within: Duration,
) -> bluer::Result<bool> {
    let up = async {
        while let Some(DeviceEvent::PropertyChanged(p)) = changes.next().await {
            if matches!(p, DeviceProperty::Connected(true)) {
                return true;
            }
        }
        false
    };
    let late = async {
        // Not held as it sleeps: the scan counts write it. An error once the driver is gone.
        drop(scans.wait_for(|n| *n == 0).await);
        tokio::time::sleep(within).await;
    };
    tokio::select! {
        biased;
        r = &mut connecting => r.map(|()| true),
        true = up => Ok(false),
        () = late => {
            let message = format!("not linked {} s after the scan was held off for it", within.as_secs());
            Err(bluer::Error { kind: bluer::ErrorKind::Failed, message })
        }
    }
}

/// Reports central connection `conn`'s values from `values` until its link goes down ([`until_down`]), and then the
/// connection closed ([`link_down`]): whether it was.
async fn read_link(
    ctx: &Ctx,
    conn: u32,
    values: impl Stream<Item = io::Result<Vec<u8>>>,
    gone: impl Future<Output = &'static str>,
) -> bool {
    let value = |value| ctx.events.send(RadioEvent::Value { conn: ConnId(conn), value });
    let why = until_down(values, gone, value).await;
    link_down(ctx, conn, &why)
}

/// Hands each value from `values` to `each` until the link goes down: why it went. BlueZ shuts the socket down as the
/// link drops, and a read then gives nothing, again and again, not an error; an empty value, which a peer never sends,
/// reads the same, so it ends the link too. `gone` comes, with why, when BlueZ says the device isn't connected
/// (NB-114), or lists HID among its services (NB110-14).
async fn until_down(
    values: impl Stream<Item = io::Result<Vec<u8>>>,
    gone: impl Future<Output = &'static str>,
    mut each: impl FnMut(Vec<u8>),
) -> String {
    tokio::pin!(values, gone);
    loop {
        tokio::select! {
            biased;
            v = values.next() => match v {
                Some(Ok(v)) if !v.is_empty() => each(v),
                Some(Ok(_)) | None => return "its socket closed".into(),
                Some(Err(e)) => return format!("reading it: {e}"),
            },
            why = &mut gone => return why.into(),
        }
    }
}

/// Comes once `changes` (a device's) say it isn't connected, or end, as BlueZ forgot it, or list HID among its
/// services, which a device can add once connected (NB110-14): why.
async fn link_gone(changes: impl Stream<Item = DeviceEvent>) -> &'static str {
    tokio::pin!(changes);
    while let Some(DeviceEvent::PropertyChanged(p)) = changes.next().await {
        match p {
            DeviceProperty::Connected(false) => break,
            DeviceProperty::Uuids(u) if u.contains(&uuid(HID_UUID)) => return "it lists HID now",
            _ => {}
        }
    }
    "BlueZ says it isn't connected"
}

/// The link of central connection `conn` went down, for `why`: it is reported closed, unless the driver already took
/// it out of `centrals` (closing it, or losing the adapter), and its writer stops. Whether it was reported. Its reader,
/// which calls this, isn't stopped: tokio would cancel a task that aborts itself at its next wait, before it reports.
fn link_down(ctx: &Ctx, conn: u32, why: &str) -> bool {
    ctx.shared.senders.lock().unwrap().remove(&conn);
    let Some(link) = ctx.centrals.lock().unwrap().remove(&conn) else { return false };
    link.write.abort();
    log::info!("Bluetooth: conn {conn} (to {}) went down: {}", tag(device_id(link.addr)), masked(why));
    ctx.events.send(RadioEvent::Disconnected { conn: ConnId(conn) });
    true
}

/// What serving a central that subscribed to `tx` takes.
struct Central {
    adapter: Adapter,
    ctx: Ctx,
}

impl Central {
    /// Serves the central behind `writer`, the only one at a time: its values go out as notifications until it stops
    /// the session or leaves, or the node closes it. Another one, or one whose socket takes too little, is refused
    /// and its link closed.
    async fn serve(self, writer: CharacteristicWriter) {
        let (addr, ctx) = (writer.device_address(), &self.ctx);
        let conn = {
            let mut p = ctx.peripheral.lock().unwrap();
            match att_mtu(writer.mtu()).filter(|_| p.conn.is_none()) {
                Some(mtu) => {
                    let conn = ctx.shared.next.fetch_add(1, Ordering::Relaxed);
                    *p = Peripheral { conn: Some(conn), from: Some(addr) };
                    Some((conn, mtu))
                }
                None => None,
            }
        };
        let Some((conn, mtu)) = conn else {
            let why =
                if att_mtu(writer.mtu()).is_some() { "one is served already" } else { "its socket takes too little" };
            log::debug!("Bluetooth: a central ({}) refused: {why}", tag(device_id(addr)));
            drop(writer);
            return drop_link(&self.adapter, &ctx.centrals, addr).await;
        };
        log::debug!("Bluetooth: conn {conn}: a central ({}) subscribed", tag(device_id(addr)));
        let (out, mut queue) = mpsc::channel::<Vec<u8>>(QUEUE);
        ctx.shared.senders.lock().unwrap().insert(conn, out);
        ctx.events.send(RadioEvent::Connected { conn: ConnId(conn), device: None, mtu });
        // Its link going down ends the session too, as BlueZ keeps the socket while another device subscribes.
        let adapter = self.adapter.clone();
        let left = async move {
            if let Ok(mut changes) = async { adapter.device(addr)?.events().await }.await {
                while let Some(DeviceEvent::PropertyChanged(p)) = changes.next().await {
                    if matches!(p, DeviceProperty::Connected(false)) {
                        return;
                    }
                }
            }
            std::future::pending::<()>().await
        };
        tokio::pin!(left);
        // Why it went, unless the node closed it (its queue ended).
        let went = loop {
            tokio::select! {
                v = queue.recv() => {
                    let Some(v) = v else { break None };
                    if writer.send(&v).await.is_err() {
                        break Some("a notification failed");
                    }
                }
                _ = writer.closed() => break Some("it unsubscribed, or its socket closed"),
                _ = &mut left => break Some("BlueZ says it isn't connected"),
            }
        };
        let closed = went.is_none();
        ctx.shared.senders.lock().unwrap().remove(&conn);
        {
            let mut p = ctx.peripheral.lock().unwrap();
            if p.conn == Some(conn) {
                *p = Peripheral::default();
                let why = went.unwrap_or("closed");
                log::info!("Bluetooth: conn {conn} (from {}) went down: {why}", tag(device_id(addr)));
                ctx.events.send(RadioEvent::Disconnected { conn: ConnId(conn) });
            }
        }
        // Its link drops here too, so that it does whichever of this and the driver's own close takes the slot
        // first: else the central could subscribe again at once (NB110-12).
        if closed {
            drop(writer);
            drop_link(&self.adapter, &ctx.centrals, addr).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::BoxFuture;
    use futures::stream::BoxStream;

    /// What a central link's socket reads.
    type Values = BoxStream<'static, io::Result<Vec<u8>>>;

    #[test]
    fn device_ids_and_addresses_round_trip() {
        let a = Address::new([0xde, 0xad, 0xbe, 0xef, 0x00, 0x01]);
        assert_eq!(address(device_id(a)), a);
        assert_eq!(device_id(a), DeviceId(0xdead_beef_0001));
        assert_eq!(uuid(SERVICE_UUID).to_string(), "1c3eea41-c2be-493c-ab0b-7707ad2dbed6");
        assert_eq!(uuid(RX_UUID).to_string(), "1c3eea42-c2be-493c-ab0b-7707ad2dbed6");
        assert_eq!(uuid(TX_UUID).to_string(), "1c3eea43-c2be-493c-ab0b-7707ad2dbed6");
        assert_eq!(uuid(HID_UUID).to_string(), "00001812-0000-1000-8000-00805f9b34fb");
    }

    /// bluer's sockets take values of up to 5 bytes under the ATT MTU: the node is told 3 over what they take, so its
    /// values fit, and nothing under the codec's least value, 20 bytes.
    #[test]
    fn the_node_is_told_an_mtu_its_values_fit() {
        assert_eq!(att_mtu(18), None, "an ATT MTU of 23");
        assert_eq!(att_mtu(19), None);
        assert_eq!(att_mtu(20), Some(23));
        assert_eq!(crate::ble::MIN_ATT_MTU, 23);
        assert_eq!(peerwire_core::ble::value_len(att_mtu(251).unwrap()), 251, "an ESP32's 256");
        assert_eq!(att_mtu(512), Some(515));
        assert_eq!(peerwire_core::ble::value_len(att_mtu(512).unwrap()), 512, "517");
    }

    /// NB-114: the log carries no device's address, so BlueZ's errors, which may name one, have theirs masked.
    #[test]
    fn the_log_masks_addresses_in_bluez_errors() {
        let path = "Method \"Connect\" doesn't exist on /org/bluez/hci0/dev_AA_BB_CC_DD_EE_0f";
        assert_eq!(masked(path), "Method \"Connect\" doesn't exist on /org/bluez/hci0/dev_<address>");
        assert_eq!(
            masked("aa:bb:cc:dd:ee:ff: le-connection-abort-by-local"),
            "<address>: le-connection-abort-by-local"
        );
        assert_eq!(masked("AA:BB:CC:DD:EE"), "AA:BB:CC:DD:EE", "not an address");
        assert_eq!(masked("AA-BB-CC-DD-EE-FF, é"), "AA-BB-CC-DD-EE-FF, é", "nor that, which BlueZ doesn't write");
        assert_eq!(masked("x11:22:33:44:55:66:77"), "x<address>:77");
    }

    #[test]
    fn an_alias_names_the_host_whatever_its_case() {
        assert!(same_name("laptop", "laptop\n"));
        assert!(same_name("Laptop", "laptop"));
        assert!(!same_name("Peerwire", "laptop"));
        assert!(!same_name("", ""), "no host name to name");
    }

    /// NB110-13: BlueZ's input plugin asks to pair with a device that serves HID unless its `input.conf` sets
    /// `LEAutoSecurity` false, read as GLib reads it. Each answer here is GLib 2.80's own: `GKeyFile`, as `hog.c`'s
    /// `hog_read_config` loads the file and reads the key, keeping true if either fails.
    #[test]
    fn le_auto_security_is_read_as_bluez_reads_it() {
        let cases = [
            ("", true),
            ("[General]\n#LEAutoSecurity=false\n", true),
            ("[General]\nLEAutoSecurity=false\n", false),
            ("[General]\nLEAutoSecurity=0\n", false),
            ("[General]\nLEAutoSecurity=true\n", true),
            ("[General]\nLEAutoSecurity=1\n", true),
            ("[General]\n  LEAutoSecurity  =  false  \n", false),
            ("[General]\r\nLEAutoSecurity=false\r\n", false),
            ("[General]  \nLEAutoSecurity=false", false),
            ("[General]\t\nLEAutoSecurity=false", false),
            ("[General] x\nLEAutoSecurity=false\n", true),
            ("[General]\nLEAutoSecurity=False\n", true),
            ("[General]\nLEAutoSecurity=no\n", true),
            ("[General]\nLEAutoSecurity=false # off\n", true),
            ("[General]\nLEAutoSecurity = \"false\"\n", true),
            ("[General]\nleautosecurity=false\n", true),
            ("[General]\nLEAutoSecurity[en]=false\n", true),
            ("[Other]\nLEAutoSecurity=false\n", true),
            ("LEAutoSecurity=false\n[General]\n", true),
            ("[General]\nLEAutoSecurity=false\nLEAutoSecurity=true\n", true),
            ("[General]\nLEAutoSecurity=true\nLEAutoSecurity=false\n", false),
            ("[General]\nLEAutoSecurity=false\n[Other]\nLEAutoSecurity=true\n", false),
            ("[General]\nLEAutoSecurity=true\n[Other]\n[General]\nLEAutoSecurity=false\n", false),
            ("[General]\nLEAutoSecurity=false\njunk\n", true),
            ("[General]\nLEAutoSecurity=false\n=x\n", true),
            ("[General]\nLEAutoSecurity=false\na]b=1\n", true),
            ("[General]\nLEAutoSecurity=false\na[en=1\n", true),
            ("[General]\nLEAutoSecurity=false\na[en]x=1\n", true),
            ("[General]\nLEAutoSecurity=false\na[en_GB.UTF-8@euro]=1\n", false),
            ("[General]\nLEAutoSecurity=false\na[]=1\n", false),
            ("[General]\nLEAutoSecurity=false\na b=1\n", false),
            ("[General]\nLEAutoSecurity=false\n[]\n", true),
            ("[General]\nLEAutoSecurity=false\n[a[b]\n", true),
            ("[General]\nLEAutoSecurity=false\n[a\n", true),
            ("[General]\nLEAutoSecurity=false\n[a]b]\n", true),
            ("[General]\nLEAutoSecurity=false\n[a\u{1}b]\n", true),
            ("[General]\nLEAutoSecurity=false\n  # indented comment\n", false),
            ("\n\n# comment first\n[General]\nLEAutoSecurity=false\n", false),
            ("[General]\nLEAutoSecurity=\n", true),
            ("[General]\nLEAutoSecurity=false\u{b}\n", true),
            ("\u{b}[General]\nLEAutoSecurity=false\n", true),
            ("[General]\nUserspaceHID=true\nClassicBondedOnly=true\nLEAutoSecurity=false\nIdleTimeout=0\n", false),
        ];
        for (conf, auto) in cases {
            assert_eq!(le_auto_security(conf), auto, "{conf:?}");
        }
    }

    /// NB110-13: a device the driver connects to that BlueZ had neither paired nor trusted counts as a stranger, with
    /// each of the driver's connects and links on it, until BlueZ has removed it; it stays one if BlueZ pairs it
    /// meanwhile, or its removal fails. One BlueZ had paired or trusted never counts.
    #[test]
    fn a_stranger_counts_until_bluez_has_removed_it() {
        let mut s = Strangers::default();
        let (a, b) = (addr(1), addr(2));
        assert!(!s.touch(a, true), "paired or trusted: the user's own");
        assert!(s.idle().is_empty() && !s.in_use(a));
        assert!(s.touch(b, false) && s.touch(b, true), "a second connect counts, whatever BlueZ says by then");
        assert!(s.in_use(b) && !s.release(b), "one is on it still");
        assert!(s.release(b) && !s.in_use(b), "the last one ended: to remove");
        assert_eq!(s.idle(), [b]);
        // Its removal failed: a connect to it counts, though BlueZ has it paired by now.
        assert!(s.touch(b, true) && s.idle().is_empty());
        s.removed(b);
        assert!(s.in_use(b), "not forgotten while a connect is on it");
        assert!(s.release(b));
        s.removed(b);
        assert!(s.idle().is_empty() && !s.in_use(b), "forgotten once removed");
        assert!(!s.release(a), "never counted");
    }

    /// NB-112: a part that fails is tried again 1, 2, 4, 8, 16 s later, then every 30 s, until it works; a new ask
    /// tries it at once.
    #[test]
    fn a_part_that_fails_is_tried_again_backing_off_to_30_s() {
        let t = Instant::now();
        let mut r = Retry::new("advertising");
        assert!(r.due(t), "tried at once");
        let wait = |r: &mut Retry| {
            r.failed(t, "refused");
            (r.at.unwrap() - t).as_secs()
        };
        let waits: Vec<u64> = (0..8).map(|_| wait(&mut r)).collect();
        assert_eq!(waits, [1, 2, 4, 8, 16, 30, 30, 30]);
        assert!(!r.due(t + Duration::from_secs(29)) && r.due(t + RETRY_MAX));
        r.now();
        assert!(r.due(t));
        r.up();
        assert_eq!((r.failures, r.at), (0, None));
        assert_eq!(wait(&mut r), 1, "a new run of failures starts at 1 s");
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    /// Lets every task that can run, run: the runtime runs them all before it waits for the timer.
    async fn quiet() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    fn addr(i: usize) -> Address {
        Address::new([0, 0, 0, 0, (i >> 8) as u8, i as u8])
    }

    /// NB-112: with no filter on the service, the scan hears every LE device nearby, so it follows at most `WATCH`:
    /// at that, a new device only if it advertises the service, in place of the oldest one that never has.
    #[test]
    fn the_scan_follows_a_bounded_number_of_devices() {
        runtime().block_on(async {
            let idle = || tokio::spawn(std::future::pending::<()>());
            let t = Instant::now();
            let mut w = Watching::default();
            for i in 0..WATCH {
                assert!(w.room(addr(i), false));
                w.add(addr(i), idle(), Arc::new(AtomicBool::new(i == 0 || i == 2)));
            }
            assert!(!w.room(addr(WATCH), false), "full: not a device that doesn't advertise the service");
            assert_eq!(w.relist_at(t), Some(t), "missed, and one of the service could take another's place");
            assert!(w.room(addr(WATCH), true), "but one that does");
            w.add(addr(WATCH), idle(), Arc::new(AtomicBool::new(true)));
            assert!(!w.devices.contains_key(&addr(1)), "in place of the oldest that never has");
            assert!([0, 2, WATCH].iter().all(|i| w.devices.contains_key(&addr(*i))));
            assert_eq!(w.devices.len(), WATCH);
            assert!(w.room(addr(3), false), "one followed already, added again, takes its own place");
            w.add(addr(3), idle(), Arc::new(AtomicBool::new(false)));
            w.remove(addr(4));
            assert!(w.room(addr(WATCH + 1), false), "one removed makes room");
            w.add(addr(WATCH + 1), idle(), Arc::new(AtomicBool::new(false)));
            // A device whose watch ended (BlueZ removed it) makes room too.
            let ended = w.devices.get(&addr(5)).map(|d| d.task.abort()).is_some();
            quiet().await;
            assert!(w.following(addr(0)) && !w.following(addr(5)), "one that ended isn't followed still (NB-116)");
            assert!(ended && w.room(addr(WATCH + 2), false), "one that ended makes room");
            assert!(!w.devices.contains_key(&addr(5)));
            let task = w.devices[&addr(0)].task.abort_handle();
            drop(w);
            quiet().await;
            assert!(task.is_finished(), "they stop with the scan");
        });
    }

    /// NB112-1: no device that has advertised the service is pushed out for another, as any device can put the
    /// service in its adverts: one that comes at the cap is missed instead. Once a device of the service could be
    /// followed, BlueZ's devices are listed again, at most every `RELIST`, and those not followed are followed, the
    /// service's first.
    #[test]
    fn a_burst_of_service_adverts_pushes_no_member_out() {
        runtime().block_on(async {
            let idle = || tokio::spawn(std::future::pending::<()>());
            let follow = |w: &mut Watching, a: Address, ours: bool| {
                let room = w.room(a, ours);
                if room {
                    w.add(a, idle(), Arc::new(AtomicBool::new(ours)));
                }
                room
            };
            let t = Instant::now();
            let mut w = Watching::default();
            let (alice, bob) = (addr(1000), addr(1001));
            assert!(follow(&mut w, alice, true));
            assert_eq!(w.relist_at(t), None, "none missed");
            // Mallory's burst, from as many addresses as it takes, with Bob's first advert in the middle of it.
            for i in 0..2 * WATCH {
                follow(&mut w, addr(i), true);
            }
            assert!(!follow(&mut w, bob, true), "full of the service's devices: one more is left out");
            assert!(w.follows(alice), "and the member followed stays");
            assert!((0..WATCH - 1).all(|i| w.follows(addr(i))) && !w.follows(addr(WATCH - 1)));
            assert_eq!(w.relist_at(t), None, "missed, but none could be followed: not listed again");
            // The burst stops, and BlueZ forgets its devices as they go unheard.
            w.devices[&addr(0)].task.abort();
            quiet().await;
            assert_eq!(w.relist_at(t), Some(t), "listed again at once");
            let found = vec![(addr(3000), false), (addr(1), true), (bob, true)];
            assert_eq!(
                w.listed(t, found),
                [(bob, true), (addr(3000), false)],
                "those not followed, the service's first"
            );
            assert_eq!(w.relist_at(t), None, "none missed now");
            assert!(follow(&mut w, bob, true) && w.follows(bob), "Bob is followed");
            assert!(!follow(&mut w, addr(3000), false), "and the cap is reached again");
            // Left out again: listed again `RELIST` after the last time, not before.
            w.remove(addr(1));
            assert_eq!(w.relist_at(t), Some(t + RELIST));
        });
    }

    fn ctx() -> (Ctx, std_mpsc::Receiver<RadioEvent>) {
        let (tx, rx) = std_mpsc::channel();
        let shared =
            Arc::new(Shared { senders: Mutex::new(HashMap::new()), next: AtomicU32::new(1), waker: Mutex::new(None) });
        let events = Events { tx, shared: shared.clone() };
        let peripheral = Arc::new(Mutex::new(Peripheral::default()));
        let centrals = Arc::new(Mutex::new(HashMap::new()));
        let strangers = Arc::new(Mutex::new(Strangers::default()));
        let turn = Arc::new(tokio::sync::Semaphore::new(1));
        let (linking, scans, held) = (watch::Sender::new(0), watch::Sender::new(0), watch::Sender::new(0));
        let removals = watch::Sender::new(0);
        (Ctx { events, shared, peripheral, centrals, turn, linking, scans, held, strangers, removals }, rx)
    }

    /// NB-112: when the adapter goes, every link on it is reported closed, a connect under way fails, and one that
    /// finished says nothing more; the devices are returned to close at BlueZ.
    #[test]
    fn links_on_an_adapter_that_goes_are_reported_closed() {
        runtime().block_on(async {
            let (ctx, events) = ctx();
            let idle = || tokio::spawn(std::future::pending::<()>());
            let done = tokio::spawn(async {});
            quiet().await;
            assert!(done.is_finished());
            let mut connects = vec![(DeviceId(1), idle()), (DeviceId(2), done)];
            let link = Link { write: idle(), read: idle(), addr: addr(3), _touch: None };
            let tasks = [link.write.abort_handle(), link.read.abort_handle(), connects[0].1.abort_handle()];
            ctx.centrals.lock().unwrap().insert(5, link);
            *ctx.peripheral.lock().unwrap() = Peripheral { conn: Some(7), from: Some(addr(4)) };
            for conn in [5, 7] {
                ctx.shared.senders.lock().unwrap().insert(conn, mpsc::channel(1).0);
            }
            let (closed, addrs) = drop_links(&ctx, &mut connects);
            assert_eq!(closed, 2);
            assert_eq!(addrs, [addr(1), addr(3), addr(4)], "the connect's device too, to stop it at BlueZ");
            let said: Vec<RadioEvent> = events.try_iter().collect();
            let closed = |c| RadioEvent::Disconnected { conn: ConnId(c) };
            assert_eq!(said, [RadioEvent::ConnectFailed { device: DeviceId(1) }, closed(5), closed(7)]);
            assert!(connects.is_empty() && ctx.centrals.lock().unwrap().is_empty());
            assert!(ctx.shared.senders.lock().unwrap().is_empty() && ctx.peripheral.lock().unwrap().conn.is_none());
            quiet().await;
            assert!(tasks.iter().all(tokio::task::AbortHandle::is_finished), "their tasks stopped");
        });
    }

    /// NB110-12: a connection the node closes leaves the books at once (the link is closed at BlueZ in a task of its
    /// own): a central connection's tasks stop, the one central's slot frees, and another connection's is untouched.
    #[test]
    fn a_connection_the_node_closes_leaves_the_books_at_once() {
        runtime().block_on(async {
            let (ctx, _events) = ctx();
            let idle = || tokio::spawn(std::future::pending::<()>());
            let link = Link { write: idle(), read: idle(), addr: addr(3), _touch: None };
            let tasks = [link.write.abort_handle(), link.read.abort_handle()];
            ctx.centrals.lock().unwrap().insert(5, link);
            *ctx.peripheral.lock().unwrap() = Peripheral { conn: Some(7), from: Some(addr(4)) };
            assert_eq!(forget(&ctx, 9), None, "not one of its");
            assert_eq!(ctx.peripheral.lock().unwrap().conn, Some(7));
            assert_eq!(forget(&ctx, 5), Some(addr(3)), "a central connection");
            assert!(ctx.centrals.lock().unwrap().is_empty());
            assert_eq!(forget(&ctx, 7), Some(addr(4)), "the central's");
            assert_eq!(ctx.peripheral.lock().unwrap().conn, None);
            assert_eq!(forget(&ctx, 7), None, "once");
            quiet().await;
            assert!(tasks.iter().all(tokio::task::AbortHandle::is_finished), "its tasks stopped");
        });
    }

    /// NB-114 (the NB-112 laptop run): a central link that drops is reported closed at once. BlueZ shuts its
    /// notification socket down, and a read then gives an empty value again and again, not an error: the reader took
    /// those for values and never reported it. BlueZ saying the device isn't connected, or forgetting it, or a read
    /// that fails ends it too; and one the driver took out of its books already isn't reported again.
    #[test]
    fn a_central_link_that_drops_is_reported_at_once() {
        runtime().block_on(async {
            let (ctx, events) = ctx();
            let link = |conn: u32| {
                let write = tokio::spawn(std::future::pending::<()>());
                let writer = write.abort_handle();
                let read = tokio::spawn(std::future::pending::<()>());
                let link = Link { write, read, addr: addr(conn as usize), _touch: None };
                ctx.centrals.lock().unwrap().insert(conn, link);
                writer
            };
            let never = futures::stream::pending::<io::Result<Vec<u8>>>;
            let c = &ctx;
            let read = |conn, values: Values, gone: BoxFuture<'static, &'static str>| async move {
                let read = read_link(c, conn, values, gone);
                tokio::time::timeout(Duration::from_secs(1), read).await.expect("it ends")
            };
            let closed = |c| RadioEvent::Disconnected { conn: ConnId(c) };
            let writer = link(5);
            let shut = (0..9).map(|i| Ok(if i == 0 { vec![1, 2] } else { Vec::new() }));
            assert!(read(5, futures::stream::iter(shut).chain(never()).boxed(), std::future::pending().boxed()).await);
            let value = RadioEvent::Value { conn: ConnId(5), value: vec![1, 2] };
            assert_eq!(events.try_iter().collect::<Vec<_>>(), [value, closed(5)], "the value, then its end");
            quiet().await;
            assert!(writer.is_finished() && ctx.centrals.lock().unwrap().is_empty(), "its writer stopped");
            let connected = |c| DeviceEvent::PropertyChanged(DeviceProperty::Connected(c));
            let up = futures::stream::iter([connected(true)]).chain(futures::stream::pending());
            let wait = tokio::time::timeout(Duration::from_millis(50), link_gone(up)).await;
            assert!(wait.is_err(), "not gone while it is connected");
            link(6);
            let says = futures::stream::iter([connected(true), connected(false)]).chain(futures::stream::pending());
            assert!(read(6, never().boxed(), link_gone(says).boxed()).await, "BlueZ says it isn't connected");
            link(7);
            assert!(read(7, never().boxed(), link_gone(futures::stream::empty()).boxed()).await, "BlueZ forgot it");
            link(8);
            let fails = futures::stream::iter([Err(io::Error::other("reset"))]).chain(never());
            assert!(read(8, fails.boxed(), std::future::pending().boxed()).await, "a read failed");
            assert_eq!(events.try_iter().collect::<Vec<_>>(), [closed(6), closed(7), closed(8)]);
            assert!(!read(9, futures::stream::empty().boxed(), std::future::pending().boxed()).await);
            assert_eq!(events.try_iter().count(), 0, "not one it had");
        });
    }

    /// NB110-14: a device can list HID only once its connect has read its services (it adds it to its GATT database
    /// later, or its link was up already, and BlueZ reads them after): a step of the set-up is given up for it, however
    /// long the device stalls the step, and a link closes for it; the device's services without HID end neither.
    #[test]
    fn a_central_link_ends_once_its_device_lists_hid() {
        runtime().block_on(async {
            let (ctx, events) = ctx();
            let services = |hid: bool| {
                let mut u = std::collections::HashSet::from([uuid(SERVICE_UUID)]);
                if hid {
                    u.insert(uuid(HID_UUID));
                }
                DeviceEvent::PropertyChanged(DeviceProperty::Uuids(u))
            };
            let changes = |hid: bool| {
                let first = DeviceEvent::PropertyChanged(DeviceProperty::Connected(true));
                futures::stream::iter([first, services(false), services(hid)]).chain(futures::stream::pending()).boxed()
            };
            async fn within<F: Future>(f: F) -> Result<F::Output, tokio::time::error::Elapsed> {
                tokio::time::timeout(Duration::from_millis(50), f).await
            }
            assert!(within(link_gone(changes(false))).await.is_err(), "not gone for its services without HID");
            assert_eq!(within(link_gone(changes(true))).await, Ok("it lists HID now"));
            // A step it stalls is given up, ...
            let stalled = std::future::pending::<bluer::Result<()>>();
            let e = within(unless_gone(stalled, &mut changes(true))).await.expect("given up").unwrap_err();
            assert_eq!(e.message, "it lists HID now");
            // ... and one that ends ends as it does, its services without HID.
            let done = async { Ok::<_, bluer::Error>(7) };
            assert_eq!(within(unless_gone(done, &mut changes(false))).await.expect("ends").unwrap(), 7);
            let failed = async { Err::<(), _>(bluer::Error { kind: bluer::ErrorKind::Failed, message: "no".into() }) };
            let e = within(unless_gone(failed, &mut changes(false))).await.expect("ends").unwrap_err();
            assert_eq!(e.message, "no");
            // A link: closed, and why, and reported closed.
            let why = until_down(futures::stream::pending(), link_gone(changes(true)), |_| {});
            assert_eq!(within(why).await.expect("it ends"), "it lists HID now");
            let write = tokio::spawn(std::future::pending::<()>());
            let read = tokio::spawn(std::future::pending::<()>());
            ctx.centrals.lock().unwrap().insert(5, Link { write, read, addr: addr(5), _touch: None });
            let never = futures::stream::pending::<io::Result<Vec<u8>>>();
            assert!(within(read_link(&ctx, 5, never, link_gone(changes(true)))).await.expect("it ends"));
            assert_eq!(events.try_iter().collect::<Vec<_>>(), [RadioEvent::Disconnected { conn: ConnId(5) }]);
        });
    }

    /// A central connection whose link goes down is reported closed once, by its reader or by the driver, whichever
    /// takes it out of `centrals` first; and its reader isn't cancelled on its way out, as it was when it aborted
    /// itself with the writer (the node never heard: NB-112).
    #[test]
    fn a_central_link_that_goes_down_is_reported_once() {
        runtime().block_on(async {
            let (ctx, events) = ctx();
            let (go, wait) = tokio::sync::oneshot::channel::<()>();
            let out = Arc::new(AtomicBool::new(false));
            let (c, o) = (ctx.clone(), out.clone());
            let read = tokio::spawn(async move {
                let _ = wait.await;
                if link_down(&c, 5, "its socket closed") {
                    tokio::task::yield_now().await; // as it waits for BlueZ to drop the link
                    o.store(true, Ordering::Relaxed);
                }
            });
            let write = tokio::spawn(std::future::pending::<()>());
            let writer = write.abort_handle();
            ctx.centrals.lock().unwrap().insert(5, Link { write, read, addr: addr(3), _touch: None });
            go.send(()).unwrap();
            quiet().await;
            assert_eq!(events.try_iter().collect::<Vec<_>>(), [RadioEvent::Disconnected { conn: ConnId(5) }]);
            assert!(out.load(Ordering::Relaxed), "the reader went on");
            assert!(writer.is_finished(), "the writer stopped");
            assert!(!link_down(&ctx, 5, "BlueZ says it isn't connected"), "and it isn't reported again");
            assert_eq!(events.try_iter().count(), 0);
        });
    }

    /// NB110-13: a stranger is removed from BlueZ once the last of the driver's connects and links on it ends,
    /// whichever way: a connect that fails or whose task is aborted, a link that drops, one the node closes, those on
    /// an adapter that goes. A device BlueZ had paired or trusted never is.
    #[test]
    fn a_stranger_is_removed_once_the_last_link_on_it_ends() {
        runtime().block_on(async {
            let (ctx, _events) = ctx();
            let removed = Arc::new(Mutex::new(Vec::new()));
            let touch = |i: usize, known: bool| {
                let r = removed.clone();
                Touch::new(&ctx.strangers, addr(i), known, move || r.lock().unwrap().push(i))
            };
            let gone = || {
                let mut g = std::mem::take(&mut *removed.lock().unwrap());
                g.sort();
                g
            };
            let idle = || tokio::spawn(std::future::pending::<()>());
            let link = |conn: u32, i: usize, touch: Option<Touch>| {
                let link = Link { write: idle(), read: idle(), addr: addr(i), _touch: touch };
                ctx.centrals.lock().unwrap().insert(conn, link);
            };
            drop(touch(1, false));
            assert_eq!(gone(), [1], "a connect that failed");
            assert!(touch(2, true).is_none(), "the user's own device");
            link(5, 3, touch(3, false));
            let again = touch(3, false);
            assert!(link_down(&ctx, 5, "its socket closed"));
            assert!(gone().is_empty(), "a link that dropped, with a connect to its device under way still");
            drop(again);
            assert_eq!(gone(), [3], "once that ended too");
            link(6, 4, touch(4, false));
            assert_eq!(forget(&ctx, 6), Some(addr(4)));
            assert_eq!(gone(), [4], "one the node closed");
            // The adapter goes, with a link and a connect under way, whose task is aborted.
            link(7, 5, touch(5, false));
            let t = touch(6, false);
            let task = tokio::spawn(async move {
                let _touch = t;
                std::future::pending::<()>().await
            });
            let mut connects = vec![(device_id(addr(6)), task)];
            assert_eq!(drop_links(&ctx, &mut connects).0, 1);
            quiet().await;
            assert_eq!(gone(), [5, 6]);
            let mut idle = ctx.strangers.lock().unwrap().idle();
            idle.sort();
            assert_eq!(idle, [1, 3, 4, 5, 6].map(addr), "each to remove until BlueZ has (`remove_stranger`)");
        });
    }

    /// A BlueZ for a test: what it was asked, in order, each answered `delay` later, and a removal never if `stuck`.
    #[derive(Clone, Default)]
    struct FakeBluez {
        asked: Arc<Mutex<Vec<(&'static str, Address)>>>,
        delay: Duration,
        stuck: bool,
    }

    impl FakeBluez {
        fn answer(&self, what: &'static str, addr: Address) -> BoxFuture<'static, bluer::Result<()>> {
            self.asked.lock().unwrap().push((what, addr));
            let (delay, stuck) = (self.delay, self.stuck && what == "remove");
            async move {
                tokio::time::sleep(delay).await;
                if stuck {
                    std::future::pending::<()>().await;
                }
                Ok(())
            }
            .boxed()
        }

        /// What it was asked about `addr`, since last asked.
        fn asked(&self, addr: Address) -> Vec<&'static str> {
            let mut asked = self.asked.lock().unwrap();
            let about = asked.iter().filter(|(_, a)| *a == addr).map(|(what, _)| *what).collect();
            asked.retain(|(_, a)| *a != addr);
            about
        }
    }

    impl Devices for FakeBluez {
        fn disconnect(&self, addr: Address) -> BoxFuture<'static, bluer::Result<()>> {
            self.answer("disconnect", addr)
        }

        fn remove(&self, addr: Address) -> BoxFuture<'static, bluer::Result<()>> {
            self.answer("remove", addr)
        }
    }

    /// The lead's review of NB110-13: a driver that closes (its `BlueZ` dropped) waits for BlueZ to remove the
    /// strangers it was linked or connecting to, and those whose removal was under way, as its runtime then drops the
    /// removals' tasks; `bound` at most, and not at all with none to wait for. The user's own device isn't removed.
    #[test]
    fn a_close_removes_the_strangers_it_was_linked_to() {
        runtime().block_on(async {
            let (ctx, _events) = ctx();
            let bluez = FakeBluez { delay: Duration::from_millis(100), ..Default::default() };
            let touch = |b: &FakeBluez, i: usize, known: bool| {
                Touch::new(&ctx.strangers, addr(i), known, removes(b, &ctx, addr(i)))
            };
            let link = |b: &FakeBluez, conn: u32, i: usize, known: bool| {
                let idle = || tokio::spawn(std::future::pending::<()>());
                let link = Link { write: idle(), read: idle(), addr: addr(i), _touch: touch(b, i, known) };
                ctx.centrals.lock().unwrap().insert(conn, link);
            };
            let connect = |b: &FakeBluez, i: usize| {
                let t = touch(b, i, false);
                let task = tokio::spawn(async move {
                    let _touch = t;
                    std::future::pending::<()>().await
                });
                (device_id(addr(i)), task)
            };
            let c = &ctx;
            // How long a close took.
            let closing = |mut connects: Vec<(DeviceId, JoinHandle<()>)>, bound| async move {
                let t = Instant::now();
                close(c, &mut connects, bound).await;
                t.elapsed()
            };
            let (removed, two) = (Duration::from_millis(200), Duration::from_secs(2));
            // A removal that failed earlier isn't waited for.
            ctx.strangers.lock().unwrap().0.insert(addr(9), 0);
            assert!(closing(vec![], two).await < Duration::from_millis(50), "nothing to wait for");
            assert!(bluez.asked(addr(9)).is_empty(), "nor tried again");
            // A connect under way alone: its use ends once its aborted task is dropped.
            let took = closing(vec![connect(&bluez, 1)], two).await;
            assert!(took >= removed && took < two, "{took:?}");
            assert_eq!(bluez.asked(addr(1)), ["disconnect", "remove"]);
            // A removal under way, a stranger linked, the user's own device linked, and a connect under way.
            link(&bluez, 4, 2, false);
            assert!(link_down(&ctx, 4, "its socket closed"));
            link(&bluez, 5, 3, false);
            link(&bluez, 6, 4, true);
            let took = closing(vec![connect(&bluez, 5)], two).await;
            assert!(took >= removed && took < two, "{took:?}");
            for i in [2, 3, 5] {
                assert_eq!(bluez.asked(addr(i)), ["disconnect", "remove"], "{i}");
            }
            assert!(bluez.asked(addr(4)).is_empty(), "the user's own isn't removed");
            assert!(ctx.centrals.lock().unwrap().is_empty());
            assert_eq!(ctx.strangers.lock().unwrap().0.keys().collect::<Vec<_>>(), [&addr(9)], "the rest are gone");
            // BlueZ doesn't answer: given up at the bound, and the device stays in it.
            let stuck = FakeBluez { stuck: true, ..Default::default() };
            link(&stuck, 7, 6, false);
            let took = tokio::time::timeout(two, closing(vec![], Duration::from_millis(300))).await;
            let took = took.expect("given up at the bound");
            assert!(took >= Duration::from_millis(300), "{took:?}");
            assert_eq!(stuck.asked(addr(6)), ["disconnect", "remove"]);
            assert!(ctx.strangers.lock().unwrap().idle().contains(&addr(6)));
        });
    }

    /// NB110-16: a failed connect closes the device's link only if BlueZ was asked to make it, not one up before (the
    /// user's keyboard, say) nor one never asked for. Not made, it is closed before the failure is reported (NB116-2);
    /// made, after (NB110-12). It ends with its report, its turn passed on in a task of its own once the scan has
    /// started again (NB116-3), so that one still under way has said nothing (`drop_links`).
    #[test]
    fn a_failed_connect_closes_only_a_link_it_asked_for() {
        runtime().block_on(async {
            let (ctx, events) = ctx();
            let bluez = FakeBluez { delay: Duration::from_millis(100), ..Default::default() };
            let failed = |i: usize| RadioEvent::ConnectFailed { device: device_id(addr(i)) };
            let said = || events.try_iter().collect::<Vec<_>>();
            let making = || Some(Making::new(ctx.turn.clone().try_acquire_owned().unwrap(), &ctx));
            let turns = || ctx.turn.available_permits();
            // Failed before it was asked for (its turn held), or its link up already (its turn given back): left.
            for (i, making) in [(1, making()), (2, None)] {
                let t = Instant::now();
                connect_failed(&bluez, &ctx, device_id(addr(i)), false, making).await;
                assert!(t.elapsed() < Duration::from_millis(50), "{i} waited on nothing");
                quiet().await;
                assert!(bluez.asked(addr(i)).is_empty(), "{i} left connected");
                assert_eq!(said(), [failed(i)]);
                assert_eq!((*ctx.linking.borrow(), turns()), (0, 1), "{i}'s turn given back");
            }
            // Asked for and not made: disconnected first, its turn held and nothing said until BlueZ answers; then
            // reported, with the scan held off still, which its turn waits for.
            let off = Counted::new(&ctx.held);
            let t = Instant::now();
            let failing = connect_failed(&bluez, &ctx, device_id(addr(3)), true, making());
            tokio::pin!(failing);
            assert!(tokio::time::timeout(Duration::from_millis(50), failing.as_mut()).await.is_err());
            assert_eq!(bluez.asked(addr(3)), ["disconnect"]);
            assert_eq!((*ctx.linking.borrow(), said()), (1, vec![]), "held, and not reported yet");
            failing.await;
            assert!(t.elapsed() >= Duration::from_millis(100) && t.elapsed() < Duration::from_millis(200));
            assert_eq!(said(), [failed(3)]);
            quiet().await;
            assert_eq!((*ctx.linking.borrow(), turns()), (0, 0), "the scan may start, and its turn waits for it");
            drop(off);
            quiet().await;
            assert_eq!(turns(), 1, "given back once it has");
            // Made: reported at once, then disconnected in a task of its own.
            let t = Instant::now();
            connect_failed(&bluez, &ctx, device_id(addr(4)), true, None).await;
            assert!(t.elapsed() < Duration::from_millis(50));
            assert_eq!(said(), [failed(4)]);
            quiet().await;
            assert_eq!(bluez.asked(addr(4)), ["disconnect"]);
            // Unless the driver has a link to it, an earlier connect's, which goes on.
            let idle = || tokio::spawn(std::future::pending::<()>());
            ctx.centrals.lock().unwrap().insert(5, Link { write: idle(), read: idle(), addr: addr(5), _touch: None });
            connect_failed(&bluez, &ctx, device_id(addr(5)), true, None).await;
            quiet().await;
            assert!(bluez.asked(addr(5)).is_empty());
            assert_eq!(said(), [failed(5)]);
        });
    }

    /// Discovery sessions for a test to feed: the start that hands out a new one each time, and a sender for each one
    /// started, which finds it closed once the session is dropped.
    fn sessions() -> (Start, Arc<Mutex<Vec<mpsc::UnboundedSender<AdapterEvent>>>>) {
        let fed = Arc::new(Mutex::new(Vec::new()));
        let f = fed.clone();
        let start: Start = Box::new(move || {
            let (tx, rx) = mpsc::unbounded_channel();
            f.lock().unwrap().push(tx);
            let found = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|e| (e, rx)) });
            async move { Ok::<Reports, String>(found.boxed()) }.boxed()
        });
        (start, fed)
    }

    /// What the scan has next within 50 ms, if anything.
    async fn soon(d: &mut Discovery) -> Option<Next> {
        tokio::time::timeout(Duration::from_millis(50), d.next()).await.ok()
    }

    /// Whether the scan is due to settle within 2 s, and settles.
    async fn due(d: &mut Discovery) -> bool {
        matches!(tokio::time::timeout(Duration::from_secs(2), d.next()).await, Ok(Next::Settle)) && d.settle().await
    }

    fn changed(n: Option<Next>) -> bool {
        matches!(n, Some(Next::Settle))
    }

    /// NB-116 (omarchy's runs 18–20): Linux starts a connect only from an advert the running discovery reports, and
    /// that reports a device once a cycle, so a connect asked for on one waited 10 s for the next, and was given up
    /// just before. The scan is held off while a connect is under way (here at once, and with no end to its pause),
    /// until each one has ended or its task was aborted, and then starts again; one that can't start again ends, as
    /// one whose session ends does.
    #[test]
    fn a_connect_holds_the_scan_off_until_it_ends() {
        runtime().block_on(async {
            let (ctx, _events) = ctx();
            let connects = &ctx.linking;
            let (mut start, fed) = sessions();
            let found = start().await.unwrap();
            let hour = Duration::from_secs(3600);
            let mut d = Discovery::new(found, start, &ctx, Duration::ZERO, hour);
            let session = |k: usize| fed.lock().unwrap()[k].clone();
            let added =
                |n: Option<Next>, i| matches!(n, Some(Next::Found(AdapterEvent::DeviceAdded(a))) if a == addr(i));
            assert!(d.settle().await);
            session(0).send(AdapterEvent::DeviceAdded(addr(1))).unwrap();
            assert!(added(soon(&mut d).await, 1), "it reports what it finds");
            let first = Counted::new(connects);
            assert!(changed(soon(&mut d).await) && d.settle().await);
            assert!(session(0).is_closed(), "a connect stops the session");
            let second = Counted::new(connects);
            assert!(changed(soon(&mut d).await) && d.settle().await);
            drop(first);
            assert!(changed(soon(&mut d).await) && d.settle().await);
            assert_eq!(fed.lock().unwrap().len(), 1, "not started again while one is under way");
            assert!(soon(&mut d).await.is_none(), "and nothing found meanwhile");
            // The second's task is aborted, as when the adapter goes.
            let task = tokio::spawn(async move {
                let _connecting = second;
                std::future::pending::<()>().await
            });
            quiet().await;
            task.abort();
            quiet().await;
            assert_eq!(*connects.borrow(), 0, "an aborted connect counts out");
            assert!(changed(soon(&mut d).await) && d.settle().await);
            assert_eq!(fed.lock().unwrap().len(), 2, "started again once none is under way");
            session(1).send(AdapterEvent::DeviceAdded(addr(2))).unwrap();
            assert!(added(soon(&mut d).await, 2));
            fed.lock().unwrap().truncate(1);
            assert!(matches!(soon(&mut d).await, Some(Next::Ended)), "its session ended");
            // One that can't start again ends, once no connect holds it off.
            let fails: Start = Box::new(|| async { Err::<Reports, String>("org.bluez.Error.NotReady".into()) }.boxed());
            let mut d = Discovery::new(futures::stream::pending().boxed(), fails, &ctx, Duration::ZERO, hour);
            let held = Counted::new(connects);
            assert!(d.settle().await && d.found.is_none(), "held off, it isn't started");
            drop(held);
            assert!(!d.settle().await && *ctx.held.borrow() == 0, "it doesn't start again, nor is it held off");
            drop(ctx);
            assert!(matches!(soon(&mut d).await, Some(Next::Ended)), "nor once the driver is gone");
        });
    }

    /// NB116-1: connects, however many and however long, leave the scan running `run` of each `run + pause`, so that
    /// it hears each device in range each time it starts. It is held off for one only once it has run `run`, for
    /// `pause` at most, and again once it has run `run`, while any is under way still.
    #[test]
    fn connects_leave_the_scan_running_between_pauses() {
        runtime().block_on(async {
            let (ctx, _events) = ctx();
            let connects = &ctx.linking;
            let (mut start, fed) = sessions();
            let found = start().await.unwrap();
            let (run, pause) = (Duration::from_millis(300), Duration::from_millis(500));
            let mut d = Discovery::new(found, start, &ctx, run, pause);
            let closed = |k: usize| fed.lock().unwrap()[k].is_closed();
            let started = || fed.lock().unwrap().len();
            let mut since = d.since;
            // One that starts as the session does holds it off once it has run `run`, ...
            let first = Counted::new(connects);
            assert!(changed(soon(&mut d).await) && d.settle().await && !closed(0), "not before it has run `run`");
            assert!(due(&mut d).await && closed(0) && d.since - since >= run, "but once it has");
            since = d.since;
            // ... for `pause` at most, though it is under way still, ...
            assert!(due(&mut d).await && started() == 2 && d.since - since >= pause, "it runs again after `pause`");
            since = d.since;
            assert!(soon(&mut d).await.is_none(), "and runs");
            // ... and is held off again once it has run `run`, while it is under way still.
            assert!(due(&mut d).await && closed(1) && d.since - since >= run, "held off again");
            drop(first);
            assert!(changed(soon(&mut d).await) && d.settle().await && started() == 3, "started again once none is");
            since = d.since;
            // One that starts within `run` of that holds it off at `run`, ...
            let second = Counted::new(connects);
            assert!(changed(soon(&mut d).await) && d.settle().await && !closed(2));
            assert!(due(&mut d).await && closed(2) && d.since - since >= run);
            drop(second);
            assert!(changed(soon(&mut d).await) && d.settle().await && started() == 4);
            // ... and one after that, at once.
            tokio::time::sleep(run).await;
            assert!(soon(&mut d).await.is_none(), "nothing is due with no connect under way");
            let _third = Counted::new(connects);
            assert!(changed(soon(&mut d).await) && d.settle().await && closed(3), "held off at once after `run`");
            assert_eq!(started(), 4);
        });
    }

    /// NB116-2: the kernel makes one LE link at a time, at the next advert of a device it was asked for once the scan
    /// is held off, so a connect is given `LINK` from then (and not while the scan runs): a device that never links, as
    /// it went silent after an advert, has its connect given up, not held until `SETUP` with the next one and the
    /// scan's start behind it. One whose link comes up goes on, as does one BlueZ answers; BlueZ's error is passed on.
    #[test]
    fn a_link_not_made_soon_after_the_scan_is_held_off_is_given_up() {
        assert!(PAUSE > LINK + ABORT && RUN + LINK + ABORT < SETUP, "the connect ends a pause, and fails within SETUP");
        runtime().block_on(async {
            let within = Duration::from_millis(200);
            let scans = watch::Sender::new(1);
            let never = || std::future::pending::<bluer::Result<()>>().boxed();
            // `linked`, for a connect that `connecting` stands for, and the device's changes `then`.
            let link = |connecting: BoxFuture<'static, bluer::Result<()>>, then: Vec<DeviceProperty>| {
                let events = then.into_iter().map(DeviceEvent::PropertyChanged);
                let mut changes = futures::stream::iter(events).chain(futures::stream::pending()).boxed();
                let scans = scans.subscribe();
                async move {
                    let mut connecting = connecting;
                    linked(Pin::new(&mut connecting), &mut changes, scans, within).await
                }
            };
            let soon = |f| tokio::time::timeout(Duration::from_secs(2), f);
            // A device that never links: not given up while the scan runs, ...
            let mut silent = Box::pin(link(never(), vec![]));
            assert!(tokio::time::timeout(Duration::from_millis(400), &mut silent).await.is_err(), "while it runs");
            // ... but `within` after it is held off.
            scans.send_replace(0);
            let t = Instant::now();
            let e = soon(silent).await.expect("given up").unwrap_err();
            assert!(t.elapsed() >= within && e.message.starts_with("not linked"), "{:?} {}", t.elapsed(), e.message);
            // Its link up: on to its services, before BlueZ answers. Other changes don't count.
            let up = link(never(), vec![DeviceProperty::Connected(false), DeviceProperty::Connected(true)]);
            let t = Instant::now();
            assert!(!soon(Box::pin(up)).await.expect("up").unwrap() && t.elapsed() < within);
            let not_up = link(never(), vec![DeviceProperty::Connected(false), DeviceProperty::Rssi(-40)]);
            assert!(soon(Box::pin(not_up)).await.expect("given up").unwrap_err().message.starts_with("not linked"));
            // BlueZ answers: the link is up (or was already), or it failed.
            assert!(soon(Box::pin(link(async { Ok(()) }.boxed(), vec![]))).await.expect("answered").unwrap());
            let failed = async { Err(bluer::Error { kind: bluer::ErrorKind::Failed, message: "abort".into() }) };
            let e = soon(Box::pin(link(failed.boxed(), vec![]))).await.expect("answered").unwrap_err();
            assert_eq!(e.message, "abort");
        });
    }

    /// NB116-2: the scan runs `run` from when its start has returned, however long BlueZ takes to start it (it answers
    /// once the kernel has, which may wait behind a link being made), before a link being made holds it off again; and
    /// it counts in [`Ctx::scans`] while it runs, not while it is held off, nor once it has ended.
    #[test]
    fn the_scan_runs_its_time_after_a_slow_start() {
        runtime().block_on(async {
            let (ctx, _events) = ctx();
            let (mut start, fed) = sessions();
            let found = start().await.unwrap();
            let slow: Start = Box::new(move || {
                let started = start();
                async move {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    started.await
                }
                .boxed()
            });
            let (run, pause) = (Duration::from_millis(200), Duration::from_millis(100));
            let mut d = Discovery::new(found, slow, &ctx, run, pause);
            let scans = || *ctx.scans.borrow();
            assert_eq!(scans(), 1, "it runs");
            let _linking = Counted::new(&ctx.linking);
            assert!(changed(soon(&mut d).await) && d.settle().await && d.found.is_some(), "not before `run`");
            assert!(due(&mut d).await && d.found.is_none() && scans() == 0, "held off once it has run `run`");
            assert!(due(&mut d).await && d.found.is_some() && scans() == 1, "started again after `pause`, slowly");
            let started = Instant::now();
            assert!(due(&mut d).await && d.found.is_none(), "held off again");
            assert!(started.elapsed() >= run, "`run` after the start returned: {:?}", started.elapsed());
            assert_eq!((scans(), fed.lock().unwrap().len()), (0, 2));
            assert!(due(&mut d).await && scans() == 1);
            drop(d);
            assert_eq!(scans(), 0, "not once it has ended");
        });
    }

    /// NB116-3: a connect that ends, its link given up (or up), passes its turn on once the scan it held off has
    /// started again, however long BlueZ takes to start it: so the next connect's link is asked for with the scan
    /// running, and the scan runs `run` between two connects that never link. At once with no session held off, as
    /// with no scan; `ABORT` after at most, should BlueZ not start it.
    #[test]
    fn a_connect_passes_its_turn_on_once_the_scan_runs_again() {
        runtime().block_on(async {
            let (ctx, _events) = ctx();
            let (mut start, fed) = sessions();
            let found = start().await.unwrap();
            let slow = Duration::from_millis(100);
            let slowly: Start = Box::new(move || {
                let started = start();
                async move {
                    tokio::time::sleep(slow).await;
                    started.await
                }
                .boxed()
            });
            let (run, ms) = (Duration::from_millis(300), Duration::from_millis(1));
            let mut d = Discovery::new(found, slowly, &ctx, run, Duration::from_secs(3600));
            // As `follow` runs it.
            let scan = tokio::spawn(async move { while d.settle().await && !matches!(d.next().await, Next::Ended) {} });
            let (scans, held) = (|| *ctx.scans.borrow(), || *ctx.held.borrow());
            let turn = |ctx: &Ctx| ctx.turn.clone().acquire_owned();
            // A makes its link, and B waits for its turn.
            let a = Making::new(turn(&ctx).await.unwrap(), &ctx);
            let c = ctx.clone();
            let b = tokio::spawn(async move {
                let b = Making::new(turn(&c).await.unwrap(), &c);
                (Instant::now(), *c.scans.borrow(), b)
            });
            tokio::time::sleep(run + 50 * ms).await;
            assert_eq!((scans(), held()), (0, 1), "the scan is held off for A once it has run `run`");
            // A's link is given up: B's turn comes once the scan has started again.
            let t = Instant::now();
            a.release(ctx.held.clone()).await;
            let (turned, running, b) = b.await.unwrap();
            assert!(turned - t >= slow && turned - t < ABORT, "B's turn {:?} after A's end", turned - t);
            assert_eq!((running, held(), fed.lock().unwrap().len()), (1, 0, 2), "with the scan running");
            // It runs `run` before it is held off for B.
            tokio::time::sleep_until(turned + run - 50 * ms).await;
            assert_eq!(scans(), 1, "not held off for B before it has run `run`");
            tokio::time::sleep(100 * ms).await;
            assert_eq!((scans(), held()), (0, 1), "but once it has");
            // With no scan, the next turn comes at once.
            scan.abort();
            quiet().await;
            assert_eq!(held(), 0, "a scan that ended isn't held off");
            let t = Instant::now();
            b.release(ctx.held.clone()).await;
            assert!(t.elapsed() < 50 * ms && ctx.turn.available_permits() == 1, "{:?}", t.elapsed());
            // A session BlueZ doesn't start again holds the next turn back `ABORT` at most.
            let c = Making::new(turn(&ctx).await.unwrap(), &ctx);
            let _off = Counted::new(&ctx.held);
            let t = Instant::now();
            let released = tokio::time::timeout(ABORT + 100 * ms, c.release(ctx.held.clone())).await;
            assert!(released.is_ok() && t.elapsed() >= ABORT, "{:?}", t.elapsed());
            assert_eq!(ctx.turn.available_permits(), 1);
        });
    }

    /// NB116-4: a connect's `SETUP` counts from the node's asking, its wait for its turn included, as the node's own
    /// wait does: one queued behind a turn that is held, as by connects to silent devices, is reported failed once it
    /// is up, and holds nothing; one whose turn comes in time takes it, and the scan is held off for it.
    #[test]
    fn a_connect_whose_turn_doesnt_come_in_time_fails() {
        assert!(SETUP + ABORT < crate::ble::CONNECT_WAIT, "the node hears first");
        runtime().block_on(async {
            let (ctx, events) = ctx();
            let within = Duration::from_millis(200);
            let id = |i| device_id(addr(i));
            let first = take_turn(&ctx, id(1), within).await.expect("its turn, at once");
            assert_eq!(*ctx.linking.borrow(), 1);
            let t = Instant::now();
            let queued = tokio::time::timeout(4 * within, take_turn(&ctx, id(2), within)).await;
            assert!(matches!(queued, Ok(None)), "not its turn");
            assert!(t.elapsed() >= within && t.elapsed() < 2 * within, "{:?}", t.elapsed());
            assert!(matches!(events.try_recv(), Ok(RadioEvent::ConnectFailed { device }) if device == id(2)));
            assert_eq!(*ctx.linking.borrow(), 1, "the scan held off for the first alone");
            // One whose turn comes in time.
            let c = ctx.clone();
            let third = tokio::spawn(async move { take_turn(&c, id(3), within).await.map(|m| *m.linking.0.borrow()) });
            quiet().await;
            first.release(ctx.held.clone()).await;
            assert_eq!(third.await.unwrap(), Some(1), "it takes its turn");
            assert!(events.try_recv().is_err(), "and isn't reported");
            assert_eq!((*ctx.linking.borrow(), ctx.turn.available_permits()), (0, 1));
        });
    }
}

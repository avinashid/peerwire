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
//! off has its links reported closed too, and is set up again once powered on, by its user.

use crate::ble::{ConnId, DeviceId, Limits, Radio, RadioEvent, RX_UUID, SERVICE_UUID, TX_UUID};
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
use futures::stream::{LocalBoxStream, Stream};
use futures::{FutureExt, StreamExt};
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::task::Waker;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// Values queued per connection in the driver; beyond that, [`Radio::send`] says no and the node keeps them.
const QUEUE: usize = 32;
/// How long [`BlueZ::new`] waits for the adapter.
const START: Duration = Duration::from_secs(10);
/// A connect not set up by then (the link, the service found, both sockets) fails, and its link drops: so a device
/// that stalls it can't hold one of the node's connects for bluer's 120 s a step, and the node, which gives one up
/// after 12 s, hears why first (NB110-11).
const SETUP: Duration = Duration::from_secs(10);
/// A part that failed is tried again this long after, doubled at each failure in a row, up to `RETRY_MAX`.
const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);
/// How often the adapter in use is asked whether it is powered: a bluetoothd that crashes says nothing.
const CHECK: Duration = Duration::from_secs(10);
/// Devices the scan follows at once ([`Watching`]).
const WATCH: usize = 256;

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
pub struct BlueZ {
    cmds: mpsc::UnboundedSender<Cmd>,
    events: std_mpsc::Receiver<RadioEvent>,
    shared: Arc<Shared>,
    alias: String,
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
        let s = shared.clone();
        std::thread::Builder::new().name("peerwire-bluez".into()).spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => return drop(ready_tx.send(Err(e))),
            };
            let events = Events { tx: events_tx, shared: s.clone() };
            rt.block_on(Driver::run(rx, events, s, ready_tx));
        })?;
        let alias = ready.recv_timeout(START).map_err(|_| io::Error::other("BlueZ didn't answer in time"))??;
        Ok(BlueZ { cmds, events, shared, alias })
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

/// Whether an adapter alias is a host name, ignoring case and the white space around it.
fn same_name(alias: &str, host: &str) -> bool {
    let host = host.trim();
    !host.is_empty() && alias.trim().eq_ignore_ascii_case(host)
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
async fn drop_link(adapter: &Adapter, centrals: &Centrals, addr: Address) {
    if centrals.lock().unwrap().values().any(|l| l.addr == addr) {
        return;
    }
    if let Ok(dev) = adapter.device(addr) {
        let _ = dev.disconnect().await;
    }
}

fn device_id(a: Address) -> DeviceId {
    let mut b = [0u8; 8];
    b[2..].copy_from_slice(&a.0);
    DeviceId(u64::from_be_bytes(b))
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
}

/// The central connections, by number.
type Centrals = Arc<Mutex<HashMap<u32, Link>>>;

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
        let ctx = Ctx { events, shared, peripheral, centrals: Arc::new(Mutex::new(HashMap::new())) };
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
                    None => break,
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
        let mut on = self.find().await?;
        let alias = on.adapter.alias().await.map_err(|e| e.to_string())?;
        on.app = Some(serve(&on.adapter, &self.ctx).await.map_err(|e| e.to_string())?);
        self.on = Some(on);
        Ok(alias)
    }

    /// The default adapter, powered on.
    async fn find(&self) -> Result<On, String> {
        let adapter = self.session.default_adapter().await.map_err(|e| e.to_string())?;
        adapter.set_powered(true).await.map_err(|e| format!("powering {} on: {e}", adapter.name()))?;
        let changes = adapter.events().await.map_err(|e| e.to_string())?.boxed_local();
        log::info!("Bluetooth: on adapter {}", adapter.name());
        Ok(On { adapter, changes, powered: true, app: None, advert: None, scan: None })
    }

    /// Brings up what is down and due: the adapter, then on it the GATT service, the advert and the scan.
    async fn settle(&mut self) {
        let now = Instant::now();
        if self.on.is_none() {
            if !self.retry_adapter.due(now) {
                return;
            }
            match self.find().await {
                Ok(on) => {
                    self.retry_adapter.up();
                    self.on = Some(on);
                    self.retry_app.now();
                    self.retry_advert.now();
                    self.retry_scan.now();
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
            match scan(&on.adapter, &self.ctx.events).await {
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

    /// The adapter in use holds no links now, for `why`: each is reported closed, and closed at BlueZ for an adapter
    /// still there; connects under way fail; the GATT service, the advert and the scan are down.
    fn lose(&mut self, why: &str) {
        let addrs = drop_links(&self.ctx, &mut self.connects);
        log::warn!("Bluetooth: {why}; {} links closed", addrs.len());
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
/// closes. The devices they were to, to close at BlueZ.
fn drop_links(ctx: &Ctx, connects: &mut Vec<(DeviceId, JoinHandle<()>)>) -> Vec<Address> {
    // One that hasn't finished hasn't said `Connected` (nothing between that and its end waits), nor `ConnectFailed`.
    for (device, t) in connects.drain(..) {
        if !t.is_finished() {
            t.abort();
            ctx.events.send(RadioEvent::ConnectFailed { device });
        }
    }
    let links: Vec<(u32, Link)> = ctx.centrals.lock().unwrap().drain().collect();
    let mut addrs = Vec::new();
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
    }
    addrs.extend(p.from);
    addrs
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
fn drop_later(adapter: &Adapter, centrals: &Centrals, addrs: impl IntoIterator<Item = Address>) {
    for a in addrs {
        let (adapter, centrals) = (adapter.clone(), centrals.clone());
        tokio::spawn(async move { drop_link(&adapter, &centrals, a).await });
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
async fn scan(adapter: &Adapter, events: &Events) -> Result<JoinHandle<()>, String> {
    let filter = DiscoveryFilter { transport: DiscoveryTransport::Le, duplicate_data: true, ..Default::default() };
    adapter.set_discovery_filter(filter).await.map_err(|e| format!("the filter: {e}"))?;
    let found = adapter.discover_devices().await.map_err(|e| format!("discovery: {e}"))?;
    Ok(tokio::spawn(follow(adapter.clone(), events.clone(), found)))
}

/// Reports the service's adverts among what the scan finds, until it ends.
async fn follow(
    adapter: Adapter,
    events: Events,
    mut found: impl Stream<Item = AdapterEvent> + Send + Unpin + 'static,
) {
    let mut watching = Watching::default();
    while let Some(e) = found.next().await {
        match e {
            AdapterEvent::DeviceAdded(addr) => {
                let Ok(dev) = adapter.device(addr) else { continue };
                let ours = matches!(dev.service_data().await, Ok(Some(data)) if seen(&events, addr, &data));
                if !watching.room(addr, ours) {
                    continue;
                }
                let Ok(mut changes) = dev.events().await else { continue };
                let (events, ours) = (events.clone(), Arc::new(AtomicBool::new(ours)));
                let flag = ours.clone();
                let watch = tokio::spawn(async move {
                    while let Some(DeviceEvent::PropertyChanged(p)) = changes.next().await {
                        if let DeviceProperty::ServiceData(data) = p {
                            if seen(&events, addr, &data) {
                                flag.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                });
                watching.add(addr, watch, ours);
            }
            AdapterEvent::DeviceRemoved(addr) => watching.remove(addr),
            _ => {}
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
/// only one that advertises the service already, in place of the device followed longest that never has. One BlueZ
/// removes (unheard for its `TemporaryTimeout`, 30 s by default) is dropped, and all of them with the scan.
#[derive(Default)]
struct Watching {
    devices: HashMap<Address, Watched>,
    added: u64,
}

struct Watched {
    task: JoinHandle<()>,
    added: u64,
    /// It has advertised the service.
    ours: Arc<AtomicBool>,
}

impl Watching {
    /// Whether to follow `addr`, which advertises the service if `ours`, making room for it if need be.
    fn room(&mut self, addr: Address, ours: bool) -> bool {
        self.remove(addr);
        self.devices.retain(|_, w| !w.task.is_finished());
        if self.devices.len() < WATCH {
            return true;
        }
        if !ours {
            return false;
        }
        let oldest = |others: bool| {
            let them = self.devices.iter().filter(|(_, w)| !(others && w.ours.load(Ordering::Relaxed)));
            them.min_by_key(|(_, w)| w.added).map(|(a, _)| *a)
        };
        if let Some(a) = oldest(true).or_else(|| oldest(false)) {
            self.remove(a);
        }
        true
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
/// without response. One that isn't set up within `SETUP`, or fails, is reported failed and then disconnected, which
/// also stops a connect BlueZ still has under way: reported first, as the disconnect is answered only once the link
/// is down (NB110-12), and this task ends with it, so one still under way has said nothing (`drop_links`).
async fn connect(adapter: Adapter, ctx: Ctx, device: DeviceId) {
    let addr = address(device);
    let Ok(dev) = adapter.device(addr) else {
        return ctx.events.send(RadioEvent::ConnectFailed { device });
    };
    let setup = async {
        dev.connect().await?;
        let mut rx = None;
        let mut tx = None;
        for s in dev.services().await? {
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
            return Err(bluer::Error { kind: bluer::ErrorKind::ServicesUnresolved, message: String::new() });
        };
        let reader = tx.notify_io().await?;
        let writer = rx.write_io().await?;
        Ok::<_, bluer::Error>((reader, writer))
    };
    let set_up = tokio::time::timeout(SETUP, setup).await.ok().and_then(Result::ok);
    let mtu = set_up.as_ref().and_then(|(r, w)| att_mtu(w.mtu().min(r.mtu())));
    let (Some((reader, writer)), Some(mtu)) = (set_up, mtu) else {
        ctx.events.send(RadioEvent::ConnectFailed { device });
        return drop_later(&adapter, &ctx.centrals, [addr]);
    };
    // Nothing from here on waits, so a connect still under way hasn't said `Connected` (`drop_links`).
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
        while let Ok(value) = reader.recv().await {
            c.events.send(RadioEvent::Value { conn: ConnId(conn), value });
        }
        if link_down(&c, conn) {
            let _ = dev.disconnect().await;
        }
    });
    ctx.centrals.lock().unwrap().insert(conn, Link { write, read, addr });
}

/// The link of central connection `conn` went down: it is reported closed, unless the driver already took it out of
/// `centrals` (closing it, or losing the adapter), and its writer stops. Whether it was reported. Its reader, which
/// calls this, isn't stopped: tokio would cancel a task that aborts itself at its next wait, before it reports.
fn link_down(ctx: &Ctx, conn: u32) -> bool {
    ctx.shared.senders.lock().unwrap().remove(&conn);
    let Some(link) = ctx.centrals.lock().unwrap().remove(&conn) else { return false };
    link.write.abort();
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
            drop(writer);
            return drop_link(&self.adapter, &ctx.centrals, addr).await;
        };
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
        // Whether the node closed it (its queue ended).
        let closed = loop {
            tokio::select! {
                v = queue.recv() => {
                    let Some(v) = v else { break true };
                    if writer.send(&v).await.is_err() {
                        break false;
                    }
                }
                _ = writer.closed() => break false,
                _ = &mut left => break false,
            }
        };
        ctx.shared.senders.lock().unwrap().remove(&conn);
        {
            let mut p = ctx.peripheral.lock().unwrap();
            if p.conn == Some(conn) {
                *p = Peripheral::default();
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

    #[test]
    fn device_ids_and_addresses_round_trip() {
        let a = Address::new([0xde, 0xad, 0xbe, 0xef, 0x00, 0x01]);
        assert_eq!(address(device_id(a)), a);
        assert_eq!(device_id(a), DeviceId(0xdead_beef_0001));
        assert_eq!(uuid(SERVICE_UUID).to_string(), "1c3eea41-c2be-493c-ab0b-7707ad2dbed6");
        assert_eq!(uuid(RX_UUID).to_string(), "1c3eea42-c2be-493c-ab0b-7707ad2dbed6");
        assert_eq!(uuid(TX_UUID).to_string(), "1c3eea43-c2be-493c-ab0b-7707ad2dbed6");
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

    #[test]
    fn an_alias_names_the_host_whatever_its_case() {
        assert!(same_name("laptop", "laptop\n"));
        assert!(same_name("Laptop", "laptop"));
        assert!(!same_name("Peerwire", "laptop"));
        assert!(!same_name("", ""), "no host name to name");
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
            let mut w = Watching::default();
            for i in 0..WATCH {
                assert!(w.room(addr(i), false));
                w.add(addr(i), idle(), Arc::new(AtomicBool::new(i == 0 || i == 2)));
            }
            assert!(!w.room(addr(WATCH), false), "full: not a device that doesn't advertise the service");
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
            assert!(ended && w.room(addr(WATCH + 2), false), "one that ended makes room");
            assert!(!w.devices.contains_key(&addr(5)));
            // All of them members: the oldest goes.
            let mut m = Watching::default();
            for i in 0..WATCH {
                m.room(addr(i), true);
                m.add(addr(i), idle(), Arc::new(AtomicBool::new(true)));
            }
            assert!(m.room(addr(WATCH), true) && !m.devices.contains_key(&addr(0)));
            let task = m.devices[&addr(1)].task.abort_handle();
            drop(m);
            quiet().await;
            assert!(task.is_finished(), "they stop with the scan");
        });
    }

    fn ctx() -> (Ctx, std_mpsc::Receiver<RadioEvent>) {
        let (tx, rx) = std_mpsc::channel();
        let shared =
            Arc::new(Shared { senders: Mutex::new(HashMap::new()), next: AtomicU32::new(1), waker: Mutex::new(None) });
        let events = Events { tx, shared: shared.clone() };
        let peripheral = Arc::new(Mutex::new(Peripheral::default()));
        (Ctx { events, shared, peripheral, centrals: Arc::new(Mutex::new(HashMap::new())) }, rx)
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
            let link = Link { write: idle(), read: idle(), addr: addr(3) };
            let tasks = [link.write.abort_handle(), link.read.abort_handle(), connects[0].1.abort_handle()];
            ctx.centrals.lock().unwrap().insert(5, link);
            *ctx.peripheral.lock().unwrap() = Peripheral { conn: Some(7), from: Some(addr(4)) };
            for conn in [5, 7] {
                ctx.shared.senders.lock().unwrap().insert(conn, mpsc::channel(1).0);
            }
            let addrs = drop_links(&ctx, &mut connects);
            assert_eq!(addrs, [addr(3), addr(4)]);
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
            let link = Link { write: idle(), read: idle(), addr: addr(3) };
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
                if link_down(&c, 5) {
                    tokio::task::yield_now().await; // as it waits for BlueZ to drop the link
                    o.store(true, Ordering::Relaxed);
                }
            });
            let write = tokio::spawn(std::future::pending::<()>());
            let writer = write.abort_handle();
            ctx.centrals.lock().unwrap().insert(5, Link { write, read, addr: addr(3) });
            go.send(()).unwrap();
            quiet().await;
            assert_eq!(events.try_iter().collect::<Vec<_>>(), [RadioEvent::Disconnected { conn: ConnId(5) }]);
            assert!(out.load(Ordering::Relaxed), "the reader went on");
            assert!(writer.is_finished(), "the writer stopped");
            assert!(!link_down(&ctx, 5), "and it isn't reported again");
            assert_eq!(events.try_iter().count(), 0);
        });
    }
}

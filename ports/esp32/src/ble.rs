//! Bluetooth LE on ESP-IDF's NimBLE (feature `ble`): [`Nimble`], the radio driver a [`Node`](peerwire::Node) runs
//! over on a board (docs/concepts/bluetooth.md, `peerwire::ble`).
//!
//! The node does everything but move GATT values. This driver serves the Peerwire service, advertises the node's
//! service data from a non-resolvable random address that changes with each new advert (BLE-R3), with the service's
//! UUID in the scan response, scans passively, connects where the node asks, and moves values on up to three
//! connections ([`Limits::SMALL`]). A central that doesn't subscribe to `tx` within 5 s is closed, so a silent one
//! can't hold a connection the node never hears of. Nothing is paired or bonded (BLE-R5): bonding, MITM and Secure
//! Connections are off, and a connection that a pairing completes on anyway is closed. `rx` takes writes without
//! response, and sdkconfig.ble leaves NimBLE no room for prepared (long) writes (BLE-R7). An advert that isn't on the
//! air is reported to the node (`Advertising`) and tried again. Each connection coming up or closing is logged with
//! its role and reason, and so are NimBLE's connections against the driver's whenever either count changes.
//!
//! NimBLE calls back on its own task, where the driver only queues events for [`Radio::poll`]. What waits on the
//! radio (a new address, connecting, finding the service, subscribing) runs on a thread of the driver's own, so no
//! call the node makes blocks.

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::ptr;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::task::Waker;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, ensure};
use esp32_nimble::enums::{AuthReq, OwnAddrType};
use esp32_nimble::utilities::BleUuid;
use esp32_nimble::{BLEDevice, NimbleProperties, NimbleSub};
use esp_idf_svc::sys;
use log::{debug, info, warn};
use peerwire::ble::{ConnId, DeviceId, Limits, Radio, RadioEvent, RX_UUID, SERVICE_UUID, TX_UUID};

/// The ATT MTU asked for: a datagram of up to 1,232 bytes goes in 5 values (sdkconfig.ble says the same).
const MTU: u16 = 256;
/// As sdkconfig.ble's `BT_NIMBLE_MAX_CONNECTIONS`, and [`Limits::SMALL`].
const MAX_CONNECTIONS: usize = 3;
/// The GAP device name, the same on every peer and never the host's (the spec's Names).
const GAP_NAME: &str = "Peerwire";
/// Passive scanning, 30 ms in every 100 (0.625 ms units).
const SCAN_INTERVAL: u16 = 160;
const SCAN_WINDOW: u16 = 48;
/// A connect, with the steps after it (the MTU, the service, `tx`'s CCCD), fails if not done within this: the node
/// gives a connect up after 12 s, and the driver's own failure comes first (the spec's Limits).
const SETUP: Duration = Duration::from_secs(10);
/// A central that hasn't subscribed to `tx` this long after connecting is closed: the node never hears of it, so it
/// couldn't, and it holds one of NimBLE's three connections. Checked about once a second while the driver's thread
/// isn't connecting.
const SUBSCRIBE_WITHIN: Duration = Duration::from_secs(5);
const SWEEP: Duration = Duration::from_secs(1);
/// An advert that won't go on the air is tried again after the first, then twice as long each time up to the second.
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);
/// Events queued for the node at most; adverts beyond the first number and values beyond the second are dropped,
/// as the radio would drop them, so a device nearby can't fill the heap.
const MAX_SEEN_QUEUED: usize = 16;
const MAX_QUEUED: usize = 64;
/// Values a peripheral notifies before this central has told the node it is connected, kept until it has.
const MAX_EARLY: usize = 4;
/// The largest value a peer can send at the MTU asked for.
const MAX_VALUE: usize = MTU as usize - 3;
/// Advertising data types: flags, the complete list of 128-bit service UUIDs, service data under one.
const AD_FLAGS: u8 = 0x01;
const AD_UUIDS_128: u8 = 0x07;
const AD_SERVICE_DATA_128: u8 = 0x21;
/// LE General Discoverable, no BR/EDR.
const FLAGS: u8 = 0x06;
/// The Client Characteristic Configuration descriptor, and its value that turns notifications on.
const CCCD: u16 = 0x2902;
const NOTIFY_ON: [u8; 2] = [1, 0];
/// Remote User Terminated Connection.
const TERMINATED: u8 = sys::ble_error_codes_BLE_ERR_REM_USER_CONN_TERM as u8;
const OWN_ADDR: u8 = sys::BLE_OWN_ADDR_RANDOM as u8;
const EDONE: u16 = sys::BLE_HS_EDONE as u16;
const ENOTCONN: u16 = sys::BLE_HS_ENOTCONN as u16;
const EALREADY: i32 = sys::BLE_HS_EALREADY as i32;
/// NimBLE gives a disconnect's HCI reason plus this.
const HCI_BASE: i32 = sys::BLE_HS_ERR_HCI_BASE as i32;
/// The connection handles looked at for those NimBLE has: ESP's controllers number them from 0, a few at most.
const HANDLES: u16 = 32;

/// The board's radio as a node drives it: NimBLE, serving the Peerwire GATT service.
///
/// ```no_run
/// use peerwire_esp::ble::Nimble;
/// use peerwire_esp::peerwire::Node;
///
/// let radio = Nimble::new()?; // before the node: see Nimble::new
/// let mut node = Node::builder().config(peerwire_esp::config()).bluetooth(radio).bind("127.0.0.1:4040")?;
/// node.join_room("my-app", "kitchen"); // links with members nearby over Bluetooth
/// # Ok::<(), anyhow::Error>(())
/// ```
pub struct Nimble {
    shared: Arc<Shared>,
    tx: u16,
}

/// What NimBLE's callbacks, the driver's thread and the node's calls share.
struct Shared {
    state: Mutex<State>,
    /// The driver's thread.
    worker: Mutex<mpsc::Sender<Cmd>>,
}

#[derive(Default)]
struct State {
    events: VecDeque<RadioEvent>,
    waker: Option<Waker>,
    /// Connections the node has heard of or will, by NimBLE's handle.
    links: HashMap<u16, Link>,
    /// Connections centrals made to this radio, subscribed or not.
    peripheral: usize,
    /// Those not subscribed yet, by NimBLE's handle, with when they connected.
    waiting: Vec<(u16, Instant)>,
    /// The last [`ConnId`] given: never reused.
    last: u32,
}

struct Link {
    conn: ConnId,
    /// `rx`'s value handle on the peer, where this radio is the central (0 until it is found); `None` where a central
    /// connected to this radio.
    rx: Option<u16>,
    /// Whether the node has heard `Connected`.
    up: bool,
    mtu: u16,
    early: Vec<Vec<u8>>,
}

/// What the driver's thread does, in order.
enum Cmd {
    Advertise(Option<Vec<u8>>),
    /// Advertise again, as NimBLE stops when a central connects: if there is an advert and room for a connection.
    Readvertise,
    Scan(bool),
    /// Scan again if scanning is wanted: NimBLE ended it.
    Rescan,
    Connect(DeviceId),
}

impl Nimble {
    /// Starts NimBLE and serves the Peerwire service, neither advertising nor scanning until the node, in a room,
    /// asks. Fails if NimBLE won't start or can't make a random address.
    ///
    /// Make it before the node, and before anything else draws random numbers on a board without Wi-Fi: the chip's
    /// random numbers are true ones only while a radio runs (BLE-R15).
    pub fn new() -> anyhow::Result<Nimble> {
        let ble = BLEDevice::take();
        ble.set_preferred_mtu(MTU)?;
        // What a connected central reads as the GAP name: the spec's, not esp32-nimble's `nimble`.
        BLEDevice::set_device_name(GAP_NAME)?;
        // Every address this radio uses is a fresh non-resolvable random one, never the efuse MAC (BLE-R3).
        // esp32-nimble does that only on the original ESP32: ask NimBLE here, for each advert.
        new_address()?;
        ble.set_own_addr_type(OwnAddrType::Random);
        // No bonding, MITM or Secure Connections (BLE-R5): a central that pairs anyway gets legacy Just Works, which
        // runs no P-256, and the connection then closes.
        ble.security().set_auth(AuthReq::empty());

        let (worker, cmds) = mpsc::channel();
        let shared = Arc::new(Shared { state: Mutex::default(), worker: Mutex::new(worker) });
        let server = ble.get_server();
        server.advertise_on_disconnect(false);
        server.on_connect({
            let shared = shared.clone();
            move |_, desc| {
                let mut state = shared.lock();
                state.peripheral += 1;
                state.waiting.push((desc.conn_handle(), Instant::now()));
                drop(state);
                shared.send(Cmd::Readvertise);
            }
        });
        server.on_disconnect({
            let shared = shared.clone();
            move |desc, _| {
                let mut state = shared.lock();
                state.peripheral = state.peripheral.saturating_sub(1);
                state.closed(desc.conn_handle());
                drop(state);
                shared.send(Cmd::Readvertise);
            }
        });
        server.on_authentication_complete(|_, desc, _| {
            warn!("ble: a central paired, closing its connection (nothing is paired)");
            terminate(desc.conn_handle());
        });

        let service = server.create_service(uuid(SERVICE_UUID));
        let tx = service.lock().create_characteristic(uuid(TX_UUID), NimbleProperties::NOTIFY);
        let rx = service.lock().create_characteristic(uuid(RX_UUID), NimbleProperties::WRITE_NO_RSP);
        tx.lock().on_subscribe({
            let shared = shared.clone();
            move |_, desc, sub| {
                let handle = desc.conn_handle();
                if shared.lock().subscribed(handle, desc.mtu(), sub.contains(NimbleSub::NOTIFY)) {
                    terminate(handle); // its notify session ended, as BlueZ's driver has it
                }
            }
        });
        rx.lock().on_write({
            let shared = shared.clone();
            move |args| {
                let (handle, mtu, value) = (args.desc().conn_handle(), args.desc().mtu(), args.recv_data());
                // One write carries at most MTU - 3 bytes; a longer one could only be a long write, which NimBLE
                // has no room for here (BLE-R7).
                if value.len() > usize::from(mtu.saturating_sub(3)) {
                    args.reject();
                    return;
                }
                shared.lock().written(handle, mtu, value);
            }
        });
        server.start()?;
        let tx = tx_handle()?;

        // NimBLE's discovery callback and its event listener get the shared state for as long as the program runs.
        let scan_arg = Arc::into_raw(shared.clone()) as usize;
        // Each connection coming and going, in either role and whether or not the node hears of it, for the log.
        let listener = Box::leak(Box::new(unsafe { std::mem::zeroed::<sys::ble_gap_event_listener>() }));
        let rc = unsafe { sys::ble_gap_event_listener_register(listener, Some(on_link), scan_arg as *mut c_void) };
        ensure!(rc == 0, "ble_gap_event_listener_register: {rc}");
        let worker = Worker {
            shared: shared.clone(),
            cmds,
            scan_arg,
            advert: None,
            unset: None,
            on_air: true,
            retry: None,
            scanning: false,
            said: None,
        };
        std::thread::Builder::new().name("ble".into()).stack_size(8 * 1024).spawn(move || worker.run())?;
        info!("ble: NimBLE up, serving the Peerwire service, {} B free", crate::free_heap());
        Ok(Nimble { shared, tx })
    }
}

impl Radio for Nimble {
    fn advertise(&mut self, data: Option<&[u8]>) {
        self.shared.send(Cmd::Advertise(data.map(<[u8]>::to_vec)));
    }

    fn scan(&mut self, on: bool) {
        self.shared.send(Cmd::Scan(on));
    }

    fn connect(&mut self, device: DeviceId) {
        self.shared.send(Cmd::Connect(device));
    }

    fn send(&mut self, conn: ConnId, value: &[u8]) -> bool {
        let found = self.shared.lock().find(conn).filter(|(_, link)| link.up).map(|(handle, link)| (handle, link.rx));
        let Some((handle, rx)) = found else {
            return true; // closed: dropped, as the node will hear
        };
        let Ok(len) = u16::try_from(value.len()) else { return true };
        let rc = match rx {
            Some(rx) => unsafe { sys::ble_gattc_write_no_rsp_flat(handle, rx, value.as_ptr().cast(), len) },
            None => {
                let om = unsafe { sys::ble_hs_mbuf_from_flat(value.as_ptr().cast(), len) };
                if om.is_null() {
                    return false;
                }
                // Consumes the buffer, sent or not.
                unsafe { sys::ble_gatts_notify_custom(handle, self.tx, om) }
            }
        };
        // Out of buffers: the node keeps the value and tries again. Anything else drops it.
        rc != sys::BLE_HS_ENOMEM as i32
    }

    fn disconnect(&mut self, conn: ConnId) {
        let handle = self.shared.lock().find(conn).map(|(handle, _)| handle);
        if let Some(handle) = handle {
            terminate(handle);
        }
    }

    fn poll(&mut self) -> Option<RadioEvent> {
        self.shared.lock().events.pop_front()
    }

    fn limits(&self) -> Limits {
        Limits::SMALL
    }

    fn set_waker(&mut self, waker: Waker) {
        self.shared.lock().waker = Some(waker);
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn send(&self, cmd: Cmd) {
        let _ = self.worker.lock().unwrap_or_else(|e| e.into_inner()).send(cmd);
    }
}

impl State {
    fn push(&mut self, event: RadioEvent) {
        let full = match event {
            RadioEvent::Seen { .. } => self.events.len() >= MAX_SEEN_QUEUED,
            RadioEvent::Value { .. } => self.events.len() >= MAX_QUEUED,
            _ => false,
        };
        if full {
            return;
        }
        self.events.push_back(event);
        if let Some(waker) = &self.waker {
            waker.wake_by_ref();
        }
    }

    fn next_conn(&mut self) -> ConnId {
        self.last += 1;
        ConnId(self.last)
    }

    fn find(&self, conn: ConnId) -> Option<(u16, &Link)> {
        self.links.iter().find(|(_, link)| link.conn == conn).map(|(handle, link)| (*handle, link))
    }

    /// A central subscribed to `tx` on `handle`, or stopped. Returns whether to close the connection.
    fn subscribed(&mut self, handle: u16, mtu: u16, on: bool) -> bool {
        match (self.links.get(&handle), on) {
            (None, true) => {
                self.waiting.retain(|&(h, _)| h != handle);
                let conn = self.next_conn();
                self.links.insert(handle, Link { conn, rx: None, up: true, mtu, early: Vec::new() });
                self.push(RadioEvent::Connected { conn, device: None, mtu });
                false
            }
            (Some(link), false) => link.rx.is_none(),
            _ => false, // a peer this radio connected to, subscribing as if it were a central: not the node's
        }
    }

    /// A central wrote `value` to `rx` on `handle`, where the ATT MTU is `mtu`.
    fn written(&mut self, handle: u16, mtu: u16, value: &[u8]) {
        let Some(link) = self.links.get_mut(&handle).filter(|link| link.rx.is_none()) else {
            return; // not subscribed yet, or a peer this radio connected to: not the node's
        };
        let conn = link.conn;
        if link.mtu != mtu {
            link.mtu = mtu;
            self.push(RadioEvent::Mtu { conn, mtu });
        }
        self.push(RadioEvent::Value { conn, value: value.to_vec() });
    }

    /// A peripheral this radio connected to notified `value` on `tx`.
    fn notified(&mut self, handle: u16, value: Vec<u8>) {
        let Some(link) = self.links.get_mut(&handle) else { return };
        if link.up {
            let conn = link.conn;
            self.push(RadioEvent::Value { conn, value });
        } else if link.early.len() < MAX_EARLY {
            link.early.push(value);
        }
    }

    fn mtu(&mut self, handle: u16, mtu: u16) {
        let Some(link) = self.links.get_mut(&handle).filter(|link| link.mtu != mtu) else { return };
        link.mtu = mtu;
        if link.up {
            let conn = link.conn;
            self.push(RadioEvent::Mtu { conn, mtu });
        }
    }

    fn closed(&mut self, handle: u16) {
        self.waiting.retain(|&(h, _)| h != handle);
        if let Some(link) = self.links.remove(&handle).filter(|link| link.up) {
            self.push(RadioEvent::Disconnected { conn: link.conn });
        }
    }

    fn centrals(&self) -> usize {
        self.links.values().filter(|link| link.rx.is_some()).count()
    }
}

/// The driver's thread: advertising, scanning and connecting, one at a time.
struct Worker {
    shared: Arc<Shared>,
    cmds: mpsc::Receiver<Cmd>,
    /// The `Shared` NimBLE's discovery callback gets, never freed.
    scan_arg: usize,
    /// The advert set up, from its own address.
    advert: Option<Vec<u8>>,
    /// The one the node asked for, while it couldn't be set up.
    unset: Option<Vec<u8>>,
    /// Whether the node was last told its advert is on the air, as it takes it to be until told otherwise.
    on_air: bool,
    /// While the advert isn't on the air: when to try it again, and the wait before that.
    retry: Option<(Instant, Duration)>,
    scanning: bool,
    /// The connections last logged.
    said: Option<Count>,
}

/// The connections NimBLE has and those the driver has, each in (a central connected) and out (this radio did).
#[derive(Clone, Copy, PartialEq, Eq)]
struct Count {
    nimble: (usize, usize),
    driver: (usize, usize),
    advertising: bool,
}

impl Worker {
    fn run(mut self) {
        loop {
            // The advert back on the air if it isn't, once a second at most, and one that failed when it's due.
            if self.retry.is_none_or(|(at, _)| Instant::now() >= at) {
                match self.unset.take() {
                    Some(data) => self.set_advert(data),
                    None => self.readvertise(),
                }
            }
            match self.cmds.recv_timeout(SWEEP) {
                Ok(Cmd::Advertise(data)) => self.advertise(data),
                Ok(Cmd::Readvertise) => self.readvertise(),
                Ok(Cmd::Scan(on)) => {
                    self.scanning = on;
                    match on {
                        true => self.start_scan(),
                        false => stop_scan(),
                    }
                }
                Ok(Cmd::Rescan) if self.scanning => self.start_scan(),
                Ok(Cmd::Rescan) | Err(RecvTimeoutError::Timeout) => {}
                Ok(Cmd::Connect(device)) => {
                    stop_scan(); // NimBLE can't connect while it scans
                    self.connect(device);
                    if self.scanning {
                        self.start_scan();
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
            self.close_unsubscribed();
            self.count();
        }
    }

    /// Logs the connections NimBLE has and the driver's when either changes: one NimBLE keeps that the driver doesn't
    /// is a link nobody uses or closes.
    fn count(&mut self) {
        // NimBLE's own lock first, never inside the driver's.
        let mut nimble = (0, 0);
        for handle in 0..HANDLES {
            let mut desc: sys::ble_gap_conn_desc = unsafe { std::mem::zeroed() };
            if unsafe { sys::ble_gap_conn_find(handle, &mut desc) } == 0 {
                match u32::from(desc.role) == sys::BLE_GAP_ROLE_MASTER {
                    true => nimble.1 += 1,
                    false => nimble.0 += 1,
                }
            }
        }
        let advertising = unsafe { sys::ble_gap_adv_active() } != 0;
        let state = self.shared.lock();
        let now = Count { nimble, driver: (state.peripheral, state.centrals()), advertising };
        drop(state);
        if self.said != Some(now) {
            self.said = Some(now);
            let (n, d) = (now.nimble, now.driver);
            let air = if advertising { "advertising" } else { "not advertising" };
            info!("ble: NimBLE has {} in, {} out; the driver {} in, {} out; {air}", n.0, n.1, d.0, d.1);
        }
    }

    /// Closes the centrals that haven't subscribed within [`SUBSCRIBE_WITHIN`] of connecting.
    fn close_unsubscribed(&self) {
        let mut late = Vec::new();
        self.shared.lock().waiting.retain(|&(handle, at)| match at.elapsed() < SUBSCRIBE_WITHIN {
            true => true,
            false => {
                late.push(handle);
                false
            }
        });
        for handle in late {
            debug!("ble: a central didn't subscribe within {SUBSCRIBE_WITHIN:?}, closing it");
            terminate(handle);
        }
    }

    /// A new advert, from a new address (BLE-R3), or none.
    fn advertise(&mut self, data: Option<Vec<u8>>) {
        let _ = BLEDevice::take().get_advertising().lock().stop();
        (self.advert, self.unset, self.retry) = (None, None, None);
        if let Some(data) = data {
            self.set_advert(data);
        }
    }

    /// Sets `data` up as the advert, from a new address, and starts it.
    fn set_advert(&mut self, data: Vec<u8>) {
        // NimBLE takes a new address only while it neither advertises nor scans.
        stop_scan();
        let set = new_address().and_then(|()| {
            let mut a = BLEDevice::take().get_advertising().lock();
            a.set_raw_data(&advert(&data)?).map_err(|e| anyhow!("advert: {e:?}"))?;
            a.set_raw_scan_response_data(&SCAN_RESPONSE).map_err(|e| anyhow!("scan response: {e:?}"))
        });
        match set {
            Ok(()) => {
                self.advert = Some(data);
                self.readvertise();
            }
            // Never the new advert from the old address, which would tie the two together.
            Err(e) => {
                self.unset = Some(data);
                self.off_air(format_args!("not advertising: {e}"));
            }
        }
        if self.scanning {
            self.start_scan();
        }
    }

    /// The same advert from the same address: the address changes with `n`, when the node gives a new advert (the
    /// spec's Rotation), not when a connection opens or closes. With no room for a connection there is no advert, as
    /// the node's own count says.
    fn readvertise(&mut self) {
        let state = self.shared.lock();
        let room = state.peripheral + state.centrals() < MAX_CONNECTIONS;
        drop(state);
        if self.advert.is_none() || !room {
            self.retry = None;
            return;
        }
        let mut a = BLEDevice::take().get_advertising().lock();
        let started = if a.is_advertising() { Ok(()) } else { a.start() };
        drop(a);
        match started {
            Ok(()) => self.on_air(),
            Err(e) => self.off_air(format_args!("advertising didn't start: {e:?}")),
        }
    }

    /// The advert is on the air: tells the node if it last heard otherwise.
    fn on_air(&mut self) {
        self.retry = None;
        if !self.on_air {
            self.on_air = true;
            info!("ble: advertising works again");
            self.shared.lock().push(RadioEvent::Advertising(true));
        }
    }

    /// The advert isn't on the air: tells the node once, so it connects to a peer of a larger `n` too (the spec's
    /// Who connects), and tries again 1, 2, 4, 8, 16, then every 30 s.
    fn off_air(&mut self, why: std::fmt::Arguments) {
        let wait = self.retry.map_or(RETRY_FIRST, |(_, wait)| (wait * 2).min(RETRY_MAX));
        self.retry = Some((Instant::now() + wait, wait));
        if self.on_air {
            self.on_air = false;
            warn!("ble: {why}; trying again in {wait:?}");
            self.shared.lock().push(RadioEvent::Advertising(false));
        } else {
            debug!("ble: {why}; trying again in {wait:?}");
        }
    }

    fn start_scan(&self) {
        if unsafe { sys::ble_gap_disc_active() } != 0 {
            return;
        }
        let mut params = sys::ble_gap_disc_params { itvl: SCAN_INTERVAL, window: SCAN_WINDOW, ..Default::default() };
        // Passive: nothing is sent, and the tag is in the advert itself (BLE-R3). Every advert, not one per address:
        // the node keeps the last of each.
        params.set_passive(1);
        params.set_filter_duplicates(0);
        let rc = unsafe { sys::ble_gap_disc(OWN_ADDR, i32::MAX, &params, Some(on_scan), self.scan_arg as *mut c_void) };
        if rc != 0 {
            warn!("ble: scanning didn't start ({rc})");
        }
    }

    /// Connects to `device`, finds the service, subscribes to `tx` and tells the node, or tells it the connect failed.
    fn connect(&self, device: DeviceId) {
        let conn = self.shared.lock().next_conn();
        let (done, steps) = mpsc::sync_channel(4);
        let central = Arc::new(Central { shared: self.shared.clone(), conn, done, found: Mutex::default() });
        if let Err(e) = self.set_up(&central, device, &steps) {
            info!("ble: connect to {:012x} failed: {e:#}", device.0 & 0xffff_ffff_ffff);
            // A connection that comes up from now on is closed as it does (on_central).
            let handle = {
                let mut found = central.found();
                found.gave_up = true;
                found.handle
            };
            if let Some(handle) = handle {
                terminate(handle);
            }
            self.shared.lock().push(RadioEvent::ConnectFailed { device });
        }
    }

    fn set_up(&self, central: &Arc<Central>, device: DeviceId, steps: &mpsc::Receiver<u16>) -> anyhow::Result<()> {
        let by = Instant::now() + SETUP;
        let addr = ble_addr(device);
        // The connection's callback holds this until its last event.
        let arg = Arc::into_raw(central.clone()) as *mut c_void;
        let ms = SETUP.as_millis() as i32;
        let rc = unsafe { sys::ble_gap_connect(OWN_ADDR, &addr, ms, ptr::null(), Some(on_central), arg) };
        if rc != 0 {
            drop(unsafe { Arc::from_raw(arg as *const Central) });
            bail!("ble_gap_connect: {rc}");
        }
        if let Err(e) = step(steps, by) {
            // NimBLE ends every connect with an event, which frees `arg`: this ends one it has let run on.
            if unsafe { sys::ble_gap_conn_active() } != 0 {
                unsafe { sys::ble_gap_conn_cancel() };
            }
            bail!("connect: {e}");
        }
        let handle = central.found().handle.ok_or_else(|| anyhow!("connected with no handle"))?;
        // A peer may ask first, as BlueZ does as soon as the link is up: NimBLE answers it and then takes the MTU as
        // exchanged, so there is nothing to wait for. The MTU is read once subscribed, below.
        match unsafe { sys::ble_gattc_exchange_mtu(handle, Some(on_mtu), arg) } {
            EALREADY => {}
            rc => ok("MTU", rc, steps, by)?,
        }
        let service = uuid128(SERVICE_UUID);
        let rc = unsafe { sys::ble_gattc_disc_svc_by_uuid(handle, &service.u, Some(on_service), arg) };
        ok("service", rc, steps, by)?;
        let (start, end) = central.found().service.ok_or_else(|| anyhow!("no Peerwire service"))?;
        ok(
            "characteristics",
            unsafe { sys::ble_gattc_disc_all_chrs(handle, start, end, Some(on_chr), arg) },
            steps,
            by,
        )?;
        let (rx, tx, tx_end) = central.found().handles(end).ok_or_else(|| anyhow!("no rx or tx"))?;
        ok("descriptors", unsafe { sys::ble_gattc_disc_all_dscs(handle, tx, tx_end, Some(on_dsc), arg) }, steps, by)?;
        let cccd = central.found().cccd.ok_or_else(|| anyhow!("tx has no CCCD"))?;
        let on = NOTIFY_ON.as_ptr().cast();
        ok("subscribe", unsafe { sys::ble_gattc_write_flat(handle, cccd, on, 2, Some(on_write), arg) }, steps, by)?;

        // NimBLE's own lock first, never inside the driver's.
        let mtu = unsafe { sys::ble_att_mtu(handle) };
        let mut state = self.shared.lock();
        let link = state.links.get_mut(&handle).ok_or_else(|| anyhow!("closed while setting up"))?;
        link.rx = Some(rx);
        link.up = true;
        link.mtu = mtu;
        let early = std::mem::take(&mut link.early);
        state.push(RadioEvent::Connected { conn: central.conn, device: Some(device), mtu });
        for value in early {
            state.push(RadioEvent::Value { conn: central.conn, value });
        }
        Ok(())
    }
}

/// A connection this radio makes, as NimBLE's callbacks for it see it.
struct Central {
    shared: Arc<Shared>,
    conn: ConnId,
    /// Each step's status, 0 when it worked, for the driver's thread.
    done: mpsc::SyncSender<u16>,
    found: Mutex<Found>,
}

/// What finding the service has found.
#[derive(Default)]
struct Found {
    handle: Option<u16>,
    service: Option<(u16, u16)>,
    /// Each characteristic's definition and value handles, and its UUID if a 128-bit one.
    chrs: Vec<(u16, u16, Option<u128>)>,
    cccd: Option<u16>,
    /// `tx`'s value handle, once subscribed to.
    tx: u16,
    /// Whether the driver's thread gave the connect up, so the node heard it failed: a connection that comes up after
    /// that would be used and closed by nobody.
    gave_up: bool,
}

impl Central {
    fn found(&self) -> MutexGuard<'_, Found> {
        self.found.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Never blocks NimBLE's task: a status nobody waits for is dropped.
    fn done(&self, status: u16) {
        let _ = self.done.try_send(status);
    }
}

impl Found {
    /// `rx`'s and `tx`'s value handles, and the last handle that can be `tx`'s descriptor. Notes `tx`'s.
    fn handles(&mut self, service_end: u16) -> Option<(u16, u16, u16)> {
        let value = |uuid| self.chrs.iter().find(|c| c.2 == Some(uuid)).map(|c| c.1);
        let (rx, tx) = (value(RX_UUID)?, value(TX_UUID)?);
        let next = self.chrs.iter().map(|c| c.0).filter(|&def| def > tx).min();
        self.tx = tx;
        Some((rx, tx, next.map_or(service_end, |def| def - 1)))
    }
}

/// Waits for the next step's status, until `by`.
fn step(steps: &mpsc::Receiver<u16>, by: Instant) -> anyhow::Result<()> {
    match steps.recv_timeout(by.saturating_duration_since(Instant::now())) {
        Ok(0) => Ok(()),
        Ok(status) => bail!("status {status}"),
        Err(_) => bail!("no answer within {SETUP:?} of the connect"),
    }
}

/// A step started with `rc`, then its status by `by`.
fn ok(what: &str, rc: i32, steps: &mpsc::Receiver<u16>, by: Instant) -> anyhow::Result<()> {
    ensure!(rc == 0, "{what}: {rc}");
    step(steps, by).map_err(|e| anyhow!("{what}: {e}"))
}

/// A connection's events, from NimBLE's task. Its `arg` is a `Central` the connection holds until its last event.
unsafe extern "C" fn on_central(event: *mut sys::ble_gap_event, arg: *mut c_void) -> i32 {
    let central = unsafe { &*(arg as *const Central) };
    let event = unsafe { &*event };
    match u32::from(event.type_) {
        sys::BLE_GAP_EVENT_CONNECT => {
            let connect = unsafe { event.__bindgen_anon_1.connect };
            if connect.status == 0 {
                let gave_up = {
                    let mut found = central.found();
                    found.handle = Some(connect.conn_handle);
                    found.gave_up
                };
                if gave_up {
                    terminate(connect.conn_handle); // its DISCONNECT frees `arg`
                    return 0;
                }
                let link = Link { conn: central.conn, rx: Some(0), up: false, mtu: MIN_MTU, early: Vec::new() };
                central.shared.lock().links.insert(connect.conn_handle, link);
                central.done(0);
            } else {
                central.done(u16::try_from(connect.status).unwrap_or(u16::MAX).max(1));
                drop(unsafe { Arc::from_raw(arg as *const Central) }); // no connection: its last event
            }
        }
        sys::BLE_GAP_EVENT_DISCONNECT => {
            let handle = unsafe { event.__bindgen_anon_1.disconnect.conn.conn_handle };
            central.shared.lock().closed(handle);
            central.done(ENOTCONN);
            drop(unsafe { Arc::from_raw(arg as *const Central) }); // its last event
        }
        sys::BLE_GAP_EVENT_NOTIFY_RX => {
            let rx = unsafe { event.__bindgen_anon_1.notify_rx };
            if rx.attr_handle == central.found().tx {
                if let Some(value) = unsafe { flat(rx.om) } {
                    central.shared.lock().notified(rx.conn_handle, value);
                }
            }
        }
        sys::BLE_GAP_EVENT_MTU => {
            let mtu = unsafe { event.__bindgen_anon_1.mtu };
            central.shared.lock().mtu(mtu.conn_handle, mtu.value);
        }
        sys::BLE_GAP_EVENT_ENC_CHANGE => {
            warn!("ble: a peripheral paired, closing the connection (nothing is paired)");
            terminate(unsafe { event.__bindgen_anon_1.enc_change.conn_handle });
        }
        _ => {}
    }
    0
}

/// Each connection coming and going as NimBLE has it, in either role, from NimBLE's task, for the log: before the
/// connection's own callback. Its `arg` is the driver's `Shared`.
unsafe extern "C" fn on_link(event: *mut sys::ble_gap_event, arg: *mut c_void) -> i32 {
    let shared = unsafe { &*(arg as *const Shared) };
    let event = unsafe { &*event };
    match u32::from(event.type_) {
        sys::BLE_GAP_EVENT_CONNECT => {
            let connect = unsafe { event.__bindgen_anon_1.connect };
            let handle = connect.conn_handle;
            let mut desc: sys::ble_gap_conn_desc = unsafe { std::mem::zeroed() };
            match connect.status {
                0 if unsafe { sys::ble_gap_conn_find(handle, &mut desc) } == 0 => {
                    info!("ble: link {handle} up, as the {}", role(desc.role));
                }
                0 => info!("ble: link {handle} up"),
                status => info!("ble: a link didn't come up ({status})"),
            }
        }
        sys::BLE_GAP_EVENT_DISCONNECT => {
            let disconnect = unsafe { event.__bindgen_anon_1.disconnect };
            let (handle, role, why) =
                (disconnect.conn.conn_handle, role(disconnect.conn.role), Reason(disconnect.reason));
            let conn = shared.lock().links.get(&handle).filter(|link| link.up).map(|link| link.conn.0);
            match conn {
                Some(conn) => info!("ble: link {handle} (conn {conn}) closed, as the {role}: {why}"),
                None => info!("ble: link {handle} closed, as the {role}, not one the node had: {why}"),
            }
        }
        _ => {}
    }
    0
}

fn role(role: u8) -> &'static str {
    match u32::from(role) {
        sys::BLE_GAP_ROLE_MASTER => "central",
        _ => "peripheral",
    }
}

/// A disconnect's reason as NimBLE gives it: an HCI reason plus [`HCI_BASE`], or else the host's own error.
struct Reason(i32);

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Some(hci) = self.0.checked_sub(HCI_BASE).filter(|hci| (0..0x100).contains(hci)) else {
            return write!(f, "host error {}", self.0);
        };
        let what = match hci {
            0x08 => ", timed out",
            0x13 => ", the peer closed it",
            0x16 => ", this radio closed it",
            0x3e => ", never established",
            _ => "",
        };
        write!(f, "HCI 0x{hci:02x}{what}")
    }
}

/// The ATT minimum, until the exchange says otherwise.
const MIN_MTU: u16 = peerwire::ble::MIN_ATT_MTU;

unsafe extern "C" fn on_mtu(_: u16, error: *const sys::ble_gatt_error, _: u16, arg: *mut c_void) -> i32 {
    unsafe { central(arg).done((*error).status) };
    0
}

unsafe extern "C" fn on_service(
    _: u16,
    error: *const sys::ble_gatt_error,
    service: *const sys::ble_gatt_svc,
    arg: *mut c_void,
) -> i32 {
    let central = unsafe { central(arg) };
    match unsafe { (*error).status } {
        0 => {
            let service = unsafe { &*service };
            central.found().service.get_or_insert((service.start_handle, service.end_handle));
        }
        status => central.done(if status == EDONE { 0 } else { status }),
    }
    0
}

unsafe extern "C" fn on_chr(
    _: u16,
    error: *const sys::ble_gatt_error,
    chr: *const sys::ble_gatt_chr,
    arg: *mut c_void,
) -> i32 {
    let central = unsafe { central(arg) };
    match unsafe { (*error).status } {
        0 => {
            let chr = unsafe { &*chr };
            let uuid = unsafe { uuid_of(&chr.uuid) };
            central.found().chrs.push((chr.def_handle, chr.val_handle, uuid));
        }
        status => central.done(if status == EDONE { 0 } else { status }),
    }
    0
}

unsafe extern "C" fn on_dsc(
    _: u16,
    error: *const sys::ble_gatt_error,
    _: u16,
    dsc: *const sys::ble_gatt_dsc,
    arg: *mut c_void,
) -> i32 {
    let central = unsafe { central(arg) };
    match unsafe { (*error).status } {
        0 => {
            let dsc = unsafe { &*dsc };
            let is_cccd = unsafe { dsc.uuid.u.type_ == sys::BLE_UUID_TYPE_16 as u8 && dsc.uuid.u16_.value == CCCD };
            if is_cccd {
                central.found().cccd.get_or_insert(dsc.handle);
            }
        }
        status => central.done(if status == EDONE { 0 } else { status }),
    }
    0
}

unsafe extern "C" fn on_write(
    _: u16,
    error: *const sys::ble_gatt_error,
    _: *mut sys::ble_gatt_attr,
    arg: *mut c_void,
) -> i32 {
    unsafe { central(arg).done((*error).status) };
    0
}

/// A GATT procedure's `arg`: the connection's `Central`, alive while the connection is.
unsafe fn central<'a>(arg: *mut c_void) -> &'a Central {
    unsafe { &*(arg as *const Central) }
}

/// Adverts heard while scanning, from NimBLE's task: the service's, with their service data, go to the node.
unsafe extern "C" fn on_scan(event: *mut sys::ble_gap_event, arg: *mut c_void) -> i32 {
    let shared = unsafe { &*(arg as *const Shared) };
    let event = unsafe { &*event };
    match u32::from(event.type_) {
        sys::BLE_GAP_EVENT_DISC => {
            let desc = unsafe { &event.__bindgen_anon_1.disc };
            let ad = unsafe { std::slice::from_raw_parts(desc.data, desc.length_data.into()) };
            if let Some(data) = service_data(ad) {
                shared.lock().push(RadioEvent::Seen { device: device_id(&desc.addr), data: data.to_vec() });
            }
        }
        // NimBLE ended the scan itself (a cancel reports nothing).
        sys::BLE_GAP_EVENT_DISC_COMPLETE => shared.send(Cmd::Rescan),
        _ => {}
    }
    0
}

/// The service data under the Peerwire service's UUID in an advert, if there is any.
fn service_data(mut ad: &[u8]) -> Option<&[u8]> {
    let uuid = SERVICE_UUID.to_le_bytes();
    while let [len, rest @ ..] = ad {
        let len = usize::from(*len);
        if len == 0 || len > rest.len() {
            return None;
        }
        let (field, next) = rest.split_at(len);
        if field[0] == AD_SERVICE_DATA_128 && field.get(1..17) == Some(&uuid[..]) {
            return Some(&field[17..]);
        }
        ad = next;
    }
    None
}

/// The advertising data: the flags, and `data` as service data under the service's UUID.
fn advert(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    let len = u8::try_from(1 + 16 + data.len()).ok().filter(|&len| len <= 31 - 3 - 1);
    let len = len.ok_or_else(|| anyhow!("{} B of service data don't fit an advert", data.len()))?;
    let mut ad = vec![2, AD_FLAGS, FLAGS, len, AD_SERVICE_DATA_128];
    ad.extend_from_slice(&SERVICE_UUID.to_le_bytes());
    ad.extend_from_slice(data);
    Ok(ad)
}

/// The scan response: the service's UUID, for Web Bluetooth's `services` filter.
const SCAN_RESPONSE: [u8; 18] = {
    let mut r = [0; 18];
    (r[0], r[1]) = (17, AD_UUIDS_128);
    let uuid = SERVICE_UUID.to_le_bytes();
    let mut i = 0;
    while i < 16 {
        r[2 + i] = uuid[i];
        i += 1;
    }
    r
};

/// A fresh non-resolvable random address for everything this radio does from now on (BLE-R3).
fn new_address() -> anyhow::Result<()> {
    let mut addr = sys::ble_addr_t::default();
    let rc = unsafe { sys::ble_hs_id_gen_rnd(1, &mut addr) };
    ensure!(rc == 0, "ble_hs_id_gen_rnd: {rc}");
    let rc = unsafe { sys::ble_hs_id_set_rnd(addr.val.as_ptr()) };
    ensure!(rc == 0, "ble_hs_id_set_rnd: {rc}");
    Ok(())
}

fn stop_scan() {
    if unsafe { sys::ble_gap_disc_active() } != 0 {
        unsafe { sys::ble_gap_disc_cancel() };
    }
}

fn terminate(handle: u16) {
    unsafe { sys::ble_gap_terminate(handle, TERMINATED) };
}

/// `tx`'s value handle, once the server has started.
fn tx_handle() -> anyhow::Result<u16> {
    let (service, tx) = (uuid128(SERVICE_UUID), uuid128(TX_UUID));
    let mut handle = 0;
    let rc = unsafe { sys::ble_gatts_find_chr(&service.u, &tx.u, ptr::null_mut(), &mut handle) };
    ensure!(rc == 0, "ble_gatts_find_chr: {rc}");
    Ok(handle)
}

fn uuid(uuid: u128) -> BleUuid {
    BleUuid::from_uuid128(uuid.to_le_bytes())
}

fn uuid128(uuid: u128) -> sys::ble_uuid128_t {
    sys::ble_uuid128_t { u: sys::ble_uuid_t { type_: sys::BLE_UUID_TYPE_128 as u8 }, value: uuid.to_le_bytes() }
}

/// A 128-bit UUID, as a number; `None` for a shorter one.
unsafe fn uuid_of(uuid: &sys::ble_uuid_any_t) -> Option<u128> {
    unsafe { (uuid.u.type_ == sys::BLE_UUID_TYPE_128 as u8).then(|| u128::from_le_bytes(uuid.u128_.value)) }
}

/// A notified value, copied out of NimBLE's buffer (which it frees).
unsafe fn flat(om: *const sys::os_mbuf) -> Option<Vec<u8>> {
    let mut buf = [0u8; MAX_VALUE];
    let mut len = 0;
    let rc = unsafe { sys::ble_hs_mbuf_to_flat(om, buf.as_mut_ptr().cast(), buf.len() as u16, &mut len) };
    (rc == 0).then(|| buf[..usize::from(len)].to_vec())
}

/// A device by its address and its address's type.
fn device_id(addr: &sys::ble_addr_t) -> DeviceId {
    let mut id = [0u8; 8];
    id[..6].copy_from_slice(&addr.val);
    id[6] = addr.type_;
    DeviceId(u64::from_le_bytes(id))
}

fn ble_addr(device: DeviceId) -> sys::ble_addr_t {
    let id = device.0.to_le_bytes();
    let mut val = [0; 6];
    val.copy_from_slice(&id[..6]);
    sys::ble_addr_t { type_: id[6], val }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_service_data() {
        let ad = advert(&[1, 2, 3, 4, 5, 6, 7, 8, 9]).unwrap();
        assert_eq!(ad.len(), 30);
        assert_eq!(service_data(&ad), Some(&[1, 2, 3, 4, 5, 6, 7, 8, 9][..]));
        assert_eq!(service_data(&SCAN_RESPONSE), None);
        assert_eq!(service_data(&[3, 0x21, 1]), None);
        assert_eq!(service_data(&[0]), None);
        assert!(advert(&[0; 11]).is_err());
    }

    #[test]
    fn device_ids_round_trip() {
        let addr = sys::ble_addr_t { type_: 1, val: [1, 2, 3, 4, 5, 0xc6] };
        let back = ble_addr(device_id(&addr));
        assert_eq!((back.type_, back.val), (addr.type_, addr.val));
    }
}

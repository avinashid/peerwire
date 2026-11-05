//! The Linux radio driver (feature `ble-bluez`): BlueZ over D-Bus through `bluer`, on a thread of its own with a
//! one-thread tokio runtime, behind the non-blocking [`Radio`] the node polls.
//!
//! As the peripheral it takes one central at a time (BlueZ sends a characteristic's notifications to every subscribed
//! central, BLE-R8): a central counts as connected once it subscribes to `tx`, and only its plain writes without
//! response at offset 0 reach the node (BLE-R7). As the central it connects, finds the service and takes the `rx`
//! writer and `tx` notifications as sockets. Nothing is paired. BlueZ picks the addresses it advertises and connects
//! from, and its GAP name is the adapter's alias: see [`BlueZ::alias`] and `docs/concepts/bluetooth.md`.

use crate::ble::{ConnId, DeviceId, Limits, Radio, RadioEvent, RX_UUID, SERVICE_UUID, TX_UUID};
use bluer::adv::{Advertisement, AdvertisementHandle, Type};
use bluer::gatt::local::{
    Application, ApplicationHandle, Characteristic, CharacteristicNotifier, CharacteristicNotify,
    CharacteristicNotifyMethod, CharacteristicWrite, CharacteristicWriteMethod, CharacteristicWriteRequest, ReqError,
    Service,
};
use bluer::gatt::WriteOp;
use bluer::{Adapter, AdapterEvent, Address, DeviceEvent, DeviceProperty, DiscoveryFilter, DiscoveryTransport, Uuid};
use futures::{FutureExt, StreamExt};
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::task::Waker;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Values queued per connection in the driver; beyond that, [`Radio::send`] says no and the node keeps them.
const QUEUE: usize = 32;
/// How long [`BlueZ::new`] waits for the adapter.
const START: Duration = Duration::from_secs(10);

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
    /// or this process may not use it over D-Bus.
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

fn device_id(a: Address) -> DeviceId {
    let mut b = [0u8; 8];
    b[2..].copy_from_slice(&a.0);
    DeviceId(u64::from_be_bytes(b))
}

fn address(d: DeviceId) -> Address {
    let b = d.0.to_be_bytes();
    Address::new([b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// Each central connection's tasks and the device it is to, so it can be closed.
type Centrals = Arc<Mutex<HashMap<u32, (Vec<JoinHandle<()>>, Address)>>>;

/// The one central this radio serves, if any.
#[derive(Default)]
struct Peripheral {
    conn: Option<u32>,
    /// Its address, from its first write.
    from: Option<Address>,
}

struct Driver {
    adapter: Adapter,
    events: Events,
    shared: Arc<Shared>,
    peripheral: Arc<Mutex<Peripheral>>,
    advert: Option<AdvertisementHandle>,
    scan: Option<JoinHandle<()>>,
    centrals: Centrals,
    _app: Option<ApplicationHandle>,
}

impl Driver {
    async fn run(
        mut cmds: mpsc::UnboundedReceiver<Cmd>,
        events: Events,
        shared: Arc<Shared>,
        ready: std_mpsc::Sender<io::Result<String>>,
    ) {
        let setup = async {
            let session = bluer::Session::new().await?;
            let adapter = session.default_adapter().await?;
            adapter.set_powered(true).await?;
            let alias = adapter.alias().await?;
            Ok::<_, bluer::Error>((session, adapter, alias))
        };
        let (_session, adapter, alias) = match setup.await {
            Ok(s) => s,
            Err(e) => return drop(ready.send(Err(io::Error::other(e)))),
        };
        let mut d = Driver {
            adapter,
            events,
            shared,
            peripheral: Arc::new(Mutex::new(Peripheral::default())),
            advert: None,
            scan: None,
            centrals: Arc::new(Mutex::new(HashMap::new())),
            _app: None,
        };
        match d.adapter.serve_gatt_application(d.application()).await {
            Ok(app) => d._app = Some(app),
            Err(e) => return drop(ready.send(Err(io::Error::other(e)))),
        }
        let _ = ready.send(Ok(alias));
        while let Some(cmd) = cmds.recv().await {
            match cmd {
                Cmd::Advertise(data) => d.advertise(data).await,
                Cmd::Scan(on) => d.scan(on).await,
                Cmd::Connect(device) => d.connect(device),
                Cmd::Disconnect(conn) => d.disconnect(conn).await,
            }
        }
    }

    /// The service: `rx` takes plain writes without response at offset 0 from the one central, `tx` notifies it.
    fn application(&self) -> Application {
        let (events, peripheral) = (self.events.clone(), self.peripheral.clone());
        let write: Box<dyn Fn(Vec<u8>, CharacteristicWriteRequest) -> _ + Send + Sync> =
            Box::new(move |value: Vec<u8>, req: CharacteristicWriteRequest| {
                let ok = req.op_type == WriteOp::Command && req.offset == 0;
                let mut p = peripheral.lock().unwrap();
                let taken = match (ok, p.conn) {
                    (true, Some(conn)) if p.from.is_none_or(|f| f == req.device_address) => {
                        if p.from.is_none() {
                            p.from = Some(req.device_address);
                            events.send(RadioEvent::Mtu { conn: ConnId(conn), mtu: req.mtu });
                        }
                        events.send(RadioEvent::Value { conn: ConnId(conn), value });
                        true
                    }
                    _ => false,
                };
                async move {
                    if taken {
                        Ok(())
                    } else {
                        Err(ReqError::NotPermitted)
                    }
                }
                .boxed()
            });
        let (events, peripheral, shared) = (self.events.clone(), self.peripheral.clone(), self.shared.clone());
        let notify: Box<dyn Fn(CharacteristicNotifier) -> _ + Send + Sync> =
            Box::new(move |notifier: CharacteristicNotifier| {
                let (events, peripheral, shared) = (events.clone(), peripheral.clone(), shared.clone());
                async move { serve_central(notifier, events, peripheral, shared).await }.boxed()
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
                method: CharacteristicNotifyMethod::Fun(notify),
                ..Default::default()
            }),
            ..Default::default()
        };
        let service =
            Service { uuid: uuid(SERVICE_UUID), primary: true, characteristics: vec![rx, tx], ..Default::default() };
        Application { services: vec![service], ..Default::default() }
    }

    async fn advertise(&mut self, data: Option<Vec<u8>>) {
        self.advert = None; // dropping the handle stops the old advert
        let Some(data) = data else { return };
        let adv = Advertisement {
            advertisement_type: Type::Peripheral,
            service_data: [(uuid(SERVICE_UUID), data)].into_iter().collect(),
            discoverable: Some(true),
            ..Default::default()
        };
        self.advert = self.adapter.advertise(adv).await.ok();
    }

    async fn scan(&mut self, on: bool) {
        if let Some(t) = self.scan.take() {
            t.abort();
        }
        if !on {
            return;
        }
        let filter = DiscoveryFilter {
            uuids: [uuid(SERVICE_UUID)].into_iter().collect(),
            transport: DiscoveryTransport::Le,
            duplicate_data: true,
            ..Default::default()
        };
        if self.adapter.set_discovery_filter(filter).await.is_err() {
            return;
        }
        let Ok(mut found) = self.adapter.discover_devices().await else { return };
        let (adapter, events) = (self.adapter.clone(), self.events.clone());
        self.scan = Some(tokio::spawn(async move {
            let mut watching: HashMap<Address, JoinHandle<()>> = HashMap::new();
            while let Some(e) = found.next().await {
                match e {
                    AdapterEvent::DeviceAdded(addr) => {
                        let Ok(dev) = adapter.device(addr) else { continue };
                        if let Ok(Some(data)) = dev.service_data().await {
                            seen(&events, addr, &data);
                        }
                        let Ok(mut changes) = dev.events().await else { continue };
                        let events = events.clone();
                        let watch = tokio::spawn(async move {
                            while let Some(DeviceEvent::PropertyChanged(p)) = changes.next().await {
                                if let DeviceProperty::ServiceData(data) = p {
                                    seen(&events, addr, &data);
                                }
                            }
                        });
                        if let Some(old) = watching.insert(addr, watch) {
                            old.abort();
                        }
                    }
                    AdapterEvent::DeviceRemoved(addr) => {
                        if let Some(w) = watching.remove(&addr) {
                            w.abort();
                        }
                    }
                    _ => {}
                }
            }
            watching.values().for_each(JoinHandle::abort);
        }));
    }

    /// Connects to `device` as a central, in a task of its own: the service's `tx` notifications as a socket to read,
    /// its `rx` as one to write without response.
    fn connect(&mut self, device: DeviceId) {
        let (adapter, events, shared, centrals) =
            (self.adapter.clone(), self.events.clone(), self.shared.clone(), self.centrals.clone());
        tokio::spawn(async move {
            let addr = address(device);
            let setup = async {
                let dev = adapter.device(addr)?;
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
                    let _ = dev.disconnect().await;
                    return Err(bluer::Error { kind: bluer::ErrorKind::ServicesUnresolved, message: String::new() });
                };
                let reader = tx.notify_io().await?;
                let writer = rx.write_io().await?;
                Ok::<_, bluer::Error>((dev, reader, writer))
            };
            let Ok((dev, reader, writer)) = setup.await else {
                events.send(RadioEvent::ConnectFailed { device });
                return;
            };
            let conn = shared.next.fetch_add(1, Ordering::Relaxed);
            let mtu = u16::try_from(writer.mtu().min(reader.mtu())).unwrap_or(u16::MAX);
            let (out, mut queue) = mpsc::channel::<Vec<u8>>(QUEUE);
            shared.senders.lock().unwrap().insert(conn, out);
            events.send(RadioEvent::Connected { conn: ConnId(conn), device: Some(device), mtu });
            let write = tokio::spawn(async move {
                while let Some(v) = queue.recv().await {
                    if writer.send(&v).await.is_err() {
                        break;
                    }
                }
            });
            let (ev, sh, ce) = (events.clone(), shared.clone(), centrals.clone());
            let read = tokio::spawn(async move {
                while let Ok(value) = reader.recv().await {
                    ev.send(RadioEvent::Value { conn: ConnId(conn), value });
                }
                sh.senders.lock().unwrap().remove(&conn);
                if let Some((tasks, _)) = ce.lock().unwrap().remove(&conn) {
                    tasks.iter().for_each(JoinHandle::abort);
                }
                let _ = dev.disconnect().await;
                ev.send(RadioEvent::Disconnected { conn: ConnId(conn) });
            });
            centrals.lock().unwrap().insert(conn, (vec![write, read], addr));
        });
    }

    async fn disconnect(&mut self, conn: ConnId) {
        let central = self.centrals.lock().unwrap().remove(&conn.0);
        if let Some((tasks, addr)) = central {
            tasks.iter().for_each(JoinHandle::abort);
            if let Ok(dev) = self.adapter.device(addr) {
                let _ = dev.disconnect().await;
            }
            return;
        }
        // The central of ours: its notify session ends with its link, which BlueZ can close given its address.
        let from = {
            let mut p = self.peripheral.lock().unwrap();
            match p.conn == Some(conn.0) {
                true => std::mem::take(&mut *p).from,
                false => None,
            }
        };
        if let Some(dev) = from.and_then(|a| self.adapter.device(a).ok()) {
            let _ = dev.disconnect().await;
        }
    }
}

/// Reports an advert's service data, if it has the service's.
fn seen(events: &Events, addr: Address, data: &HashMap<Uuid, Vec<u8>>) {
    if let Some(d) = data.get(&uuid(SERVICE_UUID)) {
        events.send(RadioEvent::Seen { device: device_id(addr), data: d.clone() });
    }
}

/// Serves a central that subscribed to `tx`, the only one at a time: its values go out as notifications until it
/// stops the session or the node closes it.
async fn serve_central(
    mut notifier: CharacteristicNotifier,
    events: Events,
    peripheral: Arc<Mutex<Peripheral>>,
    shared: Arc<Shared>,
) {
    let conn = {
        let mut p = peripheral.lock().unwrap();
        if p.conn.is_some() {
            return; // another central's: one at a time (BLE-R8)
        }
        let conn = shared.next.fetch_add(1, Ordering::Relaxed);
        *p = Peripheral { conn: Some(conn), from: None };
        conn
    };
    let (out, mut queue) = mpsc::channel::<Vec<u8>>(QUEUE);
    shared.senders.lock().unwrap().insert(conn, out);
    events.send(RadioEvent::Connected { conn: ConnId(conn), device: None, mtu: crate::ble::MIN_ATT_MTU });
    loop {
        tokio::select! {
            v = queue.recv() => {
                let Some(v) = v else { break };
                if notifier.notify(v).await.is_err() {
                    break;
                }
            }
            _ = notifier.stopped() => break,
        }
    }
    shared.senders.lock().unwrap().remove(&conn);
    let mut p = peripheral.lock().unwrap();
    if p.conn == Some(conn) {
        *p = Peripheral::default();
        events.send(RadioEvent::Disconnected { conn: ConnId(conn) });
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

    #[test]
    fn an_alias_names_the_host_whatever_its_case() {
        assert!(same_name("laptop", "laptop\n"));
        assert!(same_name("Laptop", "laptop"));
        assert!(!same_name("Peerwire", "laptop"));
        assert!(!same_name("", ""), "no host name to name");
    }
}

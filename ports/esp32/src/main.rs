//! Peerwire on an ESP32-C3 or ESP32: a feasibility prototype (NB-062, NB-064, NB-091), not a product.
//!
//! In QEMU, each node reads its number from the last flash sector (run.sh patches it in)
//! and takes the address 10.62.0.<10+n> on QEMU's emulated Ethernet. On a board, wifi.sh
//! writes Wi-Fi credentials into that sector instead: the chip joins the network, takes an
//! address by DHCP and joins the `lan_chat` example's room "esp32", so a PC on the same
//! Wi-Fi running `lan_chat esp32` links with it. On Wi-Fi it joins that room over a Nostr
//! relay too (NB-095), so `room_chat esp32` anywhere on the internet links with it as well.
//! Either way the node generates a fresh identity and finds the others by itself: it joins
//! a room over `LanSignaling` (UDP multicast on the LAN), so N nodes make a full mesh with
//! no node knowing another's key or address beforehand. Linked nodes send each other a numbered
//! reliable message every second, a short line or (run.sh's MSG_KIB) padded to that
//! many KiB. It logs free heap at each step, when each peer linked after the join,
//! and every 10 s the heap, stack, link RTTs and signals. Peers are named by the node number
//! in their address (QEMU) or by their address (Wi-Fi).

use std::any::Any;
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::anyhow;
use esp_idf_svc::eth::{BlockingEth, EspEth, EthDriver};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::ipv4::{ClientConfiguration, ClientSettings, Configuration, Mask, Subnet};
use esp_idf_svc::netif::{EspNetif, NetifConfiguration};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sntp::EspSntp;
use esp_idf_svc::sys;
use esp_idf_svc::wifi::{self, AuthMethod, BlockingWifi, EspWifi, WifiEvent};
use log::{info, warn};
use peerwire::{
    Config, Event, Identity, LanSignaling, Lane, MultiSignaling, Node, NostrSignaling, Path, PeerId, Signaling, Topic,
};

const PORT: u16 = 4040;
const MAX_NODES: u8 = 16;
/// Where run.sh writes b"PWN", the node number, the message size in KiB and a 32-byte
/// identity seed, and wifi.sh writes the Wi-Fi record (at +0x40): the last sector of a
/// 4 MB flash, past the end of the app image.
const NODE_CFG_ADDR: u32 = 0x3F_F000;
const WIFI_CFG_OFFSET: usize = 0x40;
/// On Wi-Fi: the room `lan_chat esp32` joins (examples/rust/lan_chat.rs).
const WIFI_ROOM: (&str, &str) = ("peerwire-lan-chat", "esp32");
const QEMU_ROOM: (&str, &str) = ("peerwire-esp32", "lab");
/// On Wi-Fi: one of `NostrSignaling`'s default relays, so PCs on the defaults share it.
/// One relay, as each costs the chip a thread and a TLS session.
const NOSTR_RELAY: &str = "wss://nostr.mom";

/// What the last flash sector says.
struct NodeConfig {
    n: u8,
    msg_kib: u8,
    seed: Option<[u8; 32]>,
    /// The SSID and password: a board on Wi-Fi rather than QEMU's Ethernet.
    wifi: Option<(String, String)>,
}

fn main() -> anyhow::Result<()> {
    sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let NodeConfig { n, msg_kib, seed, wifi } = node_config();
    info!("node {n}: boot, free heap {} B", free_heap());

    let peripherals = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;
    // The driver lives as long as the node: dropping it takes the network down.
    let (ip, prefix, _net): (Ipv4Addr, u8, Box<dyn Any>) = match &wifi {
        Some((ssid, password)) => {
            let w = join_wifi(n, peripherals.modem, sysloop, ssid, password)?;
            let info = w.wifi().sta_netif().get_ip_info()?;
            (info.ip, info.subnet.mask.0, Box::new(w))
        }
        None => {
            let netif = EspNetif::new_with_conf(&NetifConfiguration {
                ip_configuration: Some(Configuration::Client(ClientConfiguration::Fixed(ClientSettings {
                    ip: addr(n),
                    subnet: Subnet { gateway: Ipv4Addr::new(10, 62, 0, 1), mask: Mask(24) },
                    dns: None,
                    secondary_dns: None,
                }))),
                ..NetifConfiguration::eth_default_client()
            })?;
            let driver = EthDriver::new_openeth(peripherals.mac, sysloop.clone())?;
            let mut eth = BlockingEth::wrap(EspEth::wrap_all(driver, netif)?, sysloop)?;
            eth.start()?;
            // The driver programs no multicast filter and QEMU's OpenCores MAC drops multicast
            // frames that miss it, so take every frame (QEMU only: a board uses Wi-Fi).
            eth.eth_mut().driver_mut().set_promiscuous(true)?;
            eth.wait_netif_up()?;
            (addr(n), 24, Box::new(eth))
        }
    };
    let heap_net = free_heap();
    info!("node {n}: network up at {ip}, free heap {heap_net} B");
    // Kept, so SNTP keeps the clock in step.
    let _sntp = if wifi.is_some() { set_clock(n) } else { None };

    // Its own address, not 0.0.0.0: ESP-IDF can't list interfaces, so a node bound to
    // any interface would offer only loopback.
    let mut builder = Node::builder();
    match seed {
        // Fresh host randomness from run.sh. QEMU's RNG gives every chip the same
        // sequence, so a generated identity would be the same on all of them (NB-063:
        // without Wi-Fi up the ESP-IDF RNG is only pseudo-random). The seed fixes the
        // identity only; the Node's other random draws still repeat on every chip, so
        // the emulator shows discovery and memory, not privacy.
        Some(seed) => builder = builder.identity(Identity::from_secret_bytes(&seed)),
        // With the radio on, the ESP-IDF RNG draws on RF noise (NB-063, NB62-2).
        None if wifi.is_some() => info!("node {n}: identity from the chip's RNG, Wi-Fi on"),
        None => warn!("node {n}: no identity seed in flash; generating one from the chip's RNG"),
    }
    let (lan, nostr) = (Arc::new(Counts::default()), Arc::new(Counts::default()));
    let on_lan = Counted { n, what: "LAN", inner: lan_signaling(n, ip, prefix)?, counts: lan.clone() };
    builder = match wifi {
        // The relay's TLS and STUN reach the internet: a board only. STUN puts the
        // router's public address in our offers, for peers beyond the LAN.
        Some(_) => {
            info!("node {n}: signals on the LAN and over {NOSTR_RELAY}");
            let on_nostr =
                Counted { n, what: "Nostr", inner: NostrSignaling::new([NOSTR_RELAY]), counts: nostr.clone() };
            builder.signaling(MultiSignaling::new(vec![Box::new(on_lan), Box::new(on_nostr)]))
        }
        None => builder.stun_servers(Vec::<String>::new()).signaling(on_lan),
    };
    let mut node = builder.config(config()).bind(SocketAddr::from((ip, PORT)))?;
    let heap_node = free_heap();
    let (app, room) = if wifi.is_some() { WIFI_ROOM } else { QEMU_ROOM };
    info!(
        "node {n}: bound {} as {}, the Node took {} B, free heap {heap_node} B; room {room:?} of {app:?}",
        node.local_addr(),
        node.peer_id(),
        heap_net - heap_node
    );
    node.join_room(app, room);
    let joined = Instant::now();

    // Peers by the address they linked from.
    let mut names: HashMap<PeerId, String> = HashMap::new();

    let mut seq = 0u32;
    let mut received = 0u64;
    let mut next_send = Instant::now() + Duration::from_secs(1);
    let mut next_report = Instant::now() + Duration::from_secs(10);
    loop {
        node.poll();
        while let Some(e) = node.try_recv_event() {
            match e {
                Event::Connected { peer, initiator, path } => {
                    if let Some(m) = name_of(&path) {
                        names.insert(peer, m);
                    }
                    let how = if initiator { "dialled" } else { "answered" };
                    let links = node.peers().count();
                    info!(
                        "node {n}: linked to {} ({how}, {} ms after joining), {links} links, free heap {} B",
                        name(&names, &peer),
                        joined.elapsed().as_millis(),
                        free_heap()
                    );
                }
                Event::Message { peer, data, .. } => {
                    received += 1;
                    if received <= 8 || received % 50 == 0 {
                        let text =
                            String::from_utf8_lossy(&data[..data.len().min(24)]).trim_end_matches('.').to_string();
                        info!(
                            "node {n}: from {}: {text}, {} B ({received} received)",
                            name(&names, &peer),
                            data.len()
                        );
                    }
                }
                Event::Disconnected { peer, reason } => warn!("node {n}: {} left: {reason:?}", name(&names, &peer)),
                Event::ConnectFailed { peer } => warn!("node {n}: dial to {} failed", name(&names, &peer)),
                _ => {}
            }
        }
        let now = Instant::now();
        if now >= next_send {
            next_send = now + Duration::from_secs(1);
            seq += 1;
            let mut msg = format!("#{seq} from node {n}").into_bytes();
            if msg_kib > 0 {
                msg.resize(msg_kib as usize * 1024, b'.');
            }
            for peer in node.peers().copied().collect::<Vec<_>>() {
                if let Err(e) = node.send(&peer, Lane::Reliable, &msg) {
                    warn!("node {n}: send to {}: {e:?}", name(&names, &peer));
                }
            }
        }
        if now >= next_report {
            next_report = now + Duration::from_secs(10);
            report(n, &node, heap_node, &names, [&lan, &nostr], wifi.is_some());
        }
        std::thread::sleep(node.next_timeout().unwrap_or(Duration::from_millis(10)).min(Duration::from_millis(10)));
    }
}

/// Free heap, the lowest it has been, the main task's unused stack, each link's RTT, the
/// signals sent and heard on the LAN and over Nostr and, on Wi-Fi, the signal strength.
fn report(
    n: u8,
    node: &Node,
    heap_node: u32,
    names: &HashMap<PeerId, String>,
    [lan, nostr]: [&Counts; 2],
    on_wifi: bool,
) {
    let free = free_heap();
    let min = unsafe { sys::esp_get_minimum_free_heap_size() };
    let stack_left = unsafe { sys::uxTaskGetStackHighWaterMark(std::ptr::null_mut()) };
    let links = node.peers().count();
    let per_link = if links > 0 { (heap_node.saturating_sub(free) as usize / links).to_string() } else { "-".into() };
    let rtts: Vec<String> = node
        .peers()
        .map(|p| {
            let rtt = node.peer_stats(p).and_then(|s| s.rtt).map_or("?".into(), |d| format!("{} ms", d.as_millis()));
            format!("{}: {rtt}", name(names, p))
        })
        .collect();
    let signal = if on_wifi {
        let mut ap: sys::wifi_ap_record_t = unsafe { std::mem::zeroed() };
        match unsafe { sys::esp_wifi_sta_get_ap_info(&mut ap) } {
            sys::ESP_OK => format!(", Wi-Fi {} dBm", ap.rssi),
            _ => ", Wi-Fi not connected".into(),
        }
    } else {
        String::new()
    };
    info!(
        "node {n}: report: {links} links, free heap {free} B (min {min} B), heap per link {per_link} B, \
         stack unused {stack_left} B, rtt [{}], LAN signals sent {} heard {}, Nostr sent {} heard {} relays open {}{signal}",
        rtts.join(", "),
        lan.sent.load(Ordering::Relaxed),
        lan.heard.load(Ordering::Relaxed),
        nostr.sent.load(Ordering::Relaxed),
        nostr.heard.load(Ordering::Relaxed),
        nostr.open.load(Ordering::Relaxed)
    );
}

/// What a carrier sent, and heard from other nodes on this node's topics (their hellos
/// and offers, counted before the Node checks them). On a real network discovery can work
/// one way only, such as when the router doesn't deliver multicast to the board (NB-091).
#[derive(Default)]
struct Counts {
    sent: AtomicU32,
    heard: AtomicU32,
    /// Nostr: the relays connected now.
    open: AtomicU32,
}

/// What a carrier can tell about itself: Nostr, whether its relays are connected (a
/// relay that fails to connect only retries, NB-095).
trait Health {
    fn relays_open(&self) -> Option<u64> {
        None
    }
}

impl Health for LanSignaling {}

impl Health for NostrSignaling {
    fn relays_open(&self) -> Option<u64> {
        Some(self.stats().relays_open)
    }
}

/// A carrier, counting into `counts` and logging the first signals heard.
struct Counted<S> {
    n: u8,
    what: &'static str,
    inner: S,
    counts: Arc<Counts>,
}

impl<S: Signaling + Health> Signaling for Counted<S> {
    fn subscribe(&mut self, topic: Topic) {
        self.inner.subscribe(topic);
    }

    fn unsubscribe(&mut self, topic: Topic) {
        self.inner.unsubscribe(topic);
    }

    fn publish(&mut self, topic: Topic, payload: &[u8]) {
        self.counts.sent.fetch_add(1, Ordering::Relaxed);
        self.inner.publish(topic, payload);
    }

    fn poll(&mut self) -> Option<(Topic, Vec<u8>)> {
        if let Some(open) = self.inner.relays_open() {
            self.counts.open.store(open as u32, Ordering::Relaxed);
        }
        let m = self.inner.poll()?;
        let heard = self.counts.heard.fetch_add(1, Ordering::Relaxed) + 1;
        if heard <= 4 {
            info!("node {}: heard {} signal {heard}, {} B", self.n, self.what, m.1.len());
        }
        Some(m)
    }
}

/// The LAN carrier on this node's address, and on its subnet's broadcast address too
/// (NB-092): a home router in NB-091 never delivered multicast to the board, but passed
/// broadcast both ways. ESP-IDF can't list interfaces, so `LanSignaling` can't find it.
fn lan_signaling(n: u8, ip: Ipv4Addr, prefix: u8) -> std::io::Result<LanSignaling> {
    let lan = LanSignaling::on(LanSignaling::GROUP, ip)?;
    // A /31 or /32 has no broadcast address: below, it would give a host's address, the
    // chip's own on a /32, which `broadcast` refuses (NB92-1).
    if prefix >= 31 {
        info!("node {n}: LAN signals to {} only, on a /{prefix}", LanSignaling::GROUP.ip());
        return Ok(lan);
    }
    let broadcast = Ipv4Addr::from(u32::from(ip) | u32::MAX >> prefix);
    info!("node {n}: LAN signals to {} and {broadcast}", LanSignaling::GROUP.ip());
    lan.broadcast(broadcast)
}

/// Sets the clock by SNTP, waiting up to 20 s. Room hellos and offers carry the sender's
/// clock, and a node drops those more than a minute off its own (a replay guard). A board
/// has no battery-backed clock: at 1970 it and a PC on the LAN dropped each other's hellos
/// and never linked (NB-091). The Node reads the clock when it is bound, so this comes
/// first. In QEMU every chip's clock starts at 1970, so they agree without it.
fn set_clock(n: u8) -> Option<EspSntp<'static>> {
    let unix_s = || SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let sntp = EspSntp::new_default().map_err(|e| warn!("node {n}: SNTP: {e}")).ok()?;
    let start = Instant::now();
    // 2023-11-14: any clock before it was never set.
    while unix_s() < 1_700_000_000 && start.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(100));
    }
    match unix_s() {
        s if s >= 1_700_000_000 => info!("node {n}: clock set by SNTP in {} ms: {s} s", start.elapsed().as_millis()),
        s => warn!("node {n}: no SNTP answer in 20 s, clock at {s} s: LAN peers with a set clock drop our hellos"),
    }
    Some(sntp)
}

fn free_heap() -> u32 {
    unsafe { sys::esp_get_free_heap_size() }
}

/// Joins the Wi-Fi network and waits for a DHCP address, retrying until it does. Each
/// attempt steps the TX power down (ESP-IDF's quarter-dBm, 20 to 8.5 dBm): small C3
/// boards with a chip antenna often can't take full power, so the AP hears their short
/// auth frame but never their association request.
fn join_wifi(
    n: u8,
    modem: Modem<'static>,
    sysloop: EspSystemEventLoop,
    ssid: &str,
    password: &str,
) -> anyhow::Result<BlockingWifi<EspWifi<'static>>> {
    const TX_POWER: [i8; 4] = [80, 60, 44, 34];
    let nvs = EspDefaultNvsPartition::take()?;
    let mut w = BlockingWifi::wrap(EspWifi::new(modem, sysloop.clone(), Some(nvs))?, sysloop.clone())?;
    w.set_configuration(&wifi::Configuration::Client(wifi::ClientConfiguration {
        ssid: ssid.try_into().map_err(|_| anyhow!("SSID over 32 bytes"))?,
        password: password.try_into().map_err(|_| anyhow!("password over 64 bytes"))?,
        auth_method: if password.is_empty() { AuthMethod::None } else { AuthMethod::WPA2Personal },
        ..Default::default()
    }))?;
    w.start()?;
    // Modem sleep holds our frames until the next beacon, ~100 ms on every RTT.
    sys::esp!(unsafe { sys::esp_wifi_set_ps(sys::wifi_ps_type_t_WIFI_PS_NONE) })?;
    let aps = w.scan()?;
    let seen: Vec<_> = aps.iter().filter(|ap| ap.ssid == ssid).collect();
    if seen.is_empty() {
        warn!("node {n}: {ssid:?} not among the {} networks in range", aps.len());
    }
    for ap in seen {
        let bssid: Vec<_> = ap.bssid.iter().map(|b| format!("{b:02x}")).collect();
        info!(
            "node {n}: hears {ssid:?} at {} on channel {}, {} dBm, {:?}",
            bssid.join(":"),
            ap.channel,
            ap.signal_strength,
            ap.auth_method
        );
    }
    // The last disconnect's reason (ESP-IDF's wifi_err_reason_t); 0 while there is none.
    let reason = Arc::new(AtomicU16::new(0));
    let _sub = sysloop.subscribe::<WifiEvent, _>({
        let reason = reason.clone();
        move |event| {
            if let WifiEvent::StaDisconnected(d) = event {
                reason.store(d.reason(), Ordering::Relaxed);
            }
        }
    })?;
    for attempt in 1.. {
        let power = TX_POWER[(attempt - 1) % TX_POWER.len()];
        sys::esp!(unsafe { sys::esp_wifi_set_max_tx_power(power) })?;
        info!("node {n}: joining Wi-Fi {ssid:?}, attempt {attempt} at {} dBm", f32::from(power) / 4.0);
        reason.store(0, Ordering::Relaxed);
        w.wifi_mut().connect()?;
        let start = Instant::now();
        while !w.is_connected()? && reason.load(Ordering::Relaxed) == 0 && start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(50));
        }
        if w.is_connected()? {
            info!("node {n}: joined on attempt {attempt} at {} dBm", f32::from(power) / 4.0);
            break;
        }
        let r = reason.load(Ordering::Relaxed);
        let why = match r {
            0 => "no answer in 10 s",
            2 => "auth expired",
            4 => "the AP never answered the association request",
            15 | 204 => "4-way handshake timed out: wrong password?",
            201 => "AP not found",
            202 => "auth failed",
            203 => "association refused",
            _ => "see ESP-IDF's wifi_err_reason_t",
        };
        warn!("node {n}: attempt {attempt} failed, reason {r}: {why}");
        let _ = w.wifi_mut().disconnect();
        std::thread::sleep(Duration::from_millis(500));
    }
    w.wait_netif_up()?;
    Ok(w)
}

/// This node's number, the size of its messages in KiB (0: a short line), its identity
/// seed and Wi-Fi credentials from flash; node 1 with short lines, no seed and QEMU's
/// Ethernet for whatever wasn't written.
fn node_config() -> NodeConfig {
    let mut b = [0u8; 192];
    let r = unsafe { sys::esp_flash_read(std::ptr::null_mut(), b.as_mut_ptr().cast(), NODE_CFG_ADDR, b.len() as u32) };
    if r != sys::ESP_OK {
        return NodeConfig { n: 1, msg_kib: 0, seed: None, wifi: None };
    }
    let (n, msg_kib, seed) = if b[..3] == *b"PWN" && (1..=MAX_NODES).contains(&b[3]) {
        let seed: [u8; 32] = b[5..37].try_into().unwrap();
        // Erased flash reads 0xff.
        let seed = (seed != [0xff; 32] && seed != [0; 32]).then_some(seed);
        (b[3], b[4].min((config().max_message / 1024) as u8), seed)
    } else {
        (1, 0, None)
    };
    NodeConfig { n, msg_kib, seed, wifi: wifi_config(&b[WIFI_CFG_OFFSET..]) }
}

/// wifi.sh's record: b"PWW", the SSID's length and the SSID, the password's length and
/// the password.
fn wifi_config(b: &[u8]) -> Option<(String, String)> {
    let (&len, rest) = b.strip_prefix(b"PWW")?.split_first()?;
    let ssid = rest.get(..len as usize).filter(|_| (1..=32).contains(&len))?;
    let (&len, rest) = rest[ssid.len()..].split_first()?;
    let password = rest.get(..len as usize).filter(|_| len <= 64)?;
    Some((String::from_utf8(ssid.to_vec()).ok()?, String::from_utf8(password.to_vec()).ok()?))
}

fn addr(n: u8) -> Ipv4Addr {
    Ipv4Addr::new(10, 62, 0, 10 + n)
}

/// A direct path's name: the node number in a QEMU address (10.62.0.<10+n>), else the
/// address itself.
fn name_of(path: &Path) -> Option<String> {
    let Path::Direct(a) = path else { return None };
    if let SocketAddr::V4(v4) = a {
        let [a0, a1, a2, last] = v4.ip().octets();
        if let Some(m) = last.checked_sub(10).filter(|_| [a0, a1, a2] == [10, 62, 0]) {
            return Some(format!("node {m}"));
        }
    }
    Some(a.to_string())
}

/// `peer`'s name, or "node 0" if it hasn't linked directly yet.
fn name(names: &HashMap<PeerId, String>, peer: &PeerId) -> String {
    names.get(peer).cloned().unwrap_or_else(|| "node 0".into())
}

/// Caps sized for a chip with ~300 KB of heap; the defaults allow 1 MiB messages and buffers.
fn config() -> Config {
    Config {
        max_message: 16 * 1024,
        max_send_buffer: 32 * 1024,
        max_links: 8,
        max_pending_handshakes: 8,
        ..Config::default()
    }
}

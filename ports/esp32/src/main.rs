//! Peerwire on an ESP32-C3: a feasibility prototype (NB-062, NB-064), not a product.
//!
//! Each node reads its number from the last flash sector (run.sh patches it in) and
//! takes the address 10.62.0.<10+n> on QEMU's emulated Ethernet. It generates a fresh
//! identity and finds the others by itself: it joins a room over `LanSignaling`
//! (UDP multicast on the LAN), so N nodes make a full mesh with no node knowing
//! another's key or address beforehand. Linked nodes send each other a numbered
//! reliable message every second, a short line or (run.sh's MSG_KIB) padded to that
//! many KiB. It logs free heap at each step, when each peer linked after the join,
//! and every 10 s the heap, stack and link RTTs. Peers are named by the node number
//! in their address.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use esp_idf_svc::eth::{BlockingEth, EspEth, EthDriver};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::ipv4::{ClientConfiguration, ClientSettings, Configuration, Mask, Subnet};
use esp_idf_svc::netif::{EspNetif, NetifConfiguration};
use esp_idf_svc::sys;
use log::{info, warn};
use peerwire::{Config, Event, Identity, LanSignaling, Lane, Node, Path, PeerId};

const PORT: u16 = 4040;
const MAX_NODES: u8 = 16;
/// Where run.sh writes b"PWN", the node number, the message size in KiB and a 32-byte
/// identity seed: the last sector of a 4 MB flash, past the end of the app image.
const NODE_CFG_ADDR: u32 = 0x3F_F000;

fn main() -> anyhow::Result<()> {
    sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let (n, msg_kib, seed) = node_config();
    info!("node {n}: boot, free heap {} B", free_heap());

    let peripherals = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;
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
    // frames that miss it, so take every frame (QEMU only: a real C3 has Wi-Fi, no openeth).
    eth.eth_mut().driver_mut().set_promiscuous(true)?;
    eth.wait_netif_up()?;
    let heap_net = free_heap();
    info!("node {n}: network up at {}, free heap {heap_net} B", addr(n));

    // Its own address, not 0.0.0.0: ESP-IDF can't list interfaces, so a node bound to
    // any interface would offer only loopback.
    let mut builder = Node::builder();
    match seed {
        // Fresh host randomness from run.sh. QEMU's RNG gives every chip the same
        // sequence, so a generated identity would be the same on all of them (NB-063:
        // without Wi-Fi up the ESP-IDF RNG is only pseudo-random).
        Some(seed) => builder = builder.identity(Identity::from_secret_bytes(&seed)),
        None => warn!("node {n}: no identity seed in flash; generating one from the chip's RNG"),
    }
    let mut node = builder
        .config(config())
        .stun_servers(Vec::<String>::new())
        .signaling(LanSignaling::on(LanSignaling::GROUP, addr(n))?)
        .bind(SocketAddr::from((addr(n), PORT)))?;
    let heap_node = free_heap();
    info!(
        "node {n}: bound {} as {}, the Node took {} B, free heap {heap_node} B",
        node.local_addr(),
        node.peer_id(),
        heap_net - heap_node
    );
    node.join_room("peerwire-esp32", "lab");
    let joined = Instant::now();

    // Peers by the node number in the address they linked from.
    let mut names: HashMap<PeerId, u8> = HashMap::new();

    let mut seq = 0u32;
    let mut received = 0u64;
    let mut next_send = Instant::now() + Duration::from_secs(1);
    let mut next_report = Instant::now() + Duration::from_secs(10);
    loop {
        node.poll();
        while let Some(e) = node.try_recv_event() {
            match e {
                Event::Connected { peer, initiator, path } => {
                    if let Some(m) = node_of(&path) {
                        names.insert(peer, m);
                    }
                    let how = if initiator { "dialled" } else { "answered" };
                    let links = node.peers().count();
                    info!(
                        "node {n}: linked to node {} ({how}, {} ms after joining), {links} links, free heap {} B",
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
                            "node {n}: from node {}: {text}, {} B ({received} received)",
                            name(&names, &peer),
                            data.len()
                        );
                    }
                }
                Event::Disconnected { peer, reason } => {
                    warn!("node {n}: node {} left: {reason:?}", name(&names, &peer))
                }
                Event::ConnectFailed { peer } => warn!("node {n}: dial to node {} failed", name(&names, &peer)),
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
                    warn!("node {n}: send to node {}: {e:?}", name(&names, &peer));
                }
            }
        }
        if now >= next_report {
            next_report = now + Duration::from_secs(10);
            report(n, &node, heap_node, &names);
        }
        std::thread::sleep(node.next_timeout().unwrap_or(Duration::from_millis(10)).min(Duration::from_millis(10)));
    }
}

/// Free heap, the lowest it has been, the main task's unused stack, and each link's RTT.
fn report(n: u8, node: &Node, heap_node: u32, names: &HashMap<PeerId, u8>) {
    let free = free_heap();
    let min = unsafe { sys::esp_get_minimum_free_heap_size() };
    let stack_left = unsafe { sys::uxTaskGetStackHighWaterMark(std::ptr::null_mut()) };
    let links = node.peers().count();
    let per_link = if links > 0 { (heap_node.saturating_sub(free) as usize / links).to_string() } else { "-".into() };
    let rtts: Vec<String> = node
        .peers()
        .map(|p| {
            let rtt = node.peer_stats(p).and_then(|s| s.rtt).map_or("?".into(), |d| format!("{} ms", d.as_millis()));
            format!("node {}: {rtt}", name(names, p))
        })
        .collect();
    info!(
        "node {n}: report: {links} links, free heap {free} B (min {min} B), heap per link {per_link} B, \
         stack unused {stack_left} B, rtt [{}]",
        rtts.join(", ")
    );
}

fn free_heap() -> u32 {
    unsafe { sys::esp_get_free_heap_size() }
}

/// This node's number, the size of its messages in KiB (0: a short line) and its
/// identity seed from flash, or node 1 with short lines and no seed when none were written.
fn node_config() -> (u8, u8, Option<[u8; 32]>) {
    let mut b = [0u8; 37];
    let r = unsafe { sys::esp_flash_read(std::ptr::null_mut(), b.as_mut_ptr().cast(), NODE_CFG_ADDR, 37) };
    if r == sys::ESP_OK && b[..3] == *b"PWN" && (1..=MAX_NODES).contains(&b[3]) {
        let seed: [u8; 32] = b[5..].try_into().unwrap();
        // Erased flash reads 0xff.
        let seed = (seed != [0xff; 32] && seed != [0; 32]).then_some(seed);
        (b[3], b[4].min((config().max_message / 1024) as u8), seed)
    } else {
        (1, 0, None)
    }
}

fn addr(n: u8) -> Ipv4Addr {
    Ipv4Addr::new(10, 62, 0, 10 + n)
}

/// The node number in a direct path's address (10.62.0.<10+n>), if it is one.
fn node_of(path: &Path) -> Option<u8> {
    let Path::Direct(SocketAddr::V4(a)) = path else { return None };
    let [a0, a1, a2, last] = a.ip().octets();
    ([a0, a1, a2] == [10, 62, 0]).then(|| last.checked_sub(10)).flatten()
}

/// `peer`'s node number, or 0 if it hasn't linked directly yet.
fn name(names: &HashMap<PeerId, u8>, peer: &PeerId) -> u8 {
    names.get(peer).copied().unwrap_or(0)
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

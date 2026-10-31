//! Peerwire on an ESP32-C3: a feasibility prototype (NB-062), not a product.
//!
//! Each node reads its number from the last flash sector (run.sh patches it in),
//! takes a fixed test identity and the address 10.62.0.<10+n> on QEMU's emulated
//! Ethernet, and dials every lower-numbered node by an invite built from those,
//! so N nodes make a full mesh. Linked nodes send each other a numbered reliable
//! message every second, a short line or (run.sh's MSG_KIB) padded to that many
//! KiB. It logs free heap at each step, how long each dial took to link, and every
//! 10 s the heap, stack and link RTTs.

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
use peerwire::{Config, Event, Identity, Invite, Lane, Node, PeerId};

const PORT: u16 = 4040;
const MAX_NODES: u8 = 16;
/// Where run.sh writes b"PWN", the node number and the message size in KiB: the last
/// sector of a 4 MB flash, past the end of the app image.
const NODE_CFG_ADDR: u32 = 0x3F_F000;

fn main() -> anyhow::Result<()> {
    sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let (n, msg_kib) = node_config();
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
    eth.wait_netif_up()?;
    let heap_net = free_heap();
    info!("node {n}: network up at {}, free heap {heap_net} B", addr(n));

    let mut node = Node::builder()
        .identity(identity(n))
        .config(config())
        .stun_servers(Vec::<String>::new())
        .bind(SocketAddr::from((addr(n), PORT)))?;
    let heap_node = free_heap();
    info!("node {n}: bound {}, the Node took {} B, free heap {heap_node} B", node.local_addr(), heap_net - heap_node);

    let names: HashMap<PeerId, u8> = (1..=MAX_NODES).map(|m| (identity(m).peer_id(), m)).collect();
    let name = |p: &PeerId| names.get(p).copied().unwrap_or(0);
    let mut dialled: HashMap<PeerId, Instant> = HashMap::new();
    for m in 1..n {
        let peer = node.connect(&invite(m))?;
        dialled.insert(peer, Instant::now());
    }

    let mut seq = 0u32;
    let mut received = 0u64;
    let mut next_send = Instant::now() + Duration::from_secs(1);
    let mut next_report = Instant::now() + Duration::from_secs(10);
    loop {
        node.poll();
        while let Some(e) = node.try_recv_event() {
            match e {
                Event::Connected { peer, initiator, .. } => {
                    let took = dialled.remove(&peer).map(|t| format!(" in {} ms", t.elapsed().as_millis()));
                    let how = if initiator { "dialled" } else { "answered" };
                    let links = node.peers().count();
                    info!(
                        "node {n}: linked to node {} ({how}{}), {links} links, free heap {} B",
                        name(&peer),
                        took.unwrap_or_default(),
                        free_heap()
                    );
                }
                Event::Message { peer, data, .. } => {
                    received += 1;
                    if received <= 8 || received % 50 == 0 {
                        let text =
                            String::from_utf8_lossy(&data[..data.len().min(24)]).trim_end_matches('.').to_string();
                        info!("node {n}: from node {}: {text}, {} B ({received} received)", name(&peer), data.len());
                    }
                }
                Event::Disconnected { peer, reason } => warn!("node {n}: node {} left: {reason:?}", name(&peer)),
                Event::ConnectFailed { peer } => warn!("node {n}: dial to node {} failed", name(&peer)),
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
                    warn!("node {n}: send to node {}: {e:?}", name(&peer));
                }
            }
        }
        if now >= next_report {
            next_report = now + Duration::from_secs(10);
            report(n, &node, heap_node, &name);
        }
        std::thread::sleep(node.next_timeout().unwrap_or(Duration::from_millis(10)).min(Duration::from_millis(10)));
    }
}

/// Free heap, the lowest it has been, the main task's unused stack, and each link's RTT.
fn report(n: u8, node: &Node, heap_node: u32, name: &dyn Fn(&PeerId) -> u8) {
    let free = free_heap();
    let min = unsafe { sys::esp_get_minimum_free_heap_size() };
    let stack_left = unsafe { sys::uxTaskGetStackHighWaterMark(std::ptr::null_mut()) };
    let links = node.peers().count();
    let per_link = if links > 0 { (heap_node.saturating_sub(free) as usize / links).to_string() } else { "-".into() };
    let rtts: Vec<String> = node
        .peers()
        .map(|p| {
            let rtt = node.peer_stats(p).and_then(|s| s.rtt).map_or("?".into(), |d| format!("{} ms", d.as_millis()));
            format!("node {}: {rtt}", name(p))
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

/// This node's number and the size of its messages in KiB (0: a short line) from
/// flash, or node 1 with short lines when none were written.
fn node_config() -> (u8, u8) {
    let mut b = [0u8; 5];
    let r = unsafe { sys::esp_flash_read(std::ptr::null_mut(), b.as_mut_ptr().cast(), NODE_CFG_ADDR, 5) };
    if r == sys::ESP_OK && b[..3] == *b"PWN" && (1..=MAX_NODES).contains(&b[3]) {
        (b[3], b[4].min((config().max_message / 1024) as u8))
    } else {
        (1, 0)
    }
}

fn addr(n: u8) -> Ipv4Addr {
    Ipv4Addr::new(10, 62, 0, 10 + n)
}

/// A fixed identity per node number, so every node can build the others' invites.
/// Test only: anyone can derive these keys.
fn identity(n: u8) -> Identity {
    let mut secret = [0x5a; 32];
    secret[0] = n;
    Identity::from_secret_bytes(&secret)
}

fn invite(n: u8) -> String {
    Invite {
        peer: identity(n).peer_id(),
        key: None,
        addrs: vec![SocketAddr::from((addr(n), PORT))],
        relays: Vec::new(),
    }
    .to_link()
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

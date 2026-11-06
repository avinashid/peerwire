//! A room member over Bluetooth LE on an ESP32-C3 or ESP32 (NB-110c, docs/concepts/bluetooth.md), with Wi-Fi off.
//!
//! A board joins a room over Bluetooth only with its clock set (room hellos must be within a minute of the
//! receiver's clock). So with wifi.sh's record in the last flash sector, as the mesh example reads it, the board first
//! joins that Wi-Fi and sets its clock by SNTP, starts NimBLE and logs the heap with both radios on, then turns Wi-Fi
//! off. It joins room.sh's room ("esp32" if the flash names none) of the `lan_chat` and `room_chat` app, over
//! Bluetooth alone: a PC nearby running `ble_chat` in that room links with it over the radio.
//!
//! Built with `PW_KEEP_WIFI` set, it keeps Wi-Fi on and its socket on the board's address, with no signaling: a peer
//! is still found over Bluetooth only, and a link moves to UDP once that works (the spec's Paths: UDP first), so a PC
//! on the same network running `ble_chat` without `--no-udp` sees it move.
//!
//! It logs each link with its path kind, answers each message with "echo: " and the message, sends each linked peer
//! a numbered message every 5 s (every fourth one 2,000 B, a few GATT values long), and logs the links, the free heap
//! and the lowest it has been every 10 s. Build it with `FEATURES=ble EXAMPLE=ble ./build.sh`, then flash
//! out/ble.bin, or the copy wifi.sh writes.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::sys;
use log::{info, warn};
use peerwire_esp::ble::Nimble;
use peerwire_esp::peerwire::{Event, Lane, Node, PeerId};
use peerwire_esp::{clock, config, free_heap, Board, PORT};

/// Where wifi.sh and room.sh write their records: the last sector of a 4 MB flash (see examples/mesh.rs).
const CFG_ADDR: u32 = 0x3F_F000;
const WIFI_OFFSET: usize = 0x40;
const ROOM_OFFSET: usize = 0xC0;
/// The app of `lan_chat` and `room_chat` (examples/rust), and the room when the flash names none.
const APP: &str = "peerwire-lan-chat";
const ROOM: &str = "esp32";
const SEND_EVERY: Duration = Duration::from_secs(5);
const REPORT_EVERY: Duration = Duration::from_secs(10);
const LONG: usize = 2000;
/// Wi-Fi stays on, if wifi.sh named a network: set `PW_KEEP_WIFI` when building.
const KEEP_WIFI: bool = option_env!("PW_KEEP_WIFI").is_some();

fn main() {
    peerwire_esp::run(run)
}

fn run() -> anyhow::Result<()> {
    let boot = free_heap();
    info!("ble: boot, free heap {boot} B");
    let (wifi, room) = flash_config();
    let room = room.unwrap_or_else(|| ROOM.into());

    // Wi-Fi first, for the clock; its radio also makes the chip's random numbers true ones (BLE-R15).
    let board = match &wifi {
        Some((ssid, password)) => {
            let modem = Peripherals::take()?.modem;
            Some(Board::connect_with(modem, EspSystemEventLoop::take()?, ssid, password)?)
        }
        None => {
            // The node's socket needs lwIP, which Wi-Fi would start.
            sys::esp!(unsafe { sys::esp_netif_init() })?;
            None
        }
    };
    if !clock::is_set() {
        warn!("ble: the clock isn't set, so peers drop this board's hellos: wifi.sh names a network to set it from");
    }
    let radio = Nimble::new()?;
    let both = board.as_ref().map(|_| free_heap());
    if let Some(both) = both {
        info!("ble: Wi-Fi and NimBLE up, free heap {both} B, lowest {} B", lowest());
    }
    // Wi-Fi off, so links have the radio alone, unless kept.
    let board = board.filter(|_| KEEP_WIFI);
    let ip = board.as_ref().map_or(Ipv4Addr::LOCALHOST, Board::ip);
    let alone = free_heap();
    match &board {
        Some(_) => info!("ble: Wi-Fi kept on at {ip}, NimBLE up, free heap {alone} B"),
        None => info!("ble: Wi-Fi off, NimBLE up, free heap {alone} B"),
    }

    let mut node = Node::builder()
        .config(config())
        .stun_servers(Vec::<String>::new())
        .bluetooth(radio)
        .bind(SocketAddr::from((ip, PORT)))?;
    node.join_room(APP, &room);
    let joined = Instant::now();
    info!("ble: node {} joined {APP}/{room} over Bluetooth, free heap {} B", node.peer_id(), free_heap());
    let both = both.map_or("no Wi-Fi".into(), |b| format!("Wi-Fi and NimBLE {b} B"));
    let radios = if board.is_some() { "Wi-Fi kept, now" } else { "NimBLE alone" };
    let clock = if clock::is_set() { "set" } else { "unset" };
    let startup =
        format!("boot {boot} B, {both}, {radios} {alone} B, joined {APP}/{room} over Bluetooth, clock {clock}");

    let (mut next_send, mut next_report, mut sent, mut reports) = (joined + SEND_EVERY, joined, 0u32, 0u32);
    loop {
        node.poll();
        while let Some(event) = node.try_recv_event() {
            match event {
                Event::Connected { peer, .. } => {
                    let after = joined.elapsed().as_secs_f32();
                    info!("ble: {} linked {after:.1} s after the join, {:?}", short(&peer), node.path_kind(&peer));
                }
                Event::PathChanged { peer, .. } => info!("ble: {} moved to {:?}", short(&peer), node.path_kind(&peer)),
                Event::Message { peer, data, .. } => {
                    let text = String::from_utf8_lossy(&data[..data.len().min(48)]);
                    info!("ble: {} sent {} B: {text:?}", short(&peer), data.len());
                    if !data.starts_with(b"echo: ") {
                        let echo = [&b"echo: "[..], &data].concat();
                        if let Err(e) = node.send(&peer, Lane::Reliable, &echo) {
                            warn!("ble: echo to {}: {e:?}", short(&peer));
                        }
                    }
                }
                Event::Disconnected { peer, reason } => info!("ble: {} left: {reason:?}", short(&peer)),
                Event::ConnectFailed { peer } => warn!("ble: link to {} failed", short(&peer)),
                _ => {}
            }
        }
        let now = Instant::now();
        if now >= next_send {
            next_send += SEND_EVERY;
            let mut msg = format!("esp32 {sent}").into_bytes();
            if sent % 4 == 3 {
                msg.resize(LONG, b'.');
            }
            sent += 1;
            let peers: Vec<PeerId> = node.peers().copied().collect();
            for peer in peers {
                if let Err(e) = node.send(&peer, Lane::Reliable, &msg) {
                    warn!("ble: send to {}: {e:?}", short(&peer));
                }
            }
        }
        if now >= next_report {
            next_report += REPORT_EVERY;
            let links: Vec<String> = node.peers().map(|p| format!("{} {:?}", short(p), node.path_kind(p))).collect();
            info!(
                "ble: {} linked [{}], free heap {} B, lowest {} B",
                links.len(),
                links.join(", "),
                free_heap(),
                lowest()
            );
            // The USB console loses what the chip prints in the seconds after a reset: say the startup again.
            if reports < 3 || reports % 6 == 0 {
                info!("ble: startup: {startup}");
            }
            reports += 1;
        }
        peerwire_esp::idle(&node);
    }
}

fn short(peer: &PeerId) -> String {
    peer.to_string().chars().take(8).collect()
}

fn lowest() -> u32 {
    unsafe { sys::esp_get_minimum_free_heap_size() }
}

/// wifi.sh's and room.sh's records from the last flash sector, if they are there.
fn flash_config() -> (Option<(String, String)>, Option<String>) {
    let mut b = [0u8; 0x110];
    let r = unsafe { sys::esp_flash_read(std::ptr::null_mut(), b.as_mut_ptr().cast(), CFG_ADDR, b.len() as u32) };
    if r != sys::ESP_OK {
        return (None, None);
    }
    (wifi_config(&b[WIFI_OFFSET..]), room_config(&b[ROOM_OFFSET..]))
}

/// b"PWW", the SSID's length and the SSID, the password's length and the password.
fn wifi_config(b: &[u8]) -> Option<(String, String)> {
    let (&len, rest) = b.strip_prefix(b"PWW")?.split_first()?;
    let ssid = rest.get(..len as usize).filter(|_| (1..=32).contains(&len))?;
    let (&len, rest) = rest[ssid.len()..].split_first()?;
    let password = rest.get(..len as usize).filter(|_| len <= 64)?;
    Some((String::from_utf8(ssid.to_vec()).ok()?, String::from_utf8(password.to_vec()).ok()?))
}

/// b"PWR", the name's length (1 to 64) and the name.
fn room_config(b: &[u8]) -> Option<String> {
    let (&len, rest) = b.strip_prefix(b"PWR")?.split_first()?;
    let room = rest.get(..len as usize).filter(|_| (1..=64).contains(&len))?;
    String::from_utf8(room.to_vec()).ok()
}

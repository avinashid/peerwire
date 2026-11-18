//! Boards that link with each other over ESP-NOW alone (NB-135, docs/concepts/espnow.md): no Wi-Fi network, no
//! Bluetooth, no address typed in.
//!
//! Each board starts Wi-Fi as a station on no network, from a MAC address drawn at random at boot (`PW_ESPNOW_FIXED_MAC`
//! when building keeps the chip's own), on the channel it hears wifi.sh's network on, or channel 11
//! (`PW_ESPNOW_CHANNEL`), and joins room.sh's room ("esp32" if the flash names none) of the `lan_chat` app with
//! ESP-NOW as its only path (`PathKind::Bluetooth`): its adverts go out as ESP-NOW broadcasts, and the node proves the
//! room, does the Noise handshake and seals everything as over Bluetooth. A board with no clock takes the time from
//! the first room member that links. Frames are 1470 B where both boards have ESP-NOW v2, else 250 B
//! (`PW_ESPNOW_V1` keeps them to 250 B).
//!
//! Linked boards send each other a numbered reliable message every second, every fifth one 2,000 B (a dozen frames).
//! Each board logs its links, the first and every tenth message from each peer, and every 10 s a report: `ESPNOW_MESH
//! report ...` with its links, messages sent and received and missed, RTTs, the heap, and what the radio counted.
//! Build it with `FEATURES=espnow EXAMPLE=espnow_mesh ./build.sh`; the node number is the flash's `PWN` record, as
//! the mesh example reads it, and only names the board in its messages.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::sys;
use log::{info, warn};
use peerwire_esp::peerwire::espnow::Settings;
use peerwire_esp::peerwire::{Event, Lane, Node, PathKind, PeerId, Policy};
use peerwire_esp::{clock, config, espnow, heap, identity, wifi, PORT};

/// The config sector: b"PWN" and the node number at +0, wifi.sh's record at +0x40 and room.sh's at +0xC0.
const CFG_ADDR: u32 = 0x3F_F000;
const WIFI_OFFSET: usize = 0x40;
const ROOM_OFFSET: usize = 0xC0;
const APP: &str = "peerwire-lan-chat";
const ROOM: &str = "esp32";
const CHANNEL: Option<&str> = option_env!("PW_ESPNOW_CHANNEL");
const FIXED_MAC: bool = option_env!("PW_ESPNOW_FIXED_MAC").is_some();
const V1_ONLY: bool = option_env!("PW_ESPNOW_V1").is_some();
const SEND_EVERY: Duration = Duration::from_secs(1);
const REPORT_EVERY: Duration = Duration::from_secs(10);
const LONG: usize = 2000;

/// What came from one peer: the first and the highest message numbers, and how many messages.
#[derive(Default)]
struct Count {
    first: u32,
    last: u32,
    got: u32,
}

impl Count {
    /// Numbers from the first to the highest that never came: lost by the reliable lane, or still on their way.
    fn missed(&self) -> u32 {
        (self.last + 1).saturating_sub(self.first).saturating_sub(self.got)
    }
}

fn main() {
    peerwire_esp::run(run)
}

fn run() -> anyhow::Result<()> {
    let chip = if cfg!(esp32) { "esp32" } else { "esp32c3" };
    let (node_n, wifi_record, room) = flash_config();
    let room = room.unwrap_or_else(|| ROOM.into());
    info!("ESPNOW_MESH boot node={node_n} chip={chip} {}", heap());
    // Before any radio: the identity and the address are drawn from the SAR ADC's noise.
    let identity = identity::load_or_create()?;
    let mac = if FIXED_MAC { None } else { Some(wifi::random_mac()?) };

    let channel = match CHANNEL.map(str::parse::<u8>) {
        Some(Ok(c)) if (1..=13).contains(&c) => c,
        Some(_) => anyhow::bail!("PW_ESPNOW_CHANNEL must be 1 to 13"),
        None => 11,
    };
    let p = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;
    let mut wifi = match mac {
        Some(mac) => wifi::start_on_channel_as(p.modem, sysloop, channel, mac)?,
        None => wifi::start_on_channel(p.modem, sysloop, channel)?,
    };
    // The channel wifi.sh's network is on, so boards that join it later stay on the same channel. The log doesn't name
    // the network.
    let heard = wifi_record.and_then(|(ssid, _)| {
        let aps = wifi.scan().inspect_err(|e| warn!("ESPNOW_MESH scan: {e}")).ok()?;
        aps.iter().filter(|ap| ap.ssid == ssid.as_str()).max_by_key(|ap| ap.signal_strength).map(|ap| ap.channel)
    });
    let channel = heard.filter(|c| (1..=13).contains(c)).unwrap_or(channel);
    wifi::set_channel(channel)?;

    let radio = espnow::radio(!V1_ONLY, Settings::DEFAULT)?;
    let watch = radio.watch();
    let (version, max_frame) = (radio.ether().version(), radio.max_frame());
    info!(
        "ESPNOW_MESH up channel={channel} heard_ch={} mac={} espnow_ver={version} max_frame={max_frame} {}",
        heard.map_or(0, i32::from),
        if FIXED_MAC { "chip" } else { "random" },
        heap()
    );
    let mut timed = clock::is_set();
    let mut node = Node::builder()
        .identity(identity)
        .config(config())
        .stun_servers(Vec::<String>::new())
        .policy(Policy::new([PathKind::Bluetooth]))
        .bluetooth(radio)
        .bind(SocketAddr::from(([127, 0, 0, 1], PORT)))?;
    node.join_room(APP, &room);
    let joined = Instant::now();
    info!("ESPNOW_MESH joined {APP}/{room} as {} over ESP-NOW alone, {}", short(&node.peer_id()), heap());

    let mut from: HashMap<PeerId, Count> = HashMap::new();
    let (mut next_send, mut next_report, mut sent) = (joined + SEND_EVERY, joined + REPORT_EVERY, 0u32);
    loop {
        node.poll();
        if let Some(time) = node.clock_from_peer().filter(|_| !timed) {
            timed = true;
            if let Err(e) = clock::set(time) {
                warn!("ESPNOW_MESH the time a room member gave: {e}");
            }
        }
        while let Some(event) = node.try_recv_event() {
            match event {
                Event::Connected { peer, .. } => info!(
                    "ESPNOW_MESH linked {} after_ms={} path={:?} links={} {}",
                    short(&peer),
                    joined.elapsed().as_millis(),
                    node.path_kind(&peer),
                    node.peers().count(),
                    heap()
                ),
                Event::Message { peer, data, .. } => {
                    let n = number(&data);
                    let f = from.entry(peer).or_default();
                    if let Some(n) = n {
                        if f.got == 0 {
                            f.first = n;
                        }
                        (f.first, f.last) = (f.first.min(n), f.last.max(n));
                    }
                    f.got += 1;
                    if f.got == 1 || f.got % 10 == 0 {
                        info!(
                            "ESPNOW_MESH from {} #{} {} B, {} received",
                            short(&peer),
                            n.unwrap_or(0),
                            data.len(),
                            f.got
                        );
                    }
                }
                Event::Disconnected { peer, reason } => info!("ESPNOW_MESH left {} {reason:?}", short(&peer)),
                Event::ConnectFailed { peer } => warn!("ESPNOW_MESH link to {} failed", short(&peer)),
                _ => {}
            }
        }
        let now = Instant::now();
        if now >= next_send {
            next_send += SEND_EVERY;
            sent += 1;
            let mut msg = format!("espnow #{sent} from node {node_n}").into_bytes();
            if sent % 5 == 0 {
                msg.resize(LONG, b'.');
            }
            let peers: Vec<PeerId> = node.peers().copied().collect();
            for peer in peers {
                if let Err(e) = node.send(&peer, Lane::Reliable, &msg) {
                    warn!("ESPNOW_MESH send to {}: {e:?}", short(&peer));
                }
            }
        }
        if now >= next_report {
            next_report += REPORT_EVERY;
            report(&node, node_n, sent, &from, &watch.stats(), &radio_drops());
        }
        peerwire_esp::idle(&node);
    }
}

/// The 10 s report: links and their RTTs, messages, heap and stack, and the radio's counts.
fn report(
    node: &Node,
    n: u8,
    sent: u32,
    from: &HashMap<PeerId, Count>,
    r: &peerwire_esp::peerwire::espnow::Stats,
    drops: &str,
) {
    let links: Vec<String> = node
        .peers()
        .map(|p| {
            let rtt = node.peer_stats(p).and_then(|s| s.rtt).map_or("?".into(), |d| format!("{}ms", d.as_millis()));
            let (got, missed) = from.get(p).map_or((0, 0), |f| (f.got, f.missed()));
            format!("{}:{rtt}:got{got}:missed{missed}", short(p))
        })
        .collect();
    let received: u32 = from.values().map(|f| f.got).sum();
    let stack = unsafe { sys::uxTaskGetStackHighWaterMark(std::ptr::null_mut()) };
    info!(
        "ESPNOW_MESH report node={n} links={} [{}] sent={sent} received={received} {} stack_unused={stack} \
         radio conns={} in={} out={} tx_failed={} values_in={} values_out={} unknown={} old={} ahead={} gaps={} \
         orphans={} malformed={} closes_denied={} closed_by_peer={} closed_quiet={} closed_failing={} \
         events_dropped={} {drops}",
        links.len(),
        links.join(" "),
        heap(),
        r.connections,
        r.frames_in,
        r.frames_out,
        r.tx_failed,
        r.values_in,
        r.values_out,
        r.unknown,
        r.old,
        r.ahead,
        r.gaps,
        r.orphans,
        r.malformed,
        r.closes_denied,
        r.closed_by_peer,
        r.closed_quiet,
        r.closed_failing,
        r.events_dropped,
    );
}

/// The ether's own drops, which the node's radio doesn't see.
fn radio_drops() -> String {
    let (rx, not_ours, refused, stray) = espnow::dropped();
    format!("rx_dropped={rx} not_ours={not_ours} send_refused={refused} stray_callbacks={stray}")
}

/// The number in "espnow #N from node M".
fn number(data: &[u8]) -> Option<u32> {
    let text = std::str::from_utf8(data.get(..data.len().min(40))?).ok()?;
    text.strip_prefix("espnow #")?.split(' ').next()?.parse().ok()
}

fn short(peer: &PeerId) -> String {
    peer.to_string().chars().take(8).collect()
}

/// The node number (0 if none), wifi.sh's record and room.sh's from the last flash sector.
#[allow(clippy::type_complexity)]
fn flash_config() -> (u8, Option<(String, String)>, Option<String>) {
    let mut b = [0u8; 0x110];
    let r = unsafe { sys::esp_flash_read(std::ptr::null_mut(), b.as_mut_ptr().cast(), CFG_ADDR, b.len() as u32) };
    if r != sys::ESP_OK {
        return (0, None, None);
    }
    let n = if b[..3] == *b"PWN" && (1..=16).contains(&b[3]) { b[3] } else { 0 };
    (n, wifi_config(&b[WIFI_OFFSET..]), room_config(&b[ROOM_OFFSET..]))
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

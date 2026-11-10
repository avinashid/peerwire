//! The board's own LED, switched from anywhere on the internet (NB-126): by a phone's browser on mobile data, over the
//! room's Nostr relay, on an ESP32-C3 or ESP32 on Wi-Fi.
//!
//! The same commands as `ble_led` (`led on`, `led off`, `led toggle`, `led` alone asks; on the serial console `id` and
//! `id new` too), from a peer in its room. A browser, which can't send UDP, links over the relay itself, a pipe
//! (`PathKind::Signaling`): the LED page's "Internet" choice (`packages/js/demo/led.html`), from anywhere. A PC links
//! as it would with any board: `room_chat` from anywhere, `lan_chat` on its LAN. Anyone who knows the room's name can
//! switch the LED, and learns the board's home address from its offers (NB126-1), so the room must be one nobody can
//! guess: 22 or more of `A-Z a-z 0-9 _ -`, as the chat page asks, such as the random one room.sh names unless told
//! otherwise. It has no default room, and refuses a shorter name.
//!
//! It joins wifi.sh's network and room.sh's room (it needs both) of the `lan_chat` app, on the LAN and over
//! `wss://nostr.mom` (one of the browser's default relays). Build it with
//! `FEATURES=nostr EXAMPLE=nostr_led ./build.sh`, then write the Wi-Fi and room records with wifi.sh and flash; the LED
//! is GPIO8, lit low, as on the ESP32-C3 SuperMini (`PW_LED_PIN`, `PW_LED_HIGH` for another board, see `ble_led`).

use std::time::{Duration, Instant};

use anyhow::anyhow;
use log::{info, warn};
use peerwire_esp::console::Console;
use peerwire_esp::peerwire::{Event, PathKind};
use peerwire_esp::{free_heap, identity, nostr_thread, Board, NOSTR_RELAY};

#[path = "led/common.rs"]
mod common;
use common::{flash_config, send, short, Led, APP, HELP};

const REPORT_EVERY: Duration = Duration::from_secs(30);

fn main() {
    peerwire_esp::run(run)
}

fn run() -> anyhow::Result<()> {
    // Before any radio, as a new one is drawn only then.
    let identity = identity::load_or_create()?;
    let mut led = Led::new()?;
    led.blink()?;
    let (wifi, room) = flash_config();
    let room = room.ok_or_else(|| anyhow!("no room record: name one with room.sh (a random one by default)"))?;
    if guessable(&room) {
        anyhow::bail!("room {room:?} could be guessed: use 22 or more of A-Z a-z 0-9 _ - (room.sh names one)");
    }
    let (ssid, password) = wifi.ok_or_else(|| anyhow!("no Wi-Fi record: write one with wifi.sh, then flash"))?;
    // Joins, and sets the clock: relays' certificates and room hellos need it.
    let board = Board::connect(&ssid, &password)?;
    let console = Console::start()?;
    let mut node = board.builder_with_nostr(NOSTR_RELAY)?.identity(identity).bind(board.addr())?;
    node.join_room(APP, &room);
    info!("led: joined {APP}/{room} on the LAN and over {NOSTR_RELAY}, free heap {} B; {HELP}", free_heap());

    let mut next_report = Instant::now() + REPORT_EVERY;
    loop {
        node.poll();
        while let Some(event) = node.try_recv_event() {
            match event {
                Event::Connected { peer, .. } => {
                    let how = match node.path_kind(&peer) {
                        Some(PathKind::Signaling) => "over the relay",
                        Some(PathKind::Peers | PathKind::RelayPeers) => "through a peer",
                        Some(PathKind::Turn) => "through TURN",
                        _ => "directly",
                    };
                    info!("led: {} linked {how}, free heap {} B", short(&peer), free_heap());
                    send(&mut node, &peer, format!("{}; {HELP}", led.state()).as_bytes());
                }
                Event::Message { peer, data, .. } => {
                    let text = String::from_utf8_lossy(&data[..data.len().min(48)]);
                    info!("led: {} sent {} B: {text:?}", short(&peer), data.len());
                    match std::str::from_utf8(&data).ok().and_then(|text| led.command(text)) {
                        Some(state) => {
                            let state = state?;
                            info!("led: {state}, asked by {}", short(&peer));
                            send(&mut node, &peer, state.as_bytes());
                        }
                        None if data.starts_with(b"echo: ") => {}
                        None => send(&mut node, &peer, &[&b"echo: "[..], &data].concat()),
                    }
                }
                Event::Disconnected { peer, reason } => info!("led: {} left: {reason:?}", short(&peer)),
                Event::ConnectFailed { peer } => warn!("led: link to {} failed", short(&peer)),
                _ => {}
            }
        }
        while let Some(line) = console.line() {
            match line.as_str() {
                "id" => info!("led: id {}", node.peer_id()),
                "id new" => {
                    identity::renew()?;
                    peerwire_esp::restart();
                }
                _ => match led.command(&line) {
                    Some(state) => info!("led: {}", state?),
                    None => warn!("led: {line:?} isn't a command: `led on`, `led off`, `led toggle`, `id` or `id new`"),
                },
            }
        }
        if Instant::now() >= next_report {
            next_report += REPORT_EVERY;
            let relay = nostr_thread().map_or(String::new(), |(unused, _)| format!(", relay stack unused {unused} B"));
            // The USB console loses what the chip prints in the seconds after a reset: say the room again.
            info!(
                "led: {} linked in {APP}/{room} over {NOSTR_RELAY}, {}, free heap {} B{relay}",
                node.peers().count(),
                led.state(),
                free_heap()
            );
        }
        peerwire_esp::idle(&node);
    }
}

/// Whether others could guess `room`, as the chat page judges it (`packages/js/demo/chat.js`): shorter than 22, or
/// with a character outside `A-Z a-z 0-9 _ -`.
fn guessable(room: &str) -> bool {
    room.len() < 22 || !room.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

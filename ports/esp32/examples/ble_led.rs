//! The board's own LED, switched by a peer in its room over Bluetooth LE (NB-125), on an ESP32-C3 or ESP32.
//!
//! A peer sends `led on`, `led off` or `led toggle` (`led` alone asks), say as a line typed in `ble_chat` on a Linux
//! PC nearby, and the board switches its LED and answers `led: on` or `led: off`. It tells each peer that links the
//! LED's state and the commands, answers anything else with "echo: " and the message, and takes the same commands, with
//! `id` and `id new`, on the serial console. Anyone in the room can switch the LED: the room's name is the key.
//!
//! The LED is GPIO8, lit when the pin is low, as on the ESP32-C3 SuperMini. For another board, build with `PW_LED_PIN`
//! set to its pin, and `PW_LED_HIGH` set if it's lit when the pin is high (GPIO2 on many ESP32 boards). An addressable
//! RGB LED, as on Espressif's C3 DevKits, needs the RMT driver, which this doesn't have. The LED blinks 3 times at
//! boot: if it doesn't, the pin is wrong.
//!
//! As the `ble` example does, it joins room.sh's room ("esp32" if the flash names none) of the `lan_chat` app over
//! Bluetooth alone. Room hellos must be within a minute of the receiver's clock, and the board has none of its own: if
//! wifi.sh saved a network and it is there within 20 s, the board sets its clock by SNTP and turns Wi-Fi off; if not,
//! it takes the time from the first room member that links (a PC's `ble_chat`, a phone's browser page). Wi-Fi is never
//! needed. Build it with `FEATURES=ble EXAMPLE=ble_led ./build.sh`, then flash out/ble_led.bin; on the PC:
//!
//! ```sh
//! cargo run -p peerwire --features ble-bluez --example ble_chat -- ROOM --no-udp
//! ```

use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::sys;
use log::{info, warn};
use peerwire_esp::ble::Nimble;
use peerwire_esp::console::Console;
use peerwire_esp::peerwire::{Event, Node};
use peerwire_esp::{clock, config, free_heap, identity, Board, PORT};

#[path = "led/common.rs"]
mod common;
use common::{flash_config, send, short, Led, APP, HELP, ROOM};

const REPORT_EVERY: Duration = Duration::from_secs(30);
/// How long the board tries wifi.sh's network at boot before it goes on over Bluetooth alone.
const WIFI_WAIT: Duration = Duration::from_secs(20);

fn main() {
    peerwire_esp::run(run)
}

fn run() -> anyhow::Result<()> {
    // Before any radio, as a new one is drawn only then.
    let identity = identity::load_or_create()?;
    let mut led = Led::new()?;
    led.blink()?;
    let (wifi, room) = flash_config();
    let room = room.unwrap_or_else(|| ROOM.into());

    // Wi-Fi first if it's there, for the clock; its radio also makes the chip's random numbers true ones (BLE-R15).
    let board = match &wifi {
        Some((ssid, password)) => {
            let modem = Peripherals::take()?.modem;
            Board::connect_within(modem, EspSystemEventLoop::take()?, ssid, password, WIFI_WAIT)
                .inspect_err(|e| warn!("led: {e:#}; going on over Bluetooth alone"))
                .ok()
        }
        None => None,
    };
    // The node's socket needs lwIP, which Wi-Fi starts if it ran (a second call does nothing).
    sys::esp!(unsafe { sys::esp_netif_init() })?;
    let mut timed = clock::is_set();
    if !timed {
        info!("led: the clock isn't set: the first room member to link gives the time");
    }
    let radio = Nimble::new()?;
    // Wi-Fi off, so links have the radio alone.
    drop(board);

    let console = Console::start()?;
    let mut node = Node::builder()
        .identity(identity)
        .config(config())
        .stun_servers(Vec::<String>::new())
        .bluetooth(radio)
        .bind(SocketAddr::from((Ipv4Addr::LOCALHOST, PORT)))?;
    node.join_room(APP, &room);
    info!("led: joined {APP}/{room} over Bluetooth, free heap {} B; {HELP}", free_heap());

    let mut next_report = Instant::now() + REPORT_EVERY;
    loop {
        node.poll();
        if let Some(time) = node.clock_from_peer().filter(|_| !timed) {
            timed = true;
            if let Err(e) = clock::set(time) {
                warn!("led: the time a room member gave: {e}");
            }
        }
        while let Some(event) = node.try_recv_event() {
            match event {
                Event::Connected { peer, .. } => {
                    info!("led: {} linked", short(&peer));
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
            // The USB console loses what the chip prints in the seconds after a reset: say the room again.
            info!("led: {} linked in {APP}/{room}, {}, free heap {} B", node.peers().count(), led.state(), free_heap());
        }
        peerwire_esp::idle(&node);
    }
}

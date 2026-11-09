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

use anyhow::anyhow;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::sys;
use log::{info, warn};
use peerwire_esp::ble::Nimble;
use peerwire_esp::console::Console;
use peerwire_esp::peerwire::{Event, Lane, Node, PeerId};
use peerwire_esp::{clock, config, free_heap, identity, Board, PORT};

/// Where wifi.sh and room.sh write their records: the last sector of a 4 MB flash (see examples/mesh.rs).
const CFG_ADDR: u32 = 0x3F_F000;
const WIFI_OFFSET: usize = 0x40;
const ROOM_OFFSET: usize = 0xC0;
/// The app of `lan_chat` and `ble_chat` (examples/rust), and the room when the flash names none.
const APP: &str = "peerwire-lan-chat";
const ROOM: &str = "esp32";
/// The LED's GPIO, `PW_LED_PIN` when building, and whether it's lit when the pin is high, `PW_LED_HIGH`.
const LED_PIN: Option<&str> = option_env!("PW_LED_PIN");
const LED_HIGH: bool = option_env!("PW_LED_HIGH").is_some();
const BLINK: Duration = Duration::from_millis(150);
const REPORT_EVERY: Duration = Duration::from_secs(30);
/// How long the board tries wifi.sh's network at boot before it goes on over Bluetooth alone.
const WIFI_WAIT: Duration = Duration::from_secs(20);
const HELP: &str = "send `led on`, `led off` or `led toggle`";

fn main() {
    peerwire_esp::run(run)
}

fn run() -> anyhow::Result<()> {
    // Before any radio, as a new one is drawn only then.
    let identity = identity::load_or_create()?;
    let mut led = Led::new()?;
    for _ in 0..3 {
        led.set(true)?;
        std::thread::sleep(BLINK);
        led.set(false)?;
        std::thread::sleep(BLINK);
    }
    info!("led: GPIO{} blinked 3 times, lit when {}", led.pin, if LED_HIGH { "high" } else { "low" });
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

/// The LED on one GPIO, driven straight through ESP-IDF's GPIO driver.
struct Led {
    pin: i32,
    on: bool,
}

impl Led {
    /// The LED off, on `PW_LED_PIN` or GPIO8.
    fn new() -> anyhow::Result<Self> {
        let pin = match LED_PIN {
            Some(pin) => pin.parse().map_err(|_| anyhow!("PW_LED_PIN {pin:?} isn't a GPIO number"))?,
            None => 8,
        };
        sys::esp!(unsafe { sys::gpio_reset_pin(pin) })?;
        sys::esp!(unsafe { sys::gpio_set_direction(pin, sys::gpio_mode_t_GPIO_MODE_OUTPUT) })?;
        let mut led = Self { pin, on: false };
        led.set(false)?;
        Ok(led)
    }

    fn set(&mut self, on: bool) -> anyhow::Result<()> {
        sys::esp!(unsafe { sys::gpio_set_level(self.pin, u32::from(on == LED_HIGH)) })?;
        self.on = on;
        Ok(())
    }

    /// If `text` is one of the commands (in any case, with spaces around it), carries it out and gives the LED's state.
    fn command(&mut self, text: &str) -> Option<anyhow::Result<String>> {
        let on = match text.trim().to_ascii_lowercase().as_str() {
            "led" => self.on,
            "led on" => true,
            "led off" => false,
            "led toggle" => !self.on,
            _ => return None,
        };
        Some(self.set(on).map(|()| self.state()))
    }

    fn state(&self) -> String {
        format!("led: {}", if self.on { "on" } else { "off" })
    }
}

fn send(node: &mut Node, peer: &PeerId, data: &[u8]) {
    if let Err(e) = node.send(peer, Lane::Reliable, data) {
        warn!("led: send to {}: {e:?}", short(peer));
    }
}

fn short(peer: &PeerId) -> String {
    peer.to_string().chars().take(8).collect()
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

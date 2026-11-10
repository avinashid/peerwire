//! What the LED examples share (`ble_led`, `nostr_led`): the LED, the commands a peer sends it, and the Wi-Fi and
//! room records wifi.sh and room.sh write. Each example takes it with `#[path = "led/common.rs"] mod common;`.

use std::time::Duration;

use anyhow::anyhow;
use esp_idf_svc::sys;
use log::warn;
use peerwire_esp::peerwire::{Lane, Node, PeerId};

/// Where wifi.sh and room.sh write their records: the last sector of a 4 MB flash (see examples/mesh.rs).
const CFG_ADDR: u32 = 0x3F_F000;
const WIFI_OFFSET: usize = 0x40;
const ROOM_OFFSET: usize = 0xC0;
/// The app of `lan_chat`, `room_chat` and `ble_chat` (examples/rust).
pub const APP: &str = "peerwire-lan-chat";
/// The LED's GPIO, `PW_LED_PIN` when building, and whether it's lit when the pin is high, `PW_LED_HIGH`.
const LED_PIN: Option<&str> = option_env!("PW_LED_PIN");
const LED_HIGH: bool = option_env!("PW_LED_HIGH").is_some();
const BLINK: Duration = Duration::from_millis(150);
pub const HELP: &str = "send `led on`, `led off` or `led toggle`";

/// The LED on one GPIO, driven straight through ESP-IDF's GPIO driver.
pub struct Led {
    pin: i32,
    on: bool,
}

impl Led {
    /// The LED off, on `PW_LED_PIN` or GPIO8.
    pub fn new() -> anyhow::Result<Self> {
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

    /// Blinks 3 times, so a wrong pin shows at boot.
    pub fn blink(&mut self) -> anyhow::Result<()> {
        for _ in 0..3 {
            self.set(true)?;
            std::thread::sleep(BLINK);
            self.set(false)?;
            std::thread::sleep(BLINK);
        }
        log::info!("led: GPIO{} blinked 3 times, lit when {}", self.pin, if LED_HIGH { "high" } else { "low" });
        Ok(())
    }

    pub fn set(&mut self, on: bool) -> anyhow::Result<()> {
        sys::esp!(unsafe { sys::gpio_set_level(self.pin, u32::from(on == LED_HIGH)) })?;
        self.on = on;
        Ok(())
    }

    /// If `text` is one of the commands (in any case, with spaces around it), carries it out and gives the LED's state.
    pub fn command(&mut self, text: &str) -> Option<anyhow::Result<String>> {
        let on = match text.trim().to_ascii_lowercase().as_str() {
            "led" => self.on,
            "led on" => true,
            "led off" => false,
            "led toggle" => !self.on,
            _ => return None,
        };
        Some(self.set(on).map(|()| self.state()))
    }

    pub fn state(&self) -> String {
        format!("led: {}", if self.on { "on" } else { "off" })
    }
}

pub fn send(node: &mut Node, peer: &PeerId, data: &[u8]) {
    if let Err(e) = node.send(peer, Lane::Reliable, data) {
        warn!("led: send to {}: {e:?}", short(peer));
    }
}

pub fn short(peer: &PeerId) -> String {
    peer.to_string().chars().take(8).collect()
}

/// wifi.sh's and room.sh's records from the last flash sector, if they are there.
pub fn flash_config() -> (Option<(String, String)>, Option<String>) {
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

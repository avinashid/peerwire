//! The board's identity, kept across restarts (NB-119).
//!
//! A [`Node`](peerwire::Node) draws a new identity each time it's built unless it's given one, so a board was a new
//! peer on every boot. [`load_or_create`] draws a 32-byte seed the first time, keeps it in NVS and gives the same
//! identity on every later boot, for [`Builder::identity`](peerwire::Builder::identity). [`renew`] erases it, so the
//! next boot draws another. Erasing the NVS partition does too: the template's flash.sh does on each flash, so each
//! flash gets a new identity (`--keep-id` keeps it), and so does writing a full flash image such as build.sh's.
//!
//! The seed is drawn on the chip and never leaves it, but it isn't encrypted in NVS: anyone with the board in hand
//! can read it off its flash, and be the board. ESP-IDF encrypts NVS on the ESP32 only with flash encryption on, and
//! on the C3 also with an HMAC key in eFuse; both burn eFuses for good, and the port turns neither on. [`renew`]
//! doesn't scrub the old seed either: NVS marks it erased and leaves it in flash until its page is reused. To retire
//! a board's key before handing the board on, erase its flash.

use anyhow::bail;
use esp_idf_svc::nvs::EspDefaultNvs;
use esp_idf_svc::sys;
use log::{info, warn};
use peerwire::Identity;

/// The NVS namespace the seed is kept in.
pub const NAMESPACE: &str = "peerwire";

/// The seed's key in [`NAMESPACE`].
const KEY: &str = "id";

/// The identity kept in NVS, or a new one, drawn and kept there, if there is none.
///
/// **Call it first, before Wi-Fi or Bluetooth is set up** ([`Board::connect`](crate::Board::connect),
/// [`wifi::join`](crate::wifi::join), `ble::Nimble::new`). A new seed is drawn from the SAR ADC's noise, which
/// ESP-IDF turns on for the draw and documents as a true random source only before a radio is set up (NB119-1): with
/// none kept and a radio set up, this is an error. A kept one loads at any time. Don't call it while the firmware
/// reads the ADC or, on the ESP32, uses I2S.
pub fn load_or_create() -> anyhow::Result<Identity> {
    let nvs = store()?;
    match nvs.blob_len(KEY)? {
        Some(32) => {
            let mut seed = [0u8; 32];
            nvs.get_blob(KEY, &mut seed)?;
            return Ok(Identity::from_secret_bytes(&seed));
        }
        Some(len) => warn!("the kept identity is {len} B, not 32: drawing a new one"),
        None => {}
    }
    let mut seed = [0u8; 32];
    random(&mut seed)?;
    nvs.set_blob(KEY, &seed)?;
    let identity = Identity::from_secret_bytes(&seed);
    info!("drew a new identity, {}, and kept it", identity.peer_id());
    Ok(identity)
}

/// Erases the kept identity, so the next boot draws a new one. A running node keeps the identity it was built with:
/// restart the chip after this ([`restart`](crate::restart)).
pub fn renew() -> anyhow::Result<()> {
    store()?.remove(KEY)?;
    info!("the kept identity is erased: the next boot draws a new one");
    Ok(())
}

fn store() -> anyhow::Result<EspDefaultNvs> {
    Ok(EspDefaultNvs::new(crate::nvs()?, NAMESPACE, true)?)
}

/// Fills `buf` with true random numbers from the SAR ADC's noise, which ESP-IDF documents for before any radio is set
/// up, and which then mustn't be turned on: once one is, an error.
fn random(buf: &mut [u8]) -> anyhow::Result<()> {
    if radio_set_up() {
        bail!("no identity is kept yet, and a radio is set up: load the identity before starting Wi-Fi or Bluetooth");
    }
    unsafe {
        sys::bootloader_random_enable();
        sys::esp_fill_random(buf.as_mut_ptr().cast(), buf.len());
        sys::bootloader_random_disable();
    }
    Ok(())
}

/// Whether ESP-IDF has Wi-Fi or the Bluetooth controller set up, started or not: the controller whenever ESP-IDF is
/// built with Bluetooth, which firmware may start itself, without the `ble` feature.
fn radio_set_up() -> bool {
    let mut mode = sys::wifi_mode_t_WIFI_MODE_NULL;
    let wifi = unsafe { sys::esp_wifi_get_mode(&mut mode) } != sys::ESP_ERR_WIFI_NOT_INIT;
    #[cfg(esp_idf_bt_enabled)]
    let bt =
        unsafe { sys::esp_bt_controller_get_status() } != sys::esp_bt_controller_status_t_ESP_BT_CONTROLLER_STATUS_IDLE;
    #[cfg(not(esp_idf_bt_enabled))]
    let bt = false;
    wifi || bt
}

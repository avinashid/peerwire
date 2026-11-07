//! The board's identity, kept across restarts (NB-119).
//!
//! A [`Node`](peerwire::Node) draws a new identity each time it's built unless it's given one, so a board was a new
//! peer on every boot. [`load_or_create`] draws a 32-byte seed the first time, keeps it in NVS and gives the same
//! identity on every later boot, for [`Builder::identity`](peerwire::Builder::identity). [`renew`] erases it, so the
//! next boot draws another. Erasing the NVS partition does too: the template's flash.sh does on each flash, so each
//! flash gets a new identity (`--keep-id` keeps it), and so does writing a full flash image such as build.sh's.
//!
//! The seed is drawn on the chip and never leaves it. NVS isn't encrypted unless ESP-IDF's flash encryption is on,
//! which burns eFuses for good: anyone with the board in hand can read the seed off its flash, and be the board.

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
/// A new seed is drawn from the radio's noise while Wi-Fi or Bluetooth runs, as the chip's random numbers are true
/// ones only then (NB-063). Before either starts, it's drawn from the SAR ADC's noise, which ESP-IDF turns on for the
/// draw: don't call this while the firmware reads the ADC.
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
    random(&mut seed);
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

/// Fills `buf` with true random numbers: from the radio's noise while one runs, else from the SAR ADC's, which must
/// not be turned on while a radio runs.
fn random(buf: &mut [u8]) {
    let radio = radio_on();
    unsafe {
        if !radio {
            sys::bootloader_random_enable();
        }
        sys::esp_fill_random(buf.as_mut_ptr().cast(), buf.len());
        if !radio {
            sys::bootloader_random_disable();
        }
    }
}

/// Whether Wi-Fi or Bluetooth runs. Wi-Fi counts once ESP-IDF has it set up: [`wifi::join`](crate::wifi::join) starts
/// it before it returns.
fn radio_on() -> bool {
    let mut mode = sys::wifi_mode_t_WIFI_MODE_NULL;
    let wifi = unsafe { sys::esp_wifi_get_mode(&mut mode) } == sys::ESP_OK && mode != sys::wifi_mode_t_WIFI_MODE_NULL;
    #[cfg(feature = "ble")]
    let ble = unsafe { sys::esp_bt_controller_get_status() }
        == sys::esp_bt_controller_status_t_ESP_BT_CONTROLLER_STATUS_ENABLED;
    #[cfg(not(feature = "ble"))]
    let ble = false;
    wifi || ble
}

//! Joining a Wi-Fi network as a station, and its signal strength.

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys;
use esp_idf_svc::wifi::{AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi, WifiEvent};
use log::{info, warn};

/// A station on a Wi-Fi network, as [`join`] gives it. Dropping it takes the network down.
pub type Wifi = BlockingWifi<EspWifi<'static>>;

/// Joins the Wi-Fi network `ssid` (2.4 GHz; an empty `password` for an open one) and waits for a DHCP address,
/// retrying until it has one.
///
/// Modem sleep is off: it holds a station's frames until the next beacon, ~100 ms on every round trip. Each attempt
/// steps the transmit power down (20, 15, 11 and 8.5 dBm, then round again): small C3 boards with a chip antenna often
/// can't take full power, so the access point hears their short auth frame but never their association request
/// (NB-091). Each failed attempt logs ESP-IDF's reason for it.
pub fn join(modem: Modem<'static>, sysloop: EspSystemEventLoop, ssid: &str, password: &str) -> anyhow::Result<Wifi> {
    // ESP-IDF's quarter-dBm.
    const TX_POWER: [i8; 4] = [80, 60, 44, 34];
    let nvs = EspDefaultNvsPartition::take()?;
    let mut w = BlockingWifi::wrap(EspWifi::new(modem, sysloop.clone(), Some(nvs))?, sysloop.clone())?;
    w.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: ssid.try_into().map_err(|_| anyhow!("SSID over 32 bytes"))?,
        password: password.try_into().map_err(|_| anyhow!("password over 64 bytes"))?,
        auth_method: if password.is_empty() { AuthMethod::None } else { AuthMethod::WPA2Personal },
        ..Default::default()
    }))?;
    w.start()?;
    sys::esp!(unsafe { sys::esp_wifi_set_ps(sys::wifi_ps_type_t_WIFI_PS_NONE) })?;
    let aps = w.scan()?;
    // The log names neither the network nor its access points: a log people share would tell where the board is.
    let seen: Vec<_> = aps.iter().filter(|ap| ap.ssid == ssid).collect();
    if seen.is_empty() {
        warn!("the network isn't among the {} in range", aps.len());
    }
    for ap in seen {
        let (channel, dbm, auth) = (ap.channel, ap.signal_strength, ap.auth_method);
        info!("hears the network on channel {channel}, {dbm} dBm, {auth:?}");
    }
    // The last disconnect's reason (ESP-IDF's wifi_err_reason_t); 0 while there is none.
    let reason = Arc::new(AtomicU16::new(0));
    let _sub = sysloop.subscribe::<WifiEvent, _>({
        let reason = reason.clone();
        move |event| {
            if let WifiEvent::StaDisconnected(d) = event {
                reason.store(d.reason(), Ordering::Relaxed);
            }
        }
    })?;
    for attempt in 1.. {
        let power = TX_POWER[(attempt - 1) % TX_POWER.len()];
        let dbm = f32::from(power) / 4.0;
        sys::esp!(unsafe { sys::esp_wifi_set_max_tx_power(power) })?;
        info!("joining Wi-Fi, attempt {attempt} at {dbm} dBm");
        reason.store(0, Ordering::Relaxed);
        w.wifi_mut().connect()?;
        let start = Instant::now();
        while !w.is_connected()? && reason.load(Ordering::Relaxed) == 0 && start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(50));
        }
        if w.is_connected()? {
            info!("joined on attempt {attempt} at {dbm} dBm");
            break;
        }
        let r = reason.load(Ordering::Relaxed);
        let why = match r {
            0 => "no answer in 10 s",
            2 => "auth expired",
            4 => "the AP never answered the association request",
            15 | 204 => "4-way handshake timed out: wrong password?",
            201 => "AP not found",
            202 => "auth failed",
            203 => "association refused",
            _ => "see ESP-IDF's wifi_err_reason_t",
        };
        warn!("attempt {attempt} failed, reason {r}: {why}");
        let _ = w.wifi_mut().disconnect();
        std::thread::sleep(Duration::from_millis(500));
    }
    w.wait_netif_up()?;
    Ok(w)
}

/// The signal strength of the access point this station is joined to, in dBm; `None` when it isn't joined.
pub fn rssi() -> Option<i8> {
    let mut ap: sys::wifi_ap_record_t = unsafe { std::mem::zeroed() };
    (unsafe { sys::esp_wifi_sta_get_ap_info(&mut ap) } == sys::ESP_OK).then_some(ap.rssi)
}

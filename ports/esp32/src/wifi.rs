//! Joining a Wi-Fi network as a station, and its signal strength; or a station on a channel of its own and no network,
//! for ESP-NOW.

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
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
    join_until(modem, sysloop, ssid, password, None)
}

/// [`join`], giving up once `within` has passed with no attempt joined (an attempt under way then still has its 10 s,
/// and there is always one): for firmware that can go on without the network, over Bluetooth say, when it isn't
/// there. Wi-Fi is off again when it gives up.
pub fn join_within(
    modem: Modem<'static>,
    sysloop: EspSystemEventLoop,
    ssid: &str,
    password: &str,
    within: Duration,
) -> anyhow::Result<Wifi> {
    join_until(modem, sysloop, ssid, password, Some(Instant::now() + within))
}

fn join_until(
    modem: Modem<'static>,
    sysloop: EspSystemEventLoop,
    ssid: &str,
    password: &str,
    until: Option<Instant>,
) -> anyhow::Result<Wifi> {
    let nvs = crate::nvs()?;
    let mut w = BlockingWifi::wrap(EspWifi::new(modem, sysloop.clone(), Some(nvs))?, sysloop.clone())?;
    w.set_configuration(&client(ssid, password)?)?;
    quiet();
    w.start()?;
    sys::esp!(unsafe { sys::esp_wifi_set_ps(sys::wifi_ps_type_t_WIFI_PS_NONE) })?;
    if let Err(e) = attempts(&mut w, &sysloop, ssid, until) {
        let _ = w.stop();
        return Err(e);
    }
    w.wait_netif_up()?;
    Ok(w)
}

/// Starts Wi-Fi as a station that joins no network, on `channel` (1 to 13), with modem sleep off: what ESP-NOW needs to
/// reach boards on the same channel with no access point (NB-135). [`connect`] joins a network later, and the radio
/// then follows that network's channel.
pub fn start_on_channel(modem: Modem<'static>, sysloop: EspSystemEventLoop, channel: u8) -> anyhow::Result<Wifi> {
    start(modem, sysloop, channel, None)
}

/// [`start_on_channel`] with the station's MAC address `mac` in place of the chip's own, such as a [`random_mac`]:
/// ESP-NOW frames carry the sender's address in the clear, so a board with the chip's fixed one can be followed from
/// boot to boot by anyone in range. A network the station joins sees `mac` too.
pub fn start_on_channel_as(
    modem: Modem<'static>,
    sysloop: EspSystemEventLoop,
    channel: u8,
    mac: [u8; 6],
) -> anyhow::Result<Wifi> {
    start(modem, sysloop, channel, Some(mac))
}

/// A new random MAC address, locally administered and unicast, for [`start_on_channel_as`]. Drawn like a new identity
/// ([`identity::load_or_create`](crate::identity::load_or_create)), from the SAR ADC's noise, so call it before Wi-Fi
/// or Bluetooth is set up: after, it is an error.
pub fn random_mac() -> anyhow::Result<[u8; 6]> {
    let mut mac = [0u8; 6];
    crate::identity::random(&mut mac)?;
    // Locally administered (bit 1), not a group address (bit 0).
    mac[0] = (mac[0] & 0xfc) | 0x02;
    Ok(mac)
}

fn start(
    modem: Modem<'static>,
    sysloop: EspSystemEventLoop,
    channel: u8,
    mac: Option<[u8; 6]>,
) -> anyhow::Result<Wifi> {
    let nvs = crate::nvs()?;
    let mut w = BlockingWifi::wrap(EspWifi::new(modem, sysloop.clone(), Some(nvs))?, sysloop)?;
    // An open network's auth method, or ESP-IDF warns that an empty password can't reach a protected one.
    let none = ClientConfiguration { auth_method: AuthMethod::None, ..Default::default() };
    w.set_configuration(&Configuration::Client(none))?;
    if let Some(mac) = mac {
        // Only while the station isn't started.
        sys::esp!(unsafe { sys::esp_wifi_set_mac(sys::wifi_interface_t_WIFI_IF_STA, mac.as_ptr()) })?;
    }
    quiet();
    w.start()?;
    sys::esp!(unsafe { sys::esp_wifi_set_ps(sys::wifi_ps_type_t_WIFI_PS_NONE) })?;
    set_channel(channel)?;
    info!("Wi-Fi started on channel {channel}, on no network");
    Ok(w)
}

/// Joins the Wi-Fi network `ssid` from a station that [`start_on_channel`] started, as [`join`] does, giving up once
/// `within` has passed with no attempt joined. Unlike [`join_within`], Wi-Fi stays on when it gives up, on no network:
/// [`set_channel`] puts it back on its channel. While it joins, the radio scans other channels.
pub fn connect(
    w: &mut Wifi,
    sysloop: &EspSystemEventLoop,
    ssid: &str,
    password: &str,
    within: Duration,
) -> anyhow::Result<()> {
    w.set_configuration(&client(ssid, password)?)?;
    attempts(w, sysloop, ssid, Some(Instant::now() + within))?;
    w.wait_netif_up()?;
    Ok(())
}

/// Moves a started station that is on no network to `channel` (1 to 13). ESP-IDF refuses while it is on one: the
/// radio is on the network's channel then.
pub fn set_channel(channel: u8) -> anyhow::Result<()> {
    if !(1..=13).contains(&channel) {
        return Err(anyhow!("channel {channel} isn't 1 to 13"));
    }
    sys::esp!(unsafe { sys::esp_wifi_set_channel(channel, sys::wifi_second_chan_t_WIFI_SECOND_CHAN_NONE) })?;
    Ok(())
}

/// The channel the radio is on, while Wi-Fi is started.
pub fn channel() -> Option<u8> {
    let (mut primary, mut second) = (0, 0);
    (unsafe { sys::esp_wifi_get_channel(&mut primary, &mut second) } == sys::ESP_OK).then_some(primary)
}

/// A station's configuration for the network `ssid`.
fn client(ssid: &str, password: &str) -> anyhow::Result<Configuration> {
    Ok(Configuration::Client(ClientConfiguration {
        ssid: ssid.try_into().map_err(|_| anyhow!("SSID over 32 bytes"))?,
        password: password.try_into().map_err(|_| anyhow!("password over 64 bytes"))?,
        auth_method: if password.is_empty() { AuthMethod::None } else { AuthMethod::WPA2Personal },
        ..Default::default()
    }))
}

/// ESP-IDF's Wi-Fi library logs under its own tag the network's name and its access point's address on each join
/// (`connected with …, bssid = …`), and those of other networks in range on some failures (NB110c-5). Its lines from
/// error up only: each failed attempt is logged here, with its reason. Set after ESP-IDF's own start-up setting, so it
/// holds.
fn quiet() {
    unsafe { sys::esp_log_level_set(c"wifi".as_ptr(), sys::esp_log_level_t_ESP_LOG_ERROR) };
}

/// Joins `ssid`, which the started station `w` is set up for, retrying until it has, or until `until` has passed with
/// no attempt joined (an attempt under way then still has its 10 s, and there is always one).
fn attempts(w: &mut Wifi, sysloop: &EspSystemEventLoop, ssid: &str, until: Option<Instant>) -> anyhow::Result<()> {
    // ESP-IDF's quarter-dBm.
    const TX_POWER: [i8; 4] = [80, 60, 44, 34];
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
        if attempt > 1 && until.is_some_and(|u| Instant::now() >= u) {
            return Err(anyhow!("not on the Wi-Fi network after {} attempts", attempt - 1));
        }
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
    Ok(())
}

/// The signal strength of the access point this station is joined to, in dBm; `None` when it isn't joined.
pub fn rssi() -> Option<i8> {
    let mut ap: sys::wifi_ap_record_t = unsafe { std::mem::zeroed() };
    (unsafe { sys::esp_wifi_sta_get_ap_info(&mut ap) } == sys::ESP_OK).then_some(ap.rssi)
}

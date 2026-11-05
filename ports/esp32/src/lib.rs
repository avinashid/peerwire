//! [Peerwire](https://avinashid.github.io/peerwire/) on the ESP32-C3 and the original ESP32, under ESP-IDF.
//!
//! The unchanged [`peerwire::Node`] runs on these chips. What a board needs around it is here: joining Wi-Fi,
//! setting the clock before the node binds, caps sized for a chip's ~250 KB of heap, and signaling on the LAN and,
//! with the `nostr` feature, over a Nostr relay.
//!
//! ```no_run
//! use peerwire::{Event, Lane};
//!
//! fn main() {
//!     peerwire_esp::run(|| {
//!         let board = peerwire_esp::Board::connect("my-wifi", "its password")?;
//!         let mut node = board.node()?;
//!         // `lan_chat`'s room "esp32": `cargo run -p peerwire --example lan_chat -- esp32` on a PC on the same
//!         // Wi-Fi links with the board.
//!         node.join_room("peerwire-lan-chat", "esp32");
//!         loop {
//!             node.poll();
//!             while let Some(event) = node.try_recv_event() {
//!                 if let Event::Message { peer, data, .. } = event {
//!                     let _ = node.send(&peer, Lane::Reliable, &data);
//!                 }
//!             }
//!             peerwire_esp::idle(&node);
//!         }
//!     })
//! }
//! ```
//!
//! `cargo generate --git https://github.com/avinashid/peerwire templates/esp32` makes a project ready to flash, and
//! the [ESP32 guide](https://avinashid.github.io/peerwire/docs/guides/esp32) has the steps.
//!
//! **Entropy.** Until Wi-Fi is started, ESP-IDF's random numbers are only pseudo-random, so a node built before it
//! draws a guessable identity and keys (NB-063). [`Board::connect`] starts Wi-Fi first; firmware that builds a
//! [`Node`] another way must do the same.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Once;
use std::time::Duration;

use anyhow::anyhow;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::sys;
use log::{error, info, warn};
use peerwire::{Builder, Config, LanSignaling, Node};
#[cfg(feature = "nostr")]
use peerwire::{MultiSignaling, NostrSignaling, Signaling};

#[cfg(feature = "ble")]
pub mod ble;
pub mod clock;
mod ntp;
pub mod wifi;

pub use peerwire;

use wifi::Wifi;

/// The UDP port [`Board::node`] binds.
pub const PORT: u16 = 4040;

/// One of [`NostrSignaling`]'s default relays, so PCs on the defaults share it with the board.
#[cfg(feature = "nostr")]
pub const NOSTR_RELAY: &str = "wss://nostr.mom";

/// How long [`Board::connect`] waits for the clock.
const CLOCK_WAIT: Duration = Duration::from_secs(60);

/// The longest [`idle`] sleeps.
const IDLE_MAX: Duration = Duration::from_millis(10);

/// Applies ESP-IDF's patches to the Rust runtime and starts its logger. [`run`] calls it; call it first if you don't
/// use `run`. A second call does nothing.
pub fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        sys::link_patches();
        esp_idf_svc::log::EspLogger::initialize_default();
    });
}

/// Runs `app` after [`init`]. If it fails, or returns, this logs why and restarts the chip 5 s later, rather than
/// leave a board idle: one that joined Wi-Fi but got no address by DHCP stayed down until it was power-cycled
/// (NB-095).
pub fn run(app: impl FnOnce() -> anyhow::Result<()>) -> ! {
    init();
    let e = app().err().unwrap_or_else(|| anyhow!("the app returned"));
    error!("{e:#}; restarting in 5 s");
    std::thread::sleep(Duration::from_secs(5));
    unsafe { sys::esp_restart() }
}

/// A board on Wi-Fi with its clock set: what a [`Node`] needs to bind.
pub struct Board {
    wifi: Wifi,
    ip: Ipv4Addr,
    prefix: u8,
}

impl Board {
    /// Joins the Wi-Fi network `ssid` ([`wifi::join`]), sets the clock ([`clock::sync`], waiting up to 60 s) and
    /// keeps it in step ([`clock::keep_in_step`]). It takes the chip's peripherals and system event loop: firmware
    /// that needs others of them passes the modem and the loop to [`connect_with`](Board::connect_with) instead.
    ///
    /// A clock no server set is a warning, not an error: the clock is asked for again every minute, and until it is
    /// set a Nostr relay's certificate can't be checked. A node bound meanwhile keeps 1970, so peers with a set clock
    /// (any PC) drop its hellos: once an ask is answered, the board restarts to bind again ([`clock::keep_in_step`]).
    pub fn connect(ssid: &str, password: &str) -> anyhow::Result<Board> {
        let peripherals = Peripherals::take()?;
        Board::connect_with(peripherals.modem, EspSystemEventLoop::take()?, ssid, password)
    }

    /// [`connect`](Board::connect) with a modem and system event loop the firmware has taken.
    pub fn connect_with(
        modem: Modem<'static>,
        sysloop: EspSystemEventLoop,
        ssid: &str,
        password: &str,
    ) -> anyhow::Result<Board> {
        let wifi = wifi::join(modem, sysloop, ssid, password)?;
        let ip_info = wifi.wifi().sta_netif().get_ip_info()?;
        let (ip, prefix) = (ip_info.ip, ip_info.subnet.mask.0);
        info!("on Wi-Fi at {ip}/{prefix}, free heap {} B", free_heap());
        match clock::sync(CLOCK_WAIT) {
            // The time a server gave before a restart is kept in RTC memory.
            Err(e) if clock::is_set() => warn!("{e}: keeping the time set before the restart"),
            Err(e) => {
                warn!("{e}: the clock is at 1970, so peers with a set clock drop our hellos; we restart once it is set")
            }
            Ok(()) => {}
        }
        clock::keep_in_step()?;
        Ok(Board { wifi, ip, prefix })
    }

    /// This board's address on the Wi-Fi network.
    pub fn ip(&self) -> Ipv4Addr {
        self.ip
    }

    /// The network's prefix length (24 for a /24).
    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    /// The address [`node`](Board::node) binds: [`ip`](Board::ip) on [`PORT`]. Not 0.0.0.0: ESP-IDF can't list
    /// interfaces, so a node bound to any interface would offer only loopback.
    pub fn addr(&self) -> SocketAddr {
        SocketAddr::from((self.ip, PORT))
    }

    /// The Wi-Fi station.
    pub fn wifi(&self) -> &Wifi {
        &self.wifi
    }

    /// The LAN carrier on this board's network: [`lan_signaling`].
    pub fn lan(&self) -> io::Result<LanSignaling> {
        lan_signaling(self.ip, self.prefix)
    }

    /// A node bound to [`addr`](Board::addr), with [`config`], no STUN servers and [`LanSignaling`]: it links with
    /// the peers in its rooms on the same network.
    pub fn node(&self) -> io::Result<Node> {
        self.builder()?.bind(self.addr())
    }

    /// [`node`](Board::node)'s builder, to change before binding it to [`addr`](Board::addr): to give the node an
    /// identity of its own, say.
    pub fn builder(&self) -> io::Result<Builder> {
        Ok(Node::builder().config(config()).stun_servers(Vec::<String>::new()).signaling(self.lan()?))
    }

    /// A node like [`node`](Board::node)'s that also finds its rooms' peers over the Nostr relay `relay`
    /// ([`NOSTR_RELAY`], say), so peers anywhere on the internet that use that relay link with it. It asks the
    /// default STUN servers for the router's public address, to offer to peers beyond the LAN.
    ///
    /// Anyone who knows a room's name can link with the board from anywhere, and learn the home's public address and
    /// the board's LAN address: name rooms others can't guess.
    #[cfg(feature = "nostr")]
    pub fn node_with_nostr(&self, relay: &str) -> anyhow::Result<Node> {
        Ok(self.builder_with_nostr(relay)?.bind(self.addr())?)
    }

    /// [`node_with_nostr`](Board::node_with_nostr)'s builder.
    #[cfg(feature = "nostr")]
    pub fn builder_with_nostr(&self, relay: &str) -> anyhow::Result<Builder> {
        let carriers: Vec<Box<dyn Signaling>> = vec![Box::new(self.lan()?), Box::new(nostr_signaling(relay)?)];
        Ok(Node::builder().config(config()).signaling(MultiSignaling::new(carriers)))
    }
}

/// Caps sized for a chip with ~250 KB of free heap: 16 KiB messages, a 32 KiB send buffer, 8 links and 8 handshakes
/// at once. The defaults allow 1 MiB messages and buffers.
pub fn config() -> Config {
    Config {
        max_message: 16 * 1024,
        max_send_buffer: 32 * 1024,
        max_links: 8,
        max_pending_handshakes: 8,
        ..Config::default()
    }
}

/// The LAN carrier on the address `ip` of a network with prefix length `prefix`: it sends to the multicast group and
/// to the network's broadcast address too, as a home router in NB-091 never delivered multicast to the board but
/// passed broadcast both ways (NB-092). ESP-IDF can't list interfaces, so [`LanSignaling`] can't find them itself.
pub fn lan_signaling(ip: Ipv4Addr, prefix: u8) -> io::Result<LanSignaling> {
    let lan = LanSignaling::on(LanSignaling::GROUP, ip)?;
    // A /31 or /32 has no broadcast address: below, it would give a host's address, the chip's own on a /32, which
    // `broadcast` refuses (NB92-1).
    if prefix >= 31 {
        info!("LAN signals to {} only, on a /{prefix}", LanSignaling::GROUP.ip());
        return Ok(lan);
    }
    let broadcast = Ipv4Addr::from(u32::from(ip) | u32::MAX >> prefix);
    info!("LAN signals to {} and {broadcast}", LanSignaling::GROUP.ip());
    lan.broadcast(broadcast)
}

/// The name of [`nostr_signaling`]'s relay thread, which [`nostr_thread`] looks up.
#[cfg(feature = "nostr")]
const NOSTR_THREAD: &std::ffi::CStr = c"nostr";

/// A carrier over the Nostr relay `relay`, with its thread at the main task's priority, 1. ESP-IDF's default for a
/// thread is 5, and on the C3's one core a relay streaming events at it would hold off the node's loop: keepalives,
/// retransmits and polls (NB95-3). Each relay costs a thread (a 32 KB stack) and a TLS session, so a board uses one.
#[cfg(feature = "nostr")]
pub fn nostr_signaling(relay: &str) -> anyhow::Result<NostrSignaling> {
    use esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration;
    // ESP-IDF takes the priority from this config even where std sets the stack.
    ThreadSpawnConfiguration { name: Some(NOSTR_THREAD), priority: 1, ..Default::default() }.set()?;
    let nostr = NostrSignaling::new([relay]);
    ThreadSpawnConfiguration::default().set()?;
    Ok(nostr)
}

/// The stack [`nostr_signaling`]'s relay thread has never used, and its priority, while it runs.
#[cfg(feature = "nostr")]
pub fn nostr_thread() -> Option<(u32, u32)> {
    let task = unsafe { sys::xTaskGetHandle(NOSTR_THREAD.as_ptr()) };
    (!task.is_null()).then(|| unsafe { (sys::uxTaskGetStackHighWaterMark(task), sys::uxTaskPriorityGet(task)) })
}

/// The heap free now, in bytes.
pub fn free_heap() -> u32 {
    unsafe { sys::esp_get_free_heap_size() }
}

/// Sleeps until `node`'s next timer, or 10 ms at most, so packets that arrive meanwhile wait no longer: call it at the
/// end of each pass of the loop that polls the node.
pub fn idle(node: &Node) {
    std::thread::sleep(node.next_timeout().unwrap_or(IDLE_MAX).min(IDLE_MAX));
}

// The README's sample is a doc test too (`cargo test --doc` for the chip's target).
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

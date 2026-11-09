//! The clock: set by SNTP before a node binds, and kept in step after.
//!
//! Room hellos and offers carry the sender's clock, and a node drops those more than a minute off its own (a replay
//! guard). A board has no battery-backed clock and starts at 1970, so a board and a PC on the same LAN drop each
//! other's hellos and never link (NB-091). [`sync`] sets the clock before the node is bound, and [`keep_in_step`]
//! asks again every [`RESYNC`].
//!
//! A bound node reads the clock once and counts on from it, so the re-asks keep the relay carrier's event times and
//! TLS right, not the node's, and can't move its replay guard. If no server answered before the node was bound, it
//! keeps 1970: so when a later ask sets the clock, [`keep_in_step`] restarts the chip, to bind again (NB103-I2).
//!
//! With no network at all (no Wi-Fi saved, or none in range), a node in a room over Bluetooth takes the time from the
//! first member that proves the room to it ([`Node::clock_from_peer`](crate::peerwire::Node::clock_from_peer)), and
//! [`set`] sets the clock from that.
//!
//! The asks are this module's own, not lwIP's SNTP client, which takes any answer that reaches its port, so a device
//! on the LAN could step the clock (NB95-I2). Each ask carries 8 random bytes that the answer must echo, and an
//! answer from a server with no time to give (unsynchronised, or a kiss-o'-death) is refused.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration;
use esp_idf_svc::sys;
use log::{info, warn};

use crate::ntp::{self, Refused};

/// The servers asked, in turn.
pub const SERVERS: [&str; 4] = ["0.pool.ntp.org", "1.pool.ntp.org", "2.pool.ntp.org", "3.pool.ntp.org"];

/// How often [`keep_in_step`] asks again once the clock is set: a board drifts about a second a day, and the relay
/// carrier reads the clock for its event times and TLS. A bound node doesn't: it read the clock once, when bound.
pub const RESYNC: Duration = Duration::from_secs(6 * 3600);

/// How often [`keep_in_step`] asks again while the clock isn't set.
const RETRY: Duration = Duration::from_secs(60);

/// How long each server gets to answer. lwIP's own client gives one 15 s and can't be told less; on a board it took
/// 15 s on one boot and over 50 s, silent, on two others (NB-095 run E).
const WAIT: Duration = Duration::from_secs(2);

/// The keeper thread's stack: DNS, one socket and a log line.
const STACK: usize = 6 * 1024;

/// The node has the time: a room member gave it ([`set`]), so a late SNTP answer needs no restart.
static NODE_HAS_TIME: AtomicBool = AtomicBool::new(false);

/// Whether the clock has been set: one before 2023-11-14 never was.
pub fn is_set() -> bool {
    SystemTime::now().duration_since(UNIX_EPOCH).is_ok_and(|d| d.as_secs() >= ntp::SET_AFTER)
}

/// Sets the clock to `time`, the time a room member gave the node over Bluetooth
/// ([`Node::clock_from_peer`](crate::peerwire::Node::clock_from_peer)) on a board with no network to ask: what else
/// reads the clock (TLS, the relay carrier's events, the next bind) then has it too, and the RTC keeps it across a
/// restart, not a power cut. A late SNTP answer then sets the clock without restarting the chip
/// ([`keep_in_step`]): the node has the time already.
pub fn set(time: SystemTime) -> io::Result<()> {
    let now = time.duration_since(UNIX_EPOCH).map_err(|_| io::Error::other("a time before 1970"))?;
    let tv = sys::timeval { tv_sec: now.as_secs() as _, tv_usec: now.subsec_micros() as _ };
    if unsafe { sys::settimeofday(&tv, std::ptr::null()) } != 0 {
        return Err(io::Error::other("settimeofday failed"));
    }
    NODE_HAS_TIME.store(true, Ordering::Relaxed);
    info!("clock set from a room member: {} s", now.as_secs());
    Ok(())
}

/// Sets the clock from the first of [`SERVERS`] to answer, giving each 2 s in turn, for up to `timeout`. Call it
/// once the network is up and before binding a node: the node reads the clock when it is bound. The error says why
/// the last ask failed.
pub fn sync(timeout: Duration) -> io::Result<()> {
    let start = Instant::now();
    let mut last = String::from("no server asked");
    for server in SERVERS.iter().cycle() {
        if start.elapsed() >= timeout {
            break;
        }
        let next = Instant::now() + WAIT;
        match ask(server) {
            Ok((from, now)) => {
                let tv = sys::timeval { tv_sec: now.as_secs() as _, tv_usec: now.subsec_micros() as _ };
                if unsafe { sys::settimeofday(&tv, std::ptr::null()) } != 0 {
                    return Err(io::Error::other("settimeofday failed"));
                }
                info!("clock set by {server} ({from}) in {} ms: {} s", start.elapsed().as_millis(), now.as_secs());
                return Ok(());
            }
            Err(e) => {
                warn!("SNTP {server}: {e}");
                last = e.to_string();
            }
        }
        // A failed ask waits out its 2 s, so a DNS error doesn't spin.
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    let secs = timeout.as_secs();
    Err(io::Error::new(io::ErrorKind::TimedOut, format!("no SNTP answer in {secs} s (the last ask: {last})")))
}

/// Starts a thread that keeps the clock in step: it asks again every [`RESYNC`], or every minute while the clock
/// isn't set. It runs at the main task's priority, 1. A second call does nothing.
///
/// If the clock isn't set when it starts, the first answer sets it and restarts the chip 5 s later, as a node bound
/// meanwhile keeps 1970 and peers with a set clock drop its hellos (NB103-I2). The next boot asks before binding. Not
/// once a room member gave the node the time ([`set`]).
pub fn keep_in_step() -> io::Result<()> {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::Relaxed) {
        return Ok(());
    }
    let set_late = !is_set();
    // ESP-IDF takes the task's name and priority from this config; std sets the stack.
    ThreadSpawnConfiguration { name: Some(c"sntp"), priority: 1, ..Default::default() }
        .set()
        .map_err(io::Error::other)?;
    let spawned = std::thread::Builder::new().stack_size(STACK).spawn(move || loop {
        std::thread::sleep(if is_set() { RESYNC } else { RETRY });
        match sync(Duration::from_secs(30)) {
            Ok(()) if set_late && !NODE_HAS_TIME.load(Ordering::Relaxed) => {
                warn!("clock: set late; restarting in 5 s, so the node binds with the time and not 1970");
                std::thread::sleep(Duration::from_secs(5));
                unsafe { sys::esp_restart() }
            }
            Ok(()) => {}
            Err(e) => warn!("clock: {e}"),
        }
        let unused = unsafe { sys::uxTaskGetStackHighWaterMark(std::ptr::null_mut()) };
        info!("clock: SNTP thread stack unused {unused} B");
    });
    ThreadSpawnConfiguration::default().set().map_err(io::Error::other)?;
    spawned.map(drop).inspect_err(|_| STARTED.store(false, Ordering::Relaxed))
}

/// Asks `server` the time once, waiting up to 2 s: its address, and the time since 1970 with no allowance for the
/// trip (the replay guard allows a minute).
fn ask(server: &str) -> io::Result<(SocketAddr, Duration)> {
    let to = (server, 123)
        .to_socket_addrs()
        .map_err(|e| io::Error::new(e.kind(), format!("DNS: {e}")))?
        .find(SocketAddr::is_ipv4)
        .ok_or_else(|| io::Error::other("DNS: no IPv4 address"))?;
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    let mut token = [0u8; 8];
    unsafe { sys::esp_fill_random(token.as_mut_ptr().cast(), token.len()) };
    sock.send_to(&ntp::ask(token), to)?;
    let end = Instant::now() + WAIT;
    let mut buf = [0u8; 64];
    loop {
        let left = end.saturating_duration_since(Instant::now());
        // lwIP counts a read timeout in whole ms and takes 0 as none, so under 1 ms left would wait for ever (NB-117).
        if left < Duration::from_millis(1) {
            return Err(io::Error::new(io::ErrorKind::TimedOut, format!("no answer from {to} in 2 s")));
        }
        sock.set_read_timeout(Some(left))?;
        let (len, from) = match sock.recv_from(&mut buf) {
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => continue,
            r => r?,
        };
        if from != to {
            continue;
        }
        match ntp::time(&buf[..len], &token) {
            Ok(now) => return Ok((to, now)),
            Err(Refused::NotOurs) => continue,
            Err(Refused::NoTime(why)) => return Err(io::Error::other(format!("{to}: {why}"))),
        }
    }
}

//! The clock: set by SNTP before a node binds, and kept in step after.
//!
//! Room hellos and offers carry the sender's clock, and a node drops those more than a minute off its own (a replay
//! guard). A board has no battery-backed clock and starts at 1970, so a board and a PC on the same LAN drop each
//! other's hellos and never link (NB-091). [`sync`] sets the clock before the node is bound, and [`keep_in_step`]
//! asks again every [`RESYNC`].
//!
//! A bound node reads the clock once and counts on from it, so the re-asks keep the relay carrier's event times and
//! TLS right, not the node's, and can't move its replay guard. If no server answered before the node was bound, it
//! keeps 1970: so when a later ask moves the clock more than 30 s, [`keep_in_step`] restarts the chip, to bind again
//! (NB103-I2).
//!
//! With no network at all (no Wi-Fi saved, or none in range), a node in a room over Bluetooth takes the time from the
//! first member that proves the room to it ([`Node::clock_from_peer`](crate::peerwire::Node::clock_from_peer)), and
//! [`set`] sets the clock from that. A member may give a wrong time, so it lasts one boot at most: the RTC would keep
//! it across a restart, but [`init`](crate::init) sets it back to 1970 at the next boot unless SNTP has set the clock
//! since, and a late SNTP answer more than 30 s off it restarts the chip (NB119-2).
//!
//! The asks are this module's own, not lwIP's SNTP client, which takes any answer that reaches its port, so a device
//! on the LAN could step the clock (NB95-I2). Each ask carries 8 random bytes that the answer must echo, and an
//! answer from a server with no time to give (unsynchronised, or a kiss-o'-death) is refused.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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

/// How often [`keep_in_step`] asks again while the clock isn't set, or has a time a room member gave.
const RETRY: Duration = Duration::from_secs(60);

/// How long each server gets to answer. lwIP's own client gives one 15 s and can't be told less; on a board it took
/// 15 s on one boot and over 50 s, silent, on two others (NB-095 run E).
const WAIT: Duration = Duration::from_secs(2);

/// The keeper thread's stack: DNS, one socket and a log line.
const STACK: usize = 6 * 1024;

/// A late SNTP answer that moves the clock more than this restarts the chip ([`keep_in_step`]): half the minute a room
/// hello may be off.
const STEP_MAX: Duration = Duration::from_secs(30);

/// [`MEMBER_MARK`] while the clock has a time a room member gave ([`set`]), not SNTP. It is in RTC memory that a
/// restart keeps and boot leaves as it was, as the clock is; a power cut leaves it random, and the clock at 1970.
#[link_section = ".rtc_noinit.peerwire_clock"]
static MEMBER_TIME: AtomicU32 = AtomicU32::new(0);
const MEMBER_MARK: u32 = 0x5057_0119;

/// Whether the clock has been set: one before 2023-11-14 never was.
pub fn is_set() -> bool {
    SystemTime::now().duration_since(UNIX_EPOCH).is_ok_and(|d| d.as_secs() >= ntp::SET_AFTER)
}

/// Sets the clock to `time`, the time a room member gave the node over Bluetooth
/// ([`Node::clock_from_peer`](crate::peerwire::Node::clock_from_peer)) on a board with no network to ask: what else
/// reads the clock (TLS, the relay carrier's events) then has it too. For this boot only: [`init`](crate::init)
/// sets it back to 1970 at the next, unless SNTP has set the clock since, so the node bound then takes the time
/// afresh. A late SNTP answer more than 30 s off it restarts the chip ([`keep_in_step`]).
pub fn set(time: SystemTime) -> io::Result<()> {
    let now = time.duration_since(UNIX_EPOCH).map_err(|_| io::Error::other("a time before 1970"))?;
    settimeofday(now)?;
    MEMBER_TIME.store(MEMBER_MARK, Ordering::Relaxed);
    info!("clock set from a room member: {} s", now.as_secs());
    Ok(())
}

/// At boot, before a node is bound: if the clock has a time a room member gave before the restart ([`set`]), sets it
/// back to 1970, so the node takes the time afresh, from SNTP or a member (NB119-2). [`init`](crate::init) calls it.
pub(crate) fn forget_member_time() {
    // A load and a store, no swap: on the classic ESP32 the mark is in RTC slow memory, which may not take the atomic
    // read-modify-write (S32C1I). Nothing else runs this early.
    let marked = member_time();
    MEMBER_TIME.store(0, Ordering::Relaxed);
    if !marked {
        return;
    }
    match settimeofday(Duration::ZERO) {
        Ok(()) => info!("clock: forgot the time a room member gave before the restart; at 1970 until it is set"),
        Err(e) => warn!("clock: the time a room member gave before the restart: {e}"),
    }
}

/// Whether the clock has a time a room member gave ([`set`]), and SNTP hasn't set it since.
fn member_time() -> bool {
    MEMBER_TIME.load(Ordering::Relaxed) == MEMBER_MARK
}

fn settimeofday(since_1970: Duration) -> io::Result<()> {
    let tv = sys::timeval { tv_sec: since_1970.as_secs() as _, tv_usec: since_1970.subsec_micros() as _ };
    if unsafe { sys::settimeofday(&tv, std::ptr::null()) } != 0 {
        return Err(io::Error::other("settimeofday failed"));
    }
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
                settimeofday(now)?;
                MEMBER_TIME.store(0, Ordering::Relaxed);
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
/// isn't set or has a time a room member gave (NB119-I4). It runs at the main task's priority, 1. A second call does
/// nothing.
///
/// An answer that moves the clock more than 30 s restarts the chip 5 s later, as a node bound before keeps the time
/// it had, and peers drop its hellos: 1970, if no server answered before it was bound (NB103-I2), or a wrong time a
/// room member gave ([`set`], NB119-2). The next boot asks before binding.
pub fn keep_in_step() -> io::Result<()> {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::Relaxed) {
        return Ok(());
    }
    // ESP-IDF takes the task's name and priority from this config; std sets the stack.
    ThreadSpawnConfiguration { name: Some(c"sntp"), priority: 1, ..Default::default() }
        .set()
        .map_err(io::Error::other)?;
    let spawned = std::thread::Builder::new().stack_size(STACK).spawn(move || loop {
        std::thread::sleep(if is_set() && !member_time() { RESYNC } else { RETRY });
        let (wall, at) = (SystemTime::now(), Instant::now());
        match sync(Duration::from_secs(30)) {
            Ok(()) => {
                // The clock now, against what it would read had the answer left it alone.
                let kept = wall + at.elapsed();
                let step = SystemTime::now().duration_since(kept).unwrap_or_else(|e| e.duration());
                if step > STEP_MAX {
                    let secs = step.as_secs();
                    warn!("clock: SNTP moved it {secs} s; restarting in 5 s, so the node binds with the time");
                    std::thread::sleep(Duration::from_secs(5));
                    unsafe { sys::esp_restart() }
                }
            }
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

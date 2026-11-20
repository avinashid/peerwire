//! Peerwire Share from a terminal: send files to the peers of a room, or receive them into a directory, in the
//! protocol of the Share page (`packages/js/demo/share/`): pieces of 512 KiB, each checked with SHA-256, the whole
//! file checked again at the end, read from and written to disk a piece at a time, over direct links only.
//!
//! ```sh
//! cargo run --release -p peerwire --features files,nostr --example share -- send <room> <file>... [--stay]
//! cargo run --release -p peerwire --features files,nostr --example share -- recv <room> <dir> [--yes] [--once]
//! ```
//!
//! Copies in the same room find each other on the LAN (`LanSignaling`) and, with the `nostr` feature, over Nostr
//! relays too, as `room_chat` does; `PW_RELAYS` (relay URLs, comma-separated) replaces the default relays:
//! `ws://127.0.0.1:7447` for the `nostr_relay` example, with no internet. Anyone who guesses the room's name can join
//! it, so pick one others can't guess.
//!
//! - `send` offers each file to every peer that joins, and ends once every offer is answered and every file it sent
//!   is verified by its receiver (`--stay` keeps offering to peers that join later). Its exit code is 0 if every one
//!   was delivered.
//! - `recv` asks before taking each file (`--yes` takes them all), keeps it under its cleaned name only once its
//!   SHA-256 matches, never replacing a file (`name (2).ext` instead), and runs until stopped (`--once`: until its
//!   first file ends, with exit code 0 if it was kept).
//!
//! The room may also be `tcp://<host:port>`: the room's messages then go over a TCP connection to that address, each
//! as its length (4 bytes, big-endian) and the message, to the one peer at the other end. That is how
//! `packages/js/test/share_native.mjs` puts this example and the Share page in a browser in one room, as a browser
//! and a native node can't link directly yet.

use peerwire::files::{self, End, Event as FileEvent, Files, State, TransferId, Wire};
use peerwire::{DisconnectReason, Event, LanSignaling, Lane, MultiSignaling, Node, PeerId, SendError, Signaling};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const APP: &str = "peerwire-share"; // the Share page's app id
const USAGE: &str = "usage: share send <room> <file>... [--stay] | share recv <room> <dir> [--yes] [--once]";

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("share: {e}");
            std::process::exit(2);
        }
    }
}

struct Opts {
    send: bool,
    room: String,
    paths: Vec<PathBuf>,
    yes: bool,
    once: bool,
    stay: bool,
}

fn opts() -> Result<Opts, String> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |args: &mut Vec<String>, f: &str| {
        let had = args.iter().any(|a| a == f);
        args.retain(|a| a != f);
        had
    };
    let (yes, once, stay) = (flag(&mut args, "--yes"), flag(&mut args, "--once"), flag(&mut args, "--stay"));
    let mut args = args.into_iter();
    let send = match args.next().as_deref() {
        Some("send") => true,
        Some("recv") => false,
        _ => return Err(USAGE.into()),
    };
    let room = args.next().ok_or(USAGE)?;
    let paths: Vec<PathBuf> = args.map(PathBuf::from).collect();
    if paths.is_empty() || (!send && paths.len() != 1) {
        return Err(USAGE.into());
    }
    for p in &paths {
        let ok = if send { p.is_file() } else { p.is_dir() };
        if !ok {
            return Err(format!("{}: not a {}", p.display(), if send { "file" } else { "directory" }));
        }
    }
    Ok(Opts { send, room, paths, yes, once, stay })
}

/// Where the room's messages go: a node in a room, or a TCP bridge to one peer.
enum Link {
    Room(Box<Node>),
    Bridge(Bridge),
}

fn run() -> Result<i32, Box<dyn std::error::Error>> {
    let o = opts()?;
    let mut link = match o.room.strip_prefix("tcp://") {
        Some(addr) => Link::Bridge(Bridge::connect(addr)?),
        None => {
            let mut carriers: Vec<Box<dyn Signaling>> = vec![Box::new(LanSignaling::new()?)];
            #[cfg(feature = "nostr")]
            carriers.push(Box::new(match std::env::var("PW_RELAYS") {
                Ok(urls) => peerwire::NostrSignaling::new(urls.split(',').map(str::trim).filter(|u| !u.is_empty())),
                Err(_) => peerwire::NostrSignaling::default(),
            }));
            let mut node = Node::builder().signaling(MultiSignaling::new(carriers)).bind("0.0.0.0:0")?;
            node.join_room(APP, &o.room);
            println!("{} in room {:?}: waiting for peers", node.peer_id(), o.room);
            Link::Room(Box::new(node))
        }
    };

    let mut files = Files::new();
    let mut offered: HashSet<PeerId> = HashSet::new();
    let mut started: HashMap<TransferId, Instant> = HashMap::new();
    let mut asking: VecDeque<TransferId> = VecDeque::new();
    let mut outcome: Option<bool> = None; // all delivered (send), or the first file kept (recv --once)
    let mut failures = 0;
    let mut finished_at: Option<Instant> = None;
    let mut last_report = Instant::now();
    let (lines, typed) = mpsc::channel();
    if !o.send && !o.yes {
        std::thread::spawn(move || {
            for line in std::io::stdin().lines().map_while(Result::ok) {
                if lines.send(line).is_err() {
                    break;
                }
            }
        });
    }

    loop {
        let now = Instant::now();
        // The link's events, Share messages to `files`.
        let mut joined = Vec::new();
        match &mut link {
            Link::Room(node) => {
                node.poll();
                while let Some(e) = node.try_recv_event() {
                    if files.handle_event(now, &e) {
                        continue;
                    }
                    match e {
                        Event::Connected { peer, path, .. } => {
                            println!("+ {peer} ({path:?})");
                            joined.push(peer);
                        }
                        Event::Disconnected { peer, reason } => println!("- {peer} ({reason:?})"),
                        _ => {}
                    }
                }
            }
            Link::Bridge(b) => {
                if !b.up {
                    b.up = true;
                    files.connected(Bridge::PEER);
                    joined.push(Bridge::PEER);
                }
                for m in b.read()? {
                    files.receive(Bridge::PEER, Lane::Reliable, &m);
                }
                if b.closed && !b.gone {
                    b.gone = true;
                    files.disconnected(now, Bridge::PEER, DisconnectReason::ClosedByPeer);
                }
            }
        }
        if o.send {
            for peer in joined {
                if offered.insert(peer) && outcome.is_none() {
                    for p in &o.paths {
                        let id = files.offer_file(peer, p)?;
                        println!("offered {} to {peer} ({id:?})", p.display());
                    }
                }
            }
        }
        match &mut link {
            Link::Room(node) => files.poll(now, node.as_mut()),
            Link::Bridge(b) => {
                files.poll(now, b);
                b.flush()?;
            }
        }

        while let Some(e) = files.poll_event() {
            match e {
                FileEvent::Offer { id, peer, name, size, mime } => {
                    let mime = if mime.is_empty() { String::new() } else { format!(", {mime}") };
                    println!("{peer} offers {name:?} ({}{mime}) [{id:?}]", bytes(size));
                    if o.send {
                        files.decline(id)?;
                    } else if o.yes {
                        files.accept_into(id, &o.paths[0])?;
                        started.insert(id, now);
                    } else {
                        println!("accept {name:?}? [y/N]");
                        asking.push_back(id);
                    }
                }
                FileEvent::Accepted { id } => {
                    started.insert(id, now);
                    println!("accepted [{id:?}]: sending");
                }
                FileEvent::Resumed { id, from } => println!("[{id:?}] resumes from piece {}", from + 1),
                FileEvent::Progress { .. } => {}
                FileEvent::Ended { id, end } => {
                    let info = files.info(id).expect("kept");
                    let secs = started.get(&id).map(|t| now.duration_since(*t).as_secs_f64());
                    let speed = match secs {
                        Some(s) if s > 0.0 => format!(" in {s:.1} s, {:.1} MB/s", info.size as f64 / 1e6 / s),
                        _ => String::new(),
                    };
                    let ok = matches!(end, End::Done { .. });
                    match &end {
                        End::Done { sha256 } => println!(
                            "done {:?}: {}{speed}, SHA-256 {sha256}{}",
                            info.name,
                            bytes(info.size),
                            info.saved_as.map(|p| format!(", saved as {}", p.display())).unwrap_or_default()
                        ),
                        End::Declined => println!("{:?} was declined", info.name),
                        End::Cancelled(why) | End::Failed(why) => println!("{:?} ended: {why}", info.name),
                    }
                    if !ok {
                        failures += 1;
                    }
                    if !o.send && o.once && outcome.is_none() {
                        outcome = Some(ok);
                    }
                }
            }
        }
        while let Ok(line) = typed.try_recv() {
            if let Some(id) = asking.pop_front() {
                if matches!(line.trim(), "y" | "Y" | "yes") {
                    files.accept_into(id, &o.paths[0])?;
                    started.insert(id, Instant::now());
                } else {
                    files.decline(id)?;
                }
            }
        }

        let live: Vec<_> = files.transfers().filter(|id| files.info(*id).is_some_and(|i| i.state.is_live())).collect();
        if o.send && !o.stay && !offered.is_empty() && live.is_empty() && outcome.is_none() {
            outcome = Some(failures == 0);
        }
        if now.duration_since(last_report) >= Duration::from_secs(1) {
            last_report = now;
            for id in &live {
                let i = files.info(*id).expect("live");
                if matches!(i.state, State::Sending | State::Receiving) {
                    let rate = started.get(id).map(|t| i.bytes as f64 / 1e6 / now.duration_since(*t).as_secs_f64());
                    let wait = match (&link, i.peer_gone) {
                        (_, true) => " (the peer is gone: waiting for it)",
                        (Link::Room(node), _) if !Wire::carries_files(node.as_ref(), &i.peer) => {
                            " (waiting for a direct link)"
                        }
                        _ => "",
                    };
                    println!(
                        "  {:?}: {:.1}% of {}, {:.1} MB/s{wait}",
                        i.name,
                        100.0 * i.bytes as f64 / i.size.max(1) as f64,
                        bytes(i.size),
                        rate.unwrap_or(0.0)
                    );
                }
            }
        }
        // Once done, a moment more for the last messages (a receiver's verdict) to go.
        if let Some(ok) = outcome {
            let at = *finished_at.get_or_insert(now);
            if now.duration_since(at) >= Duration::from_secs(2) {
                return Ok(if ok { 0 } else { 1 });
            }
        }
        match &link {
            // A transfer under way moves as fast as the loop turns.
            _ if !live.is_empty() => std::thread::yield_now(),
            Link::Room(node) => {
                std::thread::sleep(node.next_timeout().unwrap_or(Duration::MAX).min(Duration::from_millis(5)))
            }
            Link::Bridge(_) => std::thread::sleep(Duration::from_millis(2)),
        }
    }
}

fn bytes(n: u64) -> String {
    match n {
        n if n < 1000 => format!("{n} B"),
        n if n < 1_000_000 => format!("{:.1} kB", n as f64 / 1e3),
        n if n < 1_000_000_000 => format!("{:.1} MB", n as f64 / 1e6),
        n => format!("{:.2} GB", n as f64 / 1e9),
    }
}

/// The room's messages over TCP to one peer: each a u32 big-endian length, then the message. Holds at most `MAX_OUT`
/// bytes unsent, as a node's reliable lane holds at most its send buffer: past that, `WouldBlock`.
struct Bridge {
    tcp: TcpStream,
    out: Vec<u8>,
    sent: usize,
    inbuf: Vec<u8>,
    up: bool,
    closed: bool,
    gone: bool,
}

impl Bridge {
    const PEER: PeerId = PeerId([0xbb; 32]);
    const MAX_OUT: usize = 4 << 20;

    fn connect(addr: &str) -> std::io::Result<Bridge> {
        let tcp = TcpStream::connect(addr)?;
        tcp.set_nodelay(true)?;
        tcp.set_nonblocking(true)?;
        println!("bridged to {addr}");
        Ok(Bridge { tcp, out: Vec::new(), sent: 0, inbuf: Vec::new(), up: false, closed: false, gone: false })
    }

    fn flush(&mut self) -> std::io::Result<()> {
        while self.sent < self.out.len() {
            match self.tcp.write(&self.out[self.sent..]) {
                Ok(0) => return Err(ErrorKind::WriteZero.into()),
                Ok(n) => self.sent += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        if self.sent == self.out.len() {
            self.out.clear();
            self.sent = 0;
        }
        Ok(())
    }

    /// The whole messages that came in.
    fn read(&mut self) -> std::io::Result<Vec<Vec<u8>>> {
        let mut buf = [0u8; 64 * 1024];
        loop {
            match self.tcp.read(&mut buf) {
                Ok(0) => {
                    self.closed = true;
                    break;
                }
                Ok(n) => self.inbuf.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        let mut msgs = Vec::new();
        let mut at = 0;
        while self.inbuf.len() - at >= 4 {
            let len = u32::from_be_bytes(self.inbuf[at..at + 4].try_into().expect("4 bytes")) as usize;
            if len > files::frame::MAX_FRAME {
                return Err(std::io::Error::new(ErrorKind::InvalidData, "a message over the room's largest"));
            }
            if self.inbuf.len() - at - 4 < len {
                break;
            }
            msgs.push(self.inbuf[at + 4..at + 4 + len].to_vec());
            at += 4 + len;
        }
        self.inbuf.drain(..at);
        Ok(msgs)
    }
}

impl Wire for Bridge {
    fn send(&mut self, _: &PeerId, frame: &[u8]) -> Result<(), SendError> {
        if self.closed {
            return Err(SendError::NotConnected);
        }
        if self.out.len() - self.sent > Self::MAX_OUT {
            let _ = self.flush();
            if self.out.len() - self.sent > Self::MAX_OUT {
                return Err(SendError::WouldBlock);
            }
        }
        let m = files::room_message(frame);
        self.out.extend_from_slice(&(m.len() as u32).to_be_bytes());
        self.out.extend_from_slice(&m);
        Ok(())
    }

    fn carries_files(&self, _: &PeerId) -> bool {
        !self.closed
    }
}

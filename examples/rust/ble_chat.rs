//! Chat in a room over Bluetooth LE on Linux: every copy in the same room finds the others nearby over the radio
//! (BlueZ) and over UDP multicast (`LanSignaling`), and links over UDP where the network allows, over Bluetooth where
//! it doesn't. Each link prints the path it takes, and again each time it moves.
//!
//! ```sh
//! cargo run -p peerwire --features ble-bluez --example ble_chat -- kitchen            # the radio and the LAN
//! cargo run -p peerwire --features ble-bluez --example ble_chat -- kitchen --no-udp   # the radio alone
//! ```
//!
//! It joins `lan_chat`'s and `room_chat`'s rooms, so `room_chat kitchen` on the LAN is in the room too, and so is
//! the ESP32 port in the room its flash names. `--no-udp` drops the LAN carrier and allows Bluetooth links alone
//! (`Policy::new([PathKind::Bluetooth])`), so every link goes over the radio. Type a line to send it to everyone in
//! the room. It needs `bluetoothd` and a powered adapter; centrals nearby read the adapter's alias as its name. The
//! driver's log goes to stderr: a part of the radio that failed and is tried again, the adapter going or changing.

use peerwire::ble::BlueZ;
use peerwire::{Event, LanSignaling, Lane, Node, PathKind, PeerId, Policy};
use std::sync::mpsc;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let no_udp = args.iter().any(|a| a == "--no-udp");
    let mut words = args.into_iter().filter(|a| a != "--no-udp");
    let room = words.next().unwrap_or_else(|| "lobby".into());
    let bind = words.next().unwrap_or_else(|| "0.0.0.0:0".into());

    log::set_logger(&Stderr).map_err(|e| e.to_string())?;
    log::set_max_level(log::LevelFilter::Info);
    let radio = BlueZ::new()?;
    println!("Bluetooth name (the adapter's alias): {:?}", radio.alias());
    if radio.names_this_host() {
        println!("warning: that is this machine's host name; set another with `bluetoothctl system-alias Peerwire`");
    }
    let builder = Node::builder().bluetooth(radio);
    let mut node = match no_udp {
        // No STUN either: with no UDP path allowed, nothing would use what it finds.
        true => builder.policy(Policy::new([PathKind::Bluetooth])).stun_servers(Vec::<String>::new()).bind(bind)?,
        false => builder.signaling(LanSignaling::new()?).bind(bind)?,
    };
    node.join_room("peerwire-lan-chat", &room);
    let only = if no_udp { ", over Bluetooth only" } else { "" };
    println!("{} in room {room:?} on {}{only}", node.peer_id(), node.local_addr());

    let (lines, typed) = mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines().map_while(Result::ok) {
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    loop {
        node.poll();
        while let Some(event) = node.try_recv_event() {
            match event {
                Event::Connected { peer, .. } => println!("+ {peer} over {}", over(&node, &peer)),
                Event::PathChanged { peer, .. } => println!("~ {peer} now over {}", over(&node, &peer)),
                Event::Disconnected { peer, .. } => println!("- {peer}"),
                // Escaped, so a member can't fake another's line or send terminal escapes.
                Event::Message { peer, data, .. } => {
                    let text = String::from_utf8_lossy(&data);
                    println!("{peer} ({}): {}", over(&node, &peer), text.escape_debug())
                }
                _ => {}
            }
        }
        while let Ok(line) = typed.try_recv() {
            let peers: Vec<_> = node.peers().copied().collect();
            for peer in &peers {
                node.send(peer, Lane::Reliable, line.as_bytes())?;
            }
            println!("(sent to {})", peers.len());
        }
        std::thread::sleep(node.next_timeout().unwrap_or(Duration::MAX).min(Duration::from_millis(5)));
    }
}

/// Prints Peerwire's log lines, such as the Bluetooth driver's, to stderr.
struct Stderr;

impl log::Log for Stderr {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Info && m.target().starts_with("peerwire")
    }

    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            eprintln!("{} {}", r.level(), r.args());
        }
    }

    fn flush(&self) {}
}

/// The kind of path the link to `peer` takes: `Direct` over UDP, `Bluetooth` over the radio.
fn over(node: &Node, peer: &PeerId) -> String {
    node.path_kind(peer).map_or_else(|| "nothing".into(), |kind| format!("{kind:?}"))
}

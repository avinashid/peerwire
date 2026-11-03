//! Chat in a room with peers on the LAN and across the internet: every copy in the same
//! room finds the others over UDP multicast (`LanSignaling`) and over public Nostr relays
//! (`NostrSignaling`), both at once (`MultiSignaling`), and links with them directly.
//!
//! ```sh
//! cargo run -p peerwire --features nostr --example room_chat -- kitchen                 # anywhere
//! cargo run -p peerwire --features nostr --example room_chat -- kitchen 0.0.0.0:40404   # on a fixed UDP port
//! ```
//!
//! It joins `lan_chat`'s rooms, so `lan_chat kitchen` on the same LAN is in the room too,
//! and so is the ESP32 port on Wi-Fi, in the room its flash names. Type a line to send it to everyone
//! in the room. Anyone who guesses the room's name can join it, so pick one others can't
//! guess.

use peerwire::{Event, LanSignaling, Lane, MultiSignaling, Node, NostrSignaling};
use std::sync::mpsc;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let room = args.next().unwrap_or_else(|| "lobby".into());
    let bind = args.next().unwrap_or_else(|| "0.0.0.0:0".into());
    let both = MultiSignaling::new(vec![Box::new(LanSignaling::new()?), Box::new(NostrSignaling::default())]);
    let mut node = Node::builder().signaling(both).bind(bind)?;
    node.join_room("peerwire-lan-chat", &room);
    println!("{} in room {room:?} on {}", node.peer_id(), node.local_addr());

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
                Event::Connected { peer, path, .. } => println!("+ {peer} ({path:?})"),
                Event::Disconnected { peer, .. } => println!("- {peer}"),
                // Escaped, so a member can't fake another's line or send terminal escapes.
                Event::Message { peer, data, .. } => {
                    println!("{peer}: {}", String::from_utf8_lossy(&data).escape_debug())
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

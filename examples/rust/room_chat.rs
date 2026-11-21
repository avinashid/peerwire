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
//! and so is the ESP32 port on Wi-Fi, in the room its flash names, and a browser on the LED page,
//! which links over the relays (a pipe). Type a line to send it to everyone in the room. Anyone who
//! guesses the room's name can join it, so pick one others can't guess. `PW_RELAYS` (relay URLs,
//! comma-separated) replaces the default relays: `ws://127.0.0.1:7447` for the `nostr_relay`
//! example, with no internet.
//!
//! With the `webrtc` feature (`--features nostr,webrtc`), `joinRoom` pages in the room (app id
//! `peerwire-lan-chat`) are in it too, over WebRTC: each shows with its presence, its text messages
//! as lines, and ours go to it as room.js text. Ours says `{"name": "<PW_NAME or 'room_chat'>"}`.

use peerwire::{Event, LanSignaling, Lane, MultiSignaling, Node, NostrSignaling};
use std::sync::mpsc;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let room = args.next().unwrap_or_else(|| "lobby".into());
    let bind = args.next().unwrap_or_else(|| "0.0.0.0:0".into());
    let nostr = match std::env::var("PW_RELAYS") {
        Ok(urls) => NostrSignaling::new(urls.split(',').map(str::trim).filter(|u| !u.is_empty())),
        Err(_) => NostrSignaling::default(),
    };
    let both = MultiSignaling::new(vec![Box::new(LanSignaling::new()?), Box::new(nostr)]);
    let builder = Node::builder().signaling(both);
    #[cfg(feature = "webrtc")]
    let builder = builder.webrtc(true);
    let mut node = builder.bind(bind)?;
    #[cfg(feature = "webrtc")]
    node.set_presence(&{
        let name = std::env::var("PW_NAME").unwrap_or_else(|_| "room_chat".into());
        serde_json::json!({ "name": name }).to_string()
    })?;
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
                Event::Connected { peer, path, .. } => println!("+ {peer} ({path:?}){}", browser(&node, &peer)),
                Event::Disconnected { peer, .. } => println!("- {peer}"),
                // Escaped, so a member can't fake another's line or send terminal escapes.
                Event::Message { peer, data, .. } => {
                    if let Some(text) = text(&node, &peer, &data) {
                        println!("{peer}: {}", text.escape_debug())
                    }
                }
                _ => {}
            }
        }
        while let Ok(line) = typed.try_recv() {
            let peers: Vec<_> = node.peers().copied().collect();
            for peer in &peers {
                #[cfg(feature = "webrtc")]
                if node.is_browser(peer) {
                    node.send(peer, Lane::Reliable, &Node::room_text(&line))?;
                    continue;
                }
                node.send(peer, Lane::Reliable, line.as_bytes())?;
            }
            println!("(sent to {})", peers.len());
        }
        std::thread::sleep(node.next_timeout().unwrap_or(Duration::MAX).min(Duration::from_millis(5)));
    }
}

/// How a peer that joined shows: a browser with its presence (feature `webrtc`).
fn browser(node: &Node, peer: &peerwire::PeerId) -> String {
    #[cfg(feature = "webrtc")]
    if node.is_browser(peer) {
        return format!(" browser {}", node.presence(peer).unwrap_or("{}"));
    }
    let _ = (node, peer);
    String::new()
}

/// A message's text: a native peer's bytes, or a browser's text or JSON message (room.js frames it, a tag first).
fn text(node: &Node, peer: &peerwire::PeerId, data: &[u8]) -> Option<String> {
    #[cfg(feature = "webrtc")]
    if node.is_browser(peer) {
        return Node::room_message_text(data);
    }
    let _ = (node, peer);
    Some(String::from_utf8_lossy(data).into_owned())
}

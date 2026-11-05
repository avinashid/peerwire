//! A Peerwire node on an ESP32: it joins your Wi-Fi, sets its clock and joins the room in cfg.toml. It sends back
//! every message a peer sends it, after "echo: ".
//!
//! To talk to it from a PC on the same Wi-Fi, run `lan_chat` with the same room in a clone of
//! https://github.com/avinashid/peerwire and type a line:
//!
//! ```sh
//! cargo run -p peerwire --example lan_chat -- esp32
//! ```

use log::{info, warn};
use peerwire_esp::peerwire::{Event, Lane};
use peerwire_esp::Board;

/// `lan_chat`'s app, so its rooms are this board's rooms.
const APP: &str = "peerwire-lan-chat";
const ROOM: &str = env!("CFG_ROOM");

fn main() {
    peerwire_esp::run(|| {
        let board = Board::connect(env!("CFG_WIFI_SSID"), env!("CFG_WIFI_PASSWORD"))?;
        #[cfg(not(feature = "nostr"))]
        let mut node = board.node()?;
        #[cfg(feature = "nostr")]
        let mut node = board.node_with_nostr(peerwire_esp::NOSTR_RELAY)?;
        node.join_room(APP, ROOM);
        info!("{} in room {ROOM:?} at {}", node.peer_id(), node.local_addr());
        loop {
            node.poll();
            while let Some(event) = node.try_recv_event() {
                match event {
                    Event::Connected { peer, .. } => info!("linked to {peer}"),
                    Event::Message { peer, data, .. } => {
                        // Escaped, as a peer could send terminal escapes to whoever reads the log.
                        let text = String::from_utf8_lossy(&data[..data.len().min(80)]).escape_debug().to_string();
                        info!("from {peer}: {text}");
                        let reply = [b"echo: ".as_slice(), &data].concat();
                        if let Err(e) = node.send(&peer, Lane::Reliable, &reply) {
                            warn!("send to {peer}: {e:?}");
                        }
                    }
                    Event::Disconnected { peer, reason } => info!("{peer} left: {reason:?}"),
                    _ => {}
                }
            }
            peerwire_esp::idle(&node);
        }
    })
}

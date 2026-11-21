//! NB-162: WebRTC to browsers changes nothing between native peers. Nodes with it on (`Builder::webrtc`) and without
//! link with each other over UDP, as ever, their events and raw messages untouched by the room layer, which runs on
//! browsers' links alone; and a browser-shaped hello is no native's. The browser side runs in
//! `packages/js/test/share_webrtc.mjs` (headless Chromium) and over memory in `src/webrtc/tests.rs`.
#![cfg(feature = "webrtc")]

use peerwire::{DisconnectReason, Event, Lane, MemoryHub, Node, PathKind, Status};
use std::time::{Duration, Instant};

fn node(hub: &MemoryHub, webrtc: bool) -> Node {
    let b = Node::builder().stun_servers(Vec::<String>::new()).signaling(hub.connect()).webrtc(webrtc);
    let mut n = b.bind("127.0.0.1:0").unwrap();
    n.set_presence(r#"{"name":"native"}"#).unwrap();
    n.join_room("pw-webrtc", "natives");
    n
}

#[test]
fn natives_link_over_udp_whether_webrtc_is_on_or_not() {
    let hub = MemoryHub::new();
    let mut nodes = [node(&hub, true), node(&hub, true), node(&hub, false)];
    let ids: Vec<_> = nodes.iter().map(Node::peer_id).collect();
    let mut events: Vec<Vec<Event>> = vec![Vec::new(); 3];
    let linked = |nodes: &[Node]| {
        nodes.iter().all(|n| ids.iter().filter(|p| **p != n.peer_id()).all(|p| n.status(p) == Status::Connected))
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !linked(&nodes) && Instant::now() < deadline {
        for (n, ev) in nodes.iter_mut().zip(events.iter_mut()) {
            n.poll();
            ev.extend(std::iter::from_fn(|| n.try_recv_event()));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(linked(&nodes), "{events:?}");
    for (i, n) in nodes.iter().enumerate() {
        for p in ids.iter().filter(|p| **p != ids[i]) {
            assert!(!n.is_browser(p) && n.presence(p).is_none(), "a native is no browser");
            assert_eq!(n.path_kind(p), Some(PathKind::Direct));
        }
        assert_eq!((n.webrtc_counts(), n.browsers()), ((0, 0), Vec::new()), "no WebRTC between natives");
    }
    // Raw bytes, no room tag: the app's own, both ways, whatever the first byte says (5 would be a link proof, 4 a
    // goodbye, to a browser).
    for (from, to) in [(0, 1), (1, 2), (2, 0)] {
        let peer = ids[to];
        nodes[from].send(&peer, Lane::Reliable, &[5, 1, 2]).unwrap();
        nodes[from].send(&peer, Lane::Reliable, &[4]).unwrap();
    }
    let got = |events: &[Vec<Event>]| {
        events.iter().all(|ev| ev.iter().filter(|e| matches!(e, Event::Message { .. })).count() == 2)
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while !got(&events) && Instant::now() < deadline {
        for (n, ev) in nodes.iter_mut().zip(events.iter_mut()) {
            n.poll();
            ev.extend(std::iter::from_fn(|| n.try_recv_event()));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    for ev in &events {
        let data: Vec<&[u8]> = ev
            .iter()
            .filter_map(|e| match e {
                Event::Message { data, .. } => Some(&data[..]),
                _ => None,
            })
            .collect();
        assert_eq!(data, [&[5, 1, 2][..], &[4]], "{ev:?}");
        let left = |e: &Event| matches!(e, Event::Disconnected { reason, .. } if *reason != DisconnectReason::Replaced);
        assert!(!ev.iter().any(left), "a 4 is no goodbye between natives");
    }
    // Presence must be JSON.
    assert!(nodes[0].set_presence("{nope").is_err() && nodes[0].set_presence("[1]").is_ok());
}

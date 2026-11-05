//! NB-106: the doctor (`examples/rust/doctor.rs`) against local fixtures only: STUN
//! servers on loopback that map as a NAT would, an RFC 5780 server on 127.0.0.1 and
//! 127.0.0.2, the in-memory Nostr relay, a fake PCP gateway and a LAN group on
//! loopback. The example's own tests (its NAT verdicts) run here too.
#[path = "../../../examples/rust/doctor.rs"]
mod doctor;
#[path = "../../../examples/rust/nostr_relay.rs"]
mod relay;

use doctor::{render, run, Config};
use peerwire_core::stun;
use serde_json::Value;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The public address the fixtures hand out (TEST-NET-2, public as far as Peerwire can tell).
const PUBLIC: Ipv4Addr = Ipv4Addr::new(198, 51, 100, 7);

/// Every check off, and nothing pointed anywhere but `stun`: each test turns on what it needs.
fn local(stun: Vec<String>) -> Config {
    Config {
        stun,
        stun5780: None,
        stun_wait: Duration::from_millis(300),
        relays: Vec::new(),
        lan: None,
        portmap: false,
        redact: false,
        wait: Duration::from_secs(5),
        gateway: None,
    }
}

/// A STUN server on loopback that tells each requester it was seen at `map(from)`,
/// answering from its own address whatever the request asks, as Google's and
/// Cloudflare's do.
fn stun_server(map: impl Fn(SocketAddr) -> SocketAddr + Send + 'static) -> String {
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = s.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut buf = [0u8; 576];
        while let Ok((n, from)) = s.recv_from(&mut buf) {
            if let Some(tx) = stun::parse_binding_request(&buf[..n]) {
                let _ = s.send_to(&stun::binding_response(&tx, map(from)), from);
            }
        }
    });
    addr.to_string()
}

/// A cone NAT that keeps the port.
fn cone(from: SocketAddr) -> SocketAddr {
    SocketAddr::from((PUBLIC, from.port()))
}

#[derive(Clone, Copy, Debug)]
enum Rfc5780 {
    /// Answers each change from where it asks, as an open NAT lets it in.
    Full,
    /// Drops the answers from its other IP, as an address-dependent filter does.
    SameIpOnly,
    /// Drops every changed answer, as an address-and-port-dependent filter does.
    NoChanges,
    /// Answers a change from its own address, as a server without RFC 5780 does.
    Ignores,
    /// Answers a change with a Binding error (420, unknown attribute).
    Refuses,
}

/// An RFC 5780 STUN server on 127.0.0.1, with its other port there and its other IP
/// 127.0.0.2 (Linux routes all of 127/8 to loopback); `None` where that can't be bound.
fn stun5780(mode: Rfc5780) -> Option<String> {
    let other_ip = UdpSocket::bind("127.0.0.2:0").ok()?;
    let main = UdpSocket::bind("127.0.0.1:0").unwrap();
    let other_port = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = main.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut buf = [0u8; 576];
        while let Ok((n, from)) = main.recv_from(&mut buf) {
            let Some(tx) = stun::parse_binding_request(&buf[..n]) else { continue };
            // CHANGE-REQUEST (0x0003), the one attribute the doctor sends.
            let change = if n >= 28 && buf[20..22] == [0, 3] { buf[27] & 0x06 } else { 0 };
            let ok = stun::binding_response(&tx, from);
            let mut error = ok[..20].to_vec();
            error[..4].copy_from_slice(&[0x01, 0x11, 0, 0]);
            let (sock, msg) = match (change, mode) {
                (0, _) | (_, Rfc5780::Ignores) => (&main, &ok),
                (_, Rfc5780::Refuses) => (&main, &error),
                (0x06, Rfc5780::Full) => (&other_ip, &ok),
                (0x02, Rfc5780::Full | Rfc5780::SameIpOnly) => (&other_port, &ok),
                _ => continue,
            };
            let _ = sock.send_to(msg, from);
        }
    });
    Some(addr.to_string())
}

/// A PCP gateway on loopback that maps any port to `ext`:41000; logs each request's lifetime.
fn pcp_gateway(ext: Ipv4Addr) -> (SocketAddr, Arc<Mutex<Vec<u32>>>) {
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = s.local_addr().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let lifetimes = log.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 1100];
        while let Ok((n, from)) = s.recv_from(&mut buf) {
            let req = &buf[..n];
            if n < 60 || req[..2] != [2, 1] {
                continue; // not a PCP MAP request
            }
            let lifetime = u32::from_be_bytes(req[4..8].try_into().unwrap());
            lifetimes.lock().unwrap().push(lifetime);
            let mut b = vec![2, 0x81, 0, 0];
            b.extend_from_slice(&lifetime.to_be_bytes());
            b.extend_from_slice(&[0; 16]); // epoch, reserved
            b.extend_from_slice(&req[24..42]); // nonce, protocol, internal port
            b.extend_from_slice(&41000u16.to_be_bytes());
            b.extend_from_slice(&ext.to_ipv6_mapped().octets());
            let _ = s.send_to(&b, from);
        }
    });
    (addr, log)
}

/// A LAN group port of this process's own (tests/lan.rs takes even ones from 34000), so
/// parallel runs don't hear each other.
fn lan_group() -> SocketAddrV4 {
    SocketAddrV4::new(*peerwire::LanSignaling::GROUP.ip(), 42001 + (std::process::id() % 4000) as u16 * 2)
}

fn eventually(mut done: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn summary(r: &Value) -> String {
    r["summary"].as_array().unwrap().iter().map(|l| l.as_str().unwrap()).collect::<Vec<_>>().join("\n")
}

#[test]
fn a_whole_report_from_local_fixtures() {
    let relay = format!("ws://{}", relay::spawn("127.0.0.1:0").unwrap());
    let (gw, lifetimes) = pcp_gateway(PUBLIC);
    let mut cfg = local(vec![stun_server(cone), stun_server(cone)]);
    cfg.stun5780 = stun5780(Rfc5780::Full);
    // The second relay refuses connections: it gives up after `wait`.
    cfg.relays = vec![relay.clone(), "ws://127.0.0.1:1".into()];
    cfg.wait = Duration::from_secs(3);
    cfg.lan = Some((lan_group(), Ipv4Addr::LOCALHOST));
    cfg.portmap = true;
    cfg.gateway = Some((gw, "127.0.0.1:9".parse().unwrap()));
    let mut steps = Vec::new();
    let r = run(&cfg, &mut |s| steps.push(s.to_string()));
    assert_eq!(steps.len(), 3, "{steps:?}");
    assert_eq!((r["doctor"].as_u64(), r["redacted"].as_bool()), (Some(1), Some(false)));

    let v4 = &r["ipv4"];
    assert_eq!(v4["mapping"]["kind"], "endpoint-independent", "{v4:#}");
    assert!(v4["mapping"]["detail"].as_str().unwrap().ends_with("port preserved"), "{v4:#}");
    let answers = v4["stun"].as_array().unwrap();
    assert_eq!(answers.len(), 2);
    assert!(answers.iter().all(|a| a["mapped"].as_str().unwrap().starts_with("198.51.100.7:")), "{v4:#}");
    if cfg.stun5780.is_some() {
        assert_eq!(v4["filtering"]["kind"], "endpoint-independent", "{v4:#}");
    }
    // No IPv6 STUN server among the fixtures, or no IPv6 route: either way no answers.
    assert!(["unknown", "no-route"].contains(&r["ipv6"]["mapping"]["kind"].as_str().unwrap()), "{:#}", r["ipv6"]);

    let pm = &r["port_mapping"];
    assert_eq!((pm["kind"].as_str(), pm["protocol"].as_str()), (Some("mapped"), Some("pcp")), "{pm:#}");
    assert_eq!(pm["addr"], "198.51.100.7:41000");
    assert!(eventually(|| lifetimes.lock().unwrap().last() == Some(&0)), "the mapping is removed: {lifetimes:?}");

    let sig = r["signaling"].as_array().unwrap();
    assert_eq!(sig.len(), 3, "{sig:#?}");
    assert_eq!((sig[0]["relay"].as_str(), sig[0]["kind"].as_str()), (Some(relay.as_str()), Some("reachable")));
    assert!(sig[0]["open_ms"].is_u64() && sig[0]["round_trip_ms"].is_u64(), "{:#}", sig[0]);
    assert_eq!(sig[1]["kind"], "unreachable", "{:#}", sig[1]);
    assert_eq!((sig[2]["network"].as_str(), sig[2]["kind"].as_str()), (Some("lan"), Some("works-here")));

    let short = summary(&r);
    for want in ["IPv4: an easy NAT", "Port mapping works", "Nostr: 1 of 2 relays work"] {
        assert!(short.contains(want), "{want:?} in\n{short}");
    }
    let text = render(&r);
    for want in [
        "Peerwire doctor: peerwire ",
        "Nothing was uploaded. This report shows your public IP addresses",
        "\nIPv4\n  local ",
        "\nIPv6\n",
        "\nRouter port mapping\n  result       mapped: ",
        "\nSignaling\n",
        &format!("nostr {relay}"),
        "\nIn short\n  - IPv4: an easy NAT",
    ] {
        assert!(text.contains(want), "{want:?} in\n{text}");
    }
    let back: Value = serde_json::from_str(&serde_json::to_string_pretty(&r).unwrap()).unwrap();
    assert_eq!(back, r);
}

#[test]
fn a_symmetric_nat_redacted() {
    // A new port for every destination, both destinations on one IP.
    let far = stun_server(|f| SocketAddr::from((PUBLIC, f.port().wrapping_add(1000))));
    let farther = stun_server(|f| SocketAddr::from((PUBLIC, f.port().wrapping_add(2000))));
    let mut cfg = local(vec![far, farther]);
    cfg.redact = true;
    let r = run(&cfg, &mut |_| {});
    assert_eq!(r["ipv4"]["mapping"]["kind"], "address-and-port-dependent", "{:#}", r["ipv4"]);
    assert_eq!(r["ipv4"]["filtering"]["kind"], "not-measured");
    assert_eq!(
        (r["port_mapping"]["kind"].as_str(), r["signaling"].as_array().map(Vec::len)),
        (Some("skipped"), Some(0))
    );
    assert!(summary(&r).starts_with("IPv4: a hard NAT"), "{}", summary(&r));
    let all = format!("{}\n{r}", render(&r));
    assert!(!all.contains("198.51.100.7") && all.contains("198.51.x.x:"), "{all}");
    assert!(all.contains("Public IP addresses are masked"), "{all}");
}

#[test]
fn filtering_and_servers_without_rfc_5780() {
    use Rfc5780::*;
    for (mode, want) in [
        (Full, "endpoint-independent"),
        (SameIpOnly, "address-dependent"),
        (NoChanges, "address-and-port-dependent"),
        (Ignores, "unsupported"),
        (Refuses, "unsupported"),
    ] {
        let Some(server) = stun5780(mode) else {
            eprintln!("127.0.0.2 can't be bound here: no filtering test");
            return;
        };
        let mut cfg = local(Vec::new());
        cfg.stun5780 = Some(server);
        let r = run(&cfg, &mut |_| {});
        assert_eq!(r["ipv4"]["filtering"]["kind"], want, "{mode:?}: {:#}", r["ipv4"]);
    }
    let mut cfg = local(Vec::new());
    cfg.stun5780 = Some("127.0.0.1:9".into()); // nothing answers there
    let r = run(&cfg, &mut |_| {});
    assert_eq!(r["ipv4"]["filtering"]["kind"], "not-measured", "{:#}", r["ipv4"]);
}

#[test]
fn port_mapping_behind_a_carrier_nat() {
    let (gw, _) = pcp_gateway(Ipv4Addr::new(100, 64, 0, 9));
    let mut cfg = local(Vec::new());
    cfg.portmap = true;
    cfg.gateway = Some((gw, "127.0.0.1:9".parse().unwrap()));
    let r = run(&cfg, &mut |_| {});
    let pm = &r["port_mapping"];
    assert_eq!(pm["kind"], "not-public", "{pm:#}");
    assert!(pm["detail"].as_str().unwrap().contains("100.64.0.9 is not public"), "{pm:#}");
    assert!(summary(&r).contains("behind another NAT"), "{}", summary(&r));
}

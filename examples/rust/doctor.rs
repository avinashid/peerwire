//! Peerwire doctor: what this network does to peer-to-peer traffic, in a report to
//! paste into an issue or a row of the field results (docs/project/field-connectivity.md).
//! It checks NAT mapping and filtering over IPv4 and IPv6, router port mapping, and
//! whether each signaling network answers; with `room <name>` it then meets a peer
//! running the same command and says whether the link came up direct, relayed or not
//! at all.
//!
//! ```sh
//! cargo run -p peerwire --features nostr --example doctor                     # the report
//! cargo run -p peerwire --features nostr --example doctor -- --json           # the same as JSON
//! cargo run -p peerwire --features nostr --example doctor -- --redact         # public IPs masked
//! cargo run -p peerwire --features nostr --example doctor -- room <name>      # report, then meet a peer
//! cargo run -p peerwire --features nostr --example doctor -- room <name> v6   # the same, over IPv6
//! ```
//!
//! Nothing is uploaded: the report goes to stdout (progress to stderr) and nowhere
//! else. To make it the doctor sends STUN Binding requests to the probe servers
//! (Google's and Cloudflare's), asks the router for a port mapping for a socket of its
//! own (PCP, NAT-PMP or UPnP, as a node with port mapping on does) and removes it before
//! it exits, publishes one ephemeral event under a throwaway key on a random topic to
//! each Nostr relay (relays forward those and keep none), and sends one frame to the
//! LAN group.
//!
//! - `--stun host:port`, repeatable: probe these STUN servers instead.
//! - `--stun5780 host:port`: a STUN server with RFC 5780 support (a second IP address
//!   and CHANGE-REQUEST), for the filtering test. Google's and Cloudflare's servers
//!   have neither, so with the defaults filtering is not measured.
//! - `--relay url`, repeatable: check these Nostr relays instead of the defaults.
//! - `--no-portmap`, `--no-lan`: skip those checks.
//!
//! `DOCTOR_PORT=<port>` binds the room node to that UDP port instead of a random one,
//! for a peer whose firewall has that one port open.

use peerwire::nostr::DEFAULT_RELAYS;
use peerwire::{
    Event, LanSignaling, Lane, MapProtocol, NatType, Node, NostrSignaling, Path, PortMapping, Signaling, Topic,
    DEFAULT_STUN_SERVERS,
};
use peerwire_core::stun;
use rand_core::{OsRng, RngCore};
use serde_json::{json, Value};
use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// STUN targets for the port-pattern probe. Google's hostnames all resolve to
/// one address, so the distinct destinations come from extra ports instead:
/// two server IPs and four IP:port pairs in all, interleaved so consecutive
/// requests go to different IPs. Not port 53: carriers and routers intercept
/// DNS, and in the field it went unanswered or came back from another address.
const PROBE_SERVERS: &[&str] =
    &["stun.l.google.com:19302", "stun.cloudflare.com:3478", "stun.l.google.com:3478", "stun.cloudflare.com:443"];

/// The report's format: bumped when a field changes meaning.
const FORMAT: u32 = 1;

const USAGE: &str = "usage: doctor [--json] [--redact] [--stun host:port]... [--stun5780 host:port] \
                     [--relay url]... [--no-portmap] [--no-lan] [room <name> [v6]]";

/// What the doctor checks, and where.
pub struct Config {
    /// STUN servers for the mapping probe.
    pub stun: Vec<String>,
    /// A STUN server with RFC 5780 support, for the filtering test.
    pub stun5780: Option<String>,
    /// How long to wait for a STUN answer before asking again (three tries).
    pub stun_wait: Duration,
    /// Nostr relays to check.
    pub relays: Vec<String>,
    /// The LAN carrier's group and interface (`None`: no LAN check).
    pub lan: Option<(SocketAddrV4, Ipv4Addr)>,
    /// Whether to ask the router for a port mapping.
    pub portmap: bool,
    /// Mask public IP addresses in the report.
    pub redact: bool,
    /// How long one relay check may take.
    pub wait: Duration,
    /// A fake gateway and SSDP address for the port-mapping check.
    #[cfg(test)]
    pub gateway: Option<(SocketAddr, SocketAddr)>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            stun: PROBE_SERVERS.iter().map(|s| s.to_string()).collect(),
            stun5780: None,
            stun_wait: Duration::from_millis(1000),
            relays: DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect(),
            lan: Some((LanSignaling::GROUP, Ipv4Addr::UNSPECIFIED)),
            portmap: true,
            redact: false,
            wait: Duration::from_secs(8),
            #[cfg(test)]
            gateway: None,
        }
    }
}

impl Config {
    /// `addr` as the report shows it: a public IP masked under `--redact`.
    fn show(&self, addr: SocketAddr) -> String {
        match addr.ip() {
            _ if !self.redact || !public(addr.ip()) => addr.to_string(),
            IpAddr::V4(ip) => format!("{}.{}.x.x:{}", ip.octets()[0], ip.octets()[1], addr.port()),
            IpAddr::V6(ip) => format!("[{:x}:{:x}:x:x:x:x:x:x]:{}", ip.segments()[0], ip.segments()[1], addr.port()),
        }
    }
}

#[allow(dead_code)] // unused where tests/doctor.rs includes this file
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut cfg = Config::default();
    let (mut json, mut stun, mut relays, mut room) = (false, Vec::new(), Vec::new(), None);
    let mut i = 0;
    while i < args.len() {
        let takes_value = matches!(args[i].as_str(), "--stun" | "--stun5780" | "--relay" | "room");
        let value = || args.get(i + 1).cloned().ok_or(USAGE);
        match args[i].as_str() {
            "--json" => json = true,
            "--redact" => cfg.redact = true,
            "--no-portmap" => cfg.portmap = false,
            "--no-lan" => cfg.lan = None,
            "--stun" => stun.push(value()?),
            "--stun5780" => cfg.stun5780 = Some(value()?),
            "--relay" => relays.push(value()?),
            "room" => {
                let v6 = args.get(i + 2).is_some_and(|a| a == "v6");
                room = Some((value()?, v6));
                i += usize::from(v6);
            }
            _ => return Err(USAGE.into()),
        }
        i += 1 + usize::from(takes_value);
    }
    if !stun.is_empty() {
        cfg.stun = stun;
    }
    if !relays.is_empty() {
        cfg.relays = relays;
    }
    if json && room.is_some() {
        return Err("--json is for the report alone: run `room` without it".into());
    }
    let report = run(&cfg, &mut |step| eprintln!("doctor: {step}"));
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render(&report));
    }
    match room {
        Some((room, v6)) => meet(&room, v6),
        None => Ok(()),
    }
}

/// Runs every check and returns the report; `progress` hears each step as it starts.
pub fn run(cfg: &Config, progress: &mut dyn FnMut(&str)) -> Value {
    progress("IPv4: asking the STUN servers how this network maps a socket");
    let ipv4 = probe(cfg, false);
    progress("IPv6: the same");
    let ipv6 = probe(cfg, true);
    progress("port mapping and signaling: asking the router, the relays and the LAN");
    let (port_mapping, relays, lan) = std::thread::scope(|s| {
        let pm = s.spawn(|| port_mapping(cfg));
        let relays: Vec<_> = cfg.relays.iter().map(|url| s.spawn(|| relay(url, cfg.wait))).collect();
        let lan = lan(cfg);
        (pm.join().unwrap(), relays.into_iter().map(|r| r.join().unwrap()).collect::<Vec<_>>(), lan)
    });
    let mut report = json!({
        "doctor": FORMAT,
        "tool": format!("peerwire {} (Rust)", env!("CARGO_PKG_VERSION")),
        "date": utc(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "redacted": cfg.redact,
        "ipv4": ipv4,
        "ipv6": ipv6,
        "port_mapping": port_mapping,
        "signaling": relays.into_iter().chain(lan).collect::<Vec<_>>(),
    });
    report["summary"] = summary(&report).into();
    report
}

/// Asks each probe server, in turn and from one socket, for its view of our
/// address, and says what the answers make of the NAT; then the filtering test.
fn probe(cfg: &Config, v6: bool) -> Value {
    let family = if v6 { "IPv6" } else { "IPv4" };
    let no_route = |detail: String| json!({ "local": null, "mapping": { "kind": "no-route", "detail": detail } });
    let Some(local_ip) = primary_ip(v6) else {
        return no_route(format!("no {family} route from this machine"));
    };
    if let IpAddr::V6(ip) = local_ip {
        if !is_global_v6(ip) {
            return no_route(format!("{ip} is not a global address: no IPv6 internet here"));
        }
    }
    let sock = match UdpSocket::bind(SocketAddr::new(unspecified(v6), 0)) {
        Ok(s) => s,
        Err(e) => return no_route(format!("can't open a UDP socket: {e}")),
    };
    let local = SocketAddr::new(local_ip, sock.local_addr().map_or(0, |a| a.port()));
    // One request per distinct destination: several names can share an address.
    let mut dests: Vec<(&str, SocketAddr)> = Vec::new();
    for server in &cfg.stun {
        let Ok(mut addrs) = server.to_socket_addrs() else { continue };
        match addrs.find(|a| a.is_ipv6() == v6) {
            Some(dest) if !dests.iter().any(|(_, d)| *d == dest) => dests.push((server, dest)),
            _ => {}
        }
    }
    let mut answers = Vec::new();
    let mut mapped: Vec<(SocketAddr, SocketAddr)> = Vec::new();
    for (server, dest) in &dests {
        let tx = txid();
        let got = ask(&sock, *dest, &stun::binding_request(&tx), &tx, cfg.stun_wait);
        match got.and_then(|(_, b, rtt)| Some((stun::parse_binding_response(&b, &tx)?, rtt))) {
            Some((m, rtt)) => {
                let ms = rtt.as_millis() as u64;
                answers.push(json!({ "server": server, "addr": dest.to_string(), "mapped": cfg.show(m), "ms": ms }));
                mapped.push((*dest, m));
            }
            None => answers.push(json!({ "server": server, "addr": dest.to_string(), "mapped": null })),
        }
    }
    let (kind, detail) = if dests.is_empty() {
        ("unknown", format!("none of the STUN servers has an {family} address"))
    } else {
        verdict(local, &mapped)
    };
    let filtering = match (&cfg.stun5780, v6) {
        (_, true) => json!({ "kind": "not-measured", "detail": "the filtering test runs over IPv4" }),
        (None, false) => json!({
            "kind": "not-measured",
            "detail": "needs a STUN server with RFC 5780 support (--stun5780 host:port): \
                       Google's and Cloudflare's can't answer from another address",
        }),
        (Some(server), false) => filtering(server, cfg.stun_wait),
    };
    json!({
        "local": cfg.show(local),
        "stun": answers,
        "mapping": { "kind": kind, "detail": detail },
        "filtering": filtering,
    })
}

/// Sends `req` to `dest` up to three times, waiting `wait` after each, for a STUN
/// message of transaction `tx`: returns where it came from, its bytes and the time
/// since the last send.
fn ask(
    sock: &UdpSocket,
    dest: SocketAddr,
    req: &[u8],
    tx: &[u8; 12],
    wait: Duration,
) -> Option<(SocketAddr, Vec<u8>, Duration)> {
    let mut buf = [0u8; 576];
    for _ in 0..3 {
        let sent = Instant::now();
        sock.send_to(req, dest).ok()?;
        while let Some(left) = (sent + wait).checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) {
            sock.set_read_timeout(Some(left)).ok()?;
            let Ok((n, from)) = sock.recv_from(&mut buf) else { break };
            if stun::is_stun(&buf[..n]) && buf[8..20] == tx[..] {
                return Some((from, buf[..n].to_vec(), sent.elapsed()));
            }
        }
    }
    None
}

fn txid() -> [u8; 12] {
    let mut tx = [0u8; 12];
    OsRng.fill_bytes(&mut tx);
    tx
}

/// Classifies the NAT from `(destination, mapped address)` pairs, in send order:
/// a kind for the JSON and a sentence for people.
fn verdict(local: SocketAddr, mapped: &[(SocketAddr, SocketAddr)]) -> (&'static str, String) {
    let Some(&(_, first)) = mapped.first() else {
        return ("blocked", "no STUN answers: UDP is blocked here, or the servers are unreachable".into());
    };
    if mapped.len() == 1 {
        return ("unknown", "only one answer: can't tell".into());
    }
    if mapped.iter().all(|(_, m)| *m == first) {
        if first == local {
            let detail = "the public address is this machine's own (a firewall may still drop unsolicited packets)";
            return ("no-nat", detail.into());
        }
        let kept = if first.port() == local.port() { ", port preserved" } else { "" };
        return ("endpoint-independent", format!("cone: the same public address for every destination{kept}"));
    }
    let ips: Vec<IpAddr> = mapped.iter().map(|(_, m)| m.ip()).collect();
    let mut distinct = ips.clone();
    distinct.sort();
    distinct.dedup();
    if mapped.iter().all(|(_, m)| m.port() == first.port()) {
        return (
            "pooled",
            format!(
                "port kept for every destination{}, but the public IP varies across {} addresses \
                 (an address pool without paired pooling)",
                if first.port() == local.port() { " (preserved)" } else { "" },
                distinct.len()
            ),
        );
    }
    let multi_ip = if distinct.len() > 1 { "; public IP varies by destination" } else { "" };
    // Same mapping for every port of one server IP means the NAT keys on the address only.
    let per_ip = mapped.iter().all(|(d, m)| mapped.iter().filter(|(d2, _)| d2.ip() == d.ip()).all(|(_, m2)| m2 == m));
    let shared = mapped.iter().any(|(d, _)| mapped.iter().filter(|(d2, _)| d2.ip() == d.ip()).count() > 1);
    let (kind, keyed) = match (shared, per_ip) {
        (false, _) => ("dependent", ""),
        (true, true) => ("address-dependent", ", address-dependent (one port per server IP)"),
        (true, false) => {
            ("address-and-port-dependent", ", address-and-port-dependent (a new port for every destination)")
        }
    };
    // Deltas between successive new ports: small steps mean a predictable allocator.
    let mut ports: Vec<u16> = Vec::new();
    for (_, m) in mapped {
        if !ports.contains(&m.port()) {
            ports.push(m.port());
        }
    }
    let deltas: Vec<i32> = ports.windows(2).map(|w| w[1] as i32 - w[0] as i32).collect();
    let alloc = match deltas.as_slice() {
        [] => "one port".to_string(),
        [_] => format!("1 delta {deltas:?}, too few to call the allocator"),
        _ if deltas.iter().all(|d| d.abs() <= 16) => format!("sequential ports, deltas {deltas:?}"),
        _ => format!("random ports, deltas {deltas:?}"),
    };
    (kind, format!("symmetric{keyed}; {alloc}{multi_ip}"))
}

/// The RFC 5780 filtering test (section 4.4) from a fresh socket: a Binding request
/// to `server` (test I), then the same asking for the answer from its other IP and
/// port (II), then from its other port only (III). What comes through says what this
/// NAT lets in from addresses it hasn't sent to. A server that answers a change from
/// its own address, or refuses the attribute, has no RFC 5780 support: that says
/// nothing about the NAT.
fn filtering(server: &str, wait: Duration) -> Value {
    let result = |kind: &str, detail: String| json!({ "kind": kind, "detail": detail, "server": server });
    let Some(dest) = server.to_socket_addrs().ok().and_then(|mut a| a.find(SocketAddr::is_ipv4)) else {
        return result("not-measured", format!("{server} has no IPv4 address"));
    };
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else {
        return result("not-measured", "can't open a UDP socket".into());
    };
    // Where an answer came from, and whether it was an error response.
    let test = |change: u8| {
        let tx = txid();
        let mut req = stun::binding_request(&tx);
        if change != 0 {
            req[3] = 8; // the message's length: one CHANGE-REQUEST attribute
            req.extend_from_slice(&[0x00, 0x03, 0x00, 0x04, 0, 0, 0, change]);
        }
        ask(&sock, dest, &req, &tx, wait).map(|(from, b, _)| (from, b[..2] == [0x01, 0x11]))
    };
    match test(0) {
        Some((from, false)) if from == dest => {}
        _ => return result("not-measured", format!("no answer from {server}")),
    }
    let unsupported = |from: SocketAddr, error: bool| {
        let why = if error { "refused CHANGE-REQUEST" } else { "answered from its own address" };
        result("unsupported", format!("{server} {why} (from {from}): it has no RFC 5780 support"))
    };
    match test(0x06) {
        Some((from, false)) if from.ip() != dest.ip() => {
            let detail = "an answer from another IP and port came in: any host can reach a mapping";
            return result("endpoint-independent", detail.into());
        }
        Some((from, error)) => return unsupported(from, error),
        None => {}
    }
    match test(0x02) {
        Some((from, false)) if from.ip() == dest.ip() && from.port() != dest.port() => {
            let detail =
                "only an answer from the same IP came in: a host this machine sent to can reach it from any port";
            result("address-dependent", detail.into())
        }
        Some((from, error)) => unsupported(from, error),
        None => {
            let detail =
                "no answer from another IP or port came in: only the address this machine sent to gets through";
            result("address-and-port-dependent", detail.into())
        }
    }
}

/// Asks the router for a mapping as a node with port mapping on does, then drops the
/// node, which removes it.
fn port_mapping(cfg: &Config) -> Value {
    if !cfg.portmap {
        return json!({ "kind": "skipped", "detail": "--no-portmap" });
    }
    let b = Node::builder().stun_servers(Vec::<String>::new()).port_mapping(true);
    #[cfg(test)]
    let b = match cfg.gateway {
        Some((gw, ssdp)) => b.port_mapping_at(gw, ssdp),
        None => b,
    };
    let mut node = match b.bind("0.0.0.0:0") {
        Ok(n) => n,
        Err(e) => return json!({ "kind": "unavailable", "detail": format!("can't open a UDP socket: {e}") }),
    };
    let start = Instant::now();
    while node.port_mapping() == PortMapping::Trying && start.elapsed() < Duration::from_secs(15) {
        node.poll();
        std::thread::sleep(Duration::from_millis(20));
    }
    let state = node.port_mapping();
    drop(node);
    let name = |p: MapProtocol| match p {
        MapProtocol::Pcp => ("pcp", "PCP"),
        MapProtocol::NatPmp => ("nat-pmp", "NAT-PMP"),
        MapProtocol::Upnp => ("upnp", "UPnP"),
    };
    match state {
        PortMapping::Mapped { addr, protocol } => json!({
            "kind": "mapped",
            "protocol": name(protocol).0,
            "addr": cfg.show(addr),
            "detail": format!("the router forwarded {} here ({}); removed again", cfg.show(addr), name(protocol).1),
        }),
        PortMapping::NotPublic { addr, protocol } => json!({
            "kind": "not-public",
            "protocol": name(protocol).0,
            "addr": cfg.show(addr),
            "detail": format!(
                "the router answered ({}), but its own address {} is not public: another NAT \
                 (carrier-grade, or a second router) sits beyond it, so a mapping can't help",
                name(protocol).1, addr.ip()
            ),
        }),
        PortMapping::Trying => json!({ "kind": "no-answer", "detail": "still asking the router after 15 s" }),
        _ => json!({
            "kind": "unavailable",
            "detail": "no PCP, NAT-PMP or UPnP answer from the router, or each refused \
                       (port mapping off, or not supported)",
        }),
    }
}

/// Whether the relay at `url` takes connections, and forwards an ephemeral event from
/// one to another subscribed to its random topic.
fn relay(url: &str, wait: Duration) -> Value {
    let (mut rx, mut tx) = (NostrSignaling::new([url]), NostrSignaling::new([url]));
    let topic = Topic::room("peerwire-doctor", &hex(&txid()));
    rx.subscribe(topic);
    let start = Instant::now();
    let open = |s: &NostrSignaling| s.stats().relays_open > 0;
    while !(open(&rx) && open(&tx)) && start.elapsed() < wait {
        std::thread::sleep(Duration::from_millis(5));
    }
    if !(open(&rx) && open(&tx)) {
        let detail = format!("no connection in {} s", wait.as_secs_f32());
        return json!({ "network": "nostr", "relay": url, "kind": "unreachable", "detail": detail });
    }
    let open_ms = start.elapsed().as_millis() as u64;
    // Again each second: the first can reach the relay before the subscription does.
    let mut sent: Vec<Instant> = Vec::new();
    while start.elapsed() < wait {
        if sent.last().is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
            tx.publish(topic, format!("peerwire doctor {}", sent.len()).as_bytes());
            sent.push(Instant::now());
        }
        while let Some((t, data)) = rx.poll() {
            let text = String::from_utf8_lossy(&data);
            let n = text.strip_prefix("peerwire doctor ").and_then(|n| n.parse::<usize>().ok());
            if let Some(at) = n.filter(|_| t == topic).and_then(|n| sent.get(n)) {
                let ms = at.elapsed().as_millis() as u64;
                return json!({
                    "network": "nostr", "relay": url, "kind": "reachable", "open_ms": open_ms, "round_trip_ms": ms,
                    "detail": format!("open in {open_ms} ms, an event came back in {ms} ms"),
                });
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let refused = if tx.stats().rejected > 0 { " (the relay refused it)" } else { "" };
    json!({
        "network": "nostr", "relay": url, "kind": "no-echo", "open_ms": open_ms,
        "detail": format!("open in {open_ms} ms, but our event didn't come back{refused}"),
    })
}

/// Whether a frame on the LAN group reaches a second carrier on this machine, that
/// is whether the interface takes multicast or broadcast. Other devices hear it only
/// if the network passes it between them, which one machine can't tell.
fn lan(cfg: &Config) -> Option<Value> {
    let (group, interface) = cfg.lan?;
    let result = |kind: &str, detail: String| json!({ "network": "lan", "group": group.to_string(), "kind": kind, "detail": detail });
    let carriers = LanSignaling::on(group, interface).and_then(|a| Ok((a, LanSignaling::on(group, interface)?)));
    let (mut a, mut b) = match carriers {
        Ok(c) => c,
        Err(e) => return Some(result("unavailable", format!("can't open the LAN carrier: {e}"))),
    };
    let topic = Topic::room("peerwire-doctor", &hex(&txid()));
    b.subscribe(topic);
    let start = Instant::now();
    a.publish(topic, b"peerwire doctor");
    while start.elapsed() < Duration::from_secs(2) {
        if b.poll().is_some_and(|(t, _)| t == topic) {
            let ms = start.elapsed().as_millis();
            let detail = format!(
                "a frame reached a second carrier on this machine in {ms} ms; \
                 other devices hear it if the network passes multicast or broadcast"
            );
            return Some(result("works-here", detail));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Some(result("silent", "no frame came back in 2 s: this interface passes no multicast or broadcast".into()))
}

/// A few lines on what the report means for links from here.
fn summary(r: &Value) -> Vec<String> {
    let kind = |v: &Value| v["kind"].as_str().unwrap_or("").to_string();
    let mut out: Vec<String> = Vec::new();
    out.push(match kind(&r["ipv4"]["mapping"]).as_str() {
        "no-nat" => "IPv4: a public address with no NAT: peers can dial in unless a firewall drops them".into(),
        "endpoint-independent" => "IPv4: an easy NAT (endpoint-independent mapping): hole punching works".into(),
        "address-dependent" | "address-and-port-dependent" | "dependent" | "pooled" => {
            "IPv4: a hard NAT: a direct link needs an easy NAT on the other side, a birthday punch or port \
             mapping; otherwise a relay peer carries it"
                .into()
        }
        "blocked" => "IPv4: no STUN answer: UDP looks blocked, so no direct or peer-relayed links over IPv4".into(),
        "no-route" => "IPv4: no route from this machine".into(),
        _ => "IPv4: too few STUN answers to tell".into(),
    });
    out.push(match kind(&r["ipv6"]["mapping"]).as_str() {
        "no-nat" | "endpoint-independent" => "IPv6: works: peers with IPv6 too can link directly over it".into(),
        _ => "IPv6: none working here: links use IPv4".into(),
    });
    match kind(&r["port_mapping"]).as_str() {
        "mapped" => out.push("Port mapping works: peers can reach the mapped port, whatever the NAT does".into()),
        "not-public" => out.push("Port mapping: the router is behind another NAT, so a mapping can't help".into()),
        _ => {}
    }
    let relays: Vec<&Value> =
        r["signaling"].as_array().into_iter().flatten().filter(|s| s["network"] == "nostr").collect();
    if !relays.is_empty() {
        let up = relays.iter().filter(|s| s["kind"] == "reachable").count();
        out.push(match up {
            0 => "Nostr: no relay works from here, so rooms over Nostr won't meet: use invites or the LAN".into(),
            n => format!("Nostr: {n} of {} relays work, so rooms can meet", relays.len()),
        });
    }
    out
}

/// The report as text that pastes well: one section per check.
pub fn render(r: &Value) -> String {
    let s = |v: &Value| v.as_str().map_or_else(|| v.to_string(), str::to_string);
    let mut out = format!("Peerwire doctor: {}, {} {}, {}\n", s(&r["tool"]), s(&r["os"]), s(&r["arch"]), s(&r["date"]));
    out += if r["redacted"] == true {
        "Nothing was uploaded. Public IP addresses are masked.\n"
    } else {
        "Nothing was uploaded. This report shows your public IP addresses: --redact masks them.\n"
    };
    for (name, key) in [("IPv4", "ipv4"), ("IPv6", "ipv6")] {
        let f = &r[key];
        out += &format!("\n{name}\n");
        row(&mut out, "local", &if f["local"].is_null() { "none".into() } else { s(&f["local"]) });
        for (i, a) in f["stun"].as_array().into_iter().flatten().enumerate() {
            let answer = match a["mapped"].as_str() {
                Some(m) => format!("{m} in {} ms", a["ms"]),
                None => "no answer".into(),
            };
            let label = if i == 0 { "STUN" } else { "" };
            row(&mut out, label, &format!("{} ({}) -> {answer}", s(&a["server"]), s(&a["addr"])));
        }
        row(&mut out, "mapping", &format!("{}: {}", s(&f["mapping"]["kind"]), s(&f["mapping"]["detail"])));
        if !f["filtering"].is_null() {
            row(&mut out, "filtering", &format!("{}: {}", s(&f["filtering"]["kind"]), s(&f["filtering"]["detail"])));
        }
    }
    let pm = &r["port_mapping"];
    out += "\nRouter port mapping\n";
    row(&mut out, "result", &format!("{}: {}", s(&pm["kind"]), s(&pm["detail"])));
    out += "\nSignaling\n";
    for sig in r["signaling"].as_array().into_iter().flatten() {
        let at = if sig["network"] == "lan" { &sig["group"] } else { &sig["relay"] };
        let label = format!("{} {}", s(&sig["network"]), s(at));
        writeln!(out, "  {label:<32} {}: {}", s(&sig["kind"]), s(&sig["detail"])).unwrap();
    }
    out += "\nIn short\n";
    for line in r["summary"].as_array().into_iter().flatten() {
        writeln!(out, "  - {}", s(line)).unwrap();
    }
    out
}

fn row(out: &mut String, label: &str, text: &str) {
    writeln!(out, "  {label:<12} {text}").unwrap();
}

/// Seconds since 1970 as an ISO 8601 UTC time (days to a civil date by Howard Hinnant's algorithm).
fn utc(secs: u64) -> String {
    let (days, rem) = ((secs / 86400) as i64, secs % 86400);
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem / 60 % 60, rem % 60)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unspecified(v6: bool) -> IpAddr {
    if v6 {
        Ipv6Addr::UNSPECIFIED.into()
    } else {
        Ipv4Addr::UNSPECIFIED.into()
    }
}

/// The address the default route leaves by (`connect` on UDP sends nothing).
fn primary_ip(v6: bool) -> Option<IpAddr> {
    let probe: SocketAddr = if v6 {
        (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1), 9).into()
    } else {
        (Ipv4Addr::new(192, 0, 2, 1), 9).into()
    };
    let s = UdpSocket::bind(SocketAddr::new(unspecified(v6), 0)).ok()?;
    s.connect(probe).ok()?;
    Some(s.local_addr().ok()?.ip()).filter(|ip| !ip.is_unspecified() && !ip.is_loopback())
}

/// 2000::/3, the global unicast range (not link-local, ULA or a documentation prefix).
fn is_global_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    s[0] & 0xe000 == 0x2000 && s[0..2] != [0x2001, 0x0db8]
}

/// An address on the internet, which `--redact` masks: not private, carrier-grade
/// NAT (100.64/10), loopback, link-local or ULA.
fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let [a, b, ..] = v.octets();
            let cgnat = a == 100 && b & 0xc0 == 64;
            !(v.is_private() || v.is_loopback() || v.is_link_local() || v.is_unspecified() || cgnat)
        }
        IpAddr::V6(v) => is_global_v6(v),
    }
}

/// Joins `room` over Nostr, waits for a peer, exchanges a ping each way and
/// reports the path.
#[allow(dead_code)]
fn meet(room: &str, v6: bool) -> Result<(), Box<dyn std::error::Error>> {
    let port = match std::env::var("DOCTOR_PORT") {
        Ok(p) => p.parse().map_err(|_| format!("DOCTOR_PORT: not a port: {p:?}"))?,
        Err(_) => 0,
    };
    let mut node = Node::builder().signaling(NostrSignaling::default()).bind((unspecified(v6), port))?;
    let start = Instant::now();
    while node.nat_type() == NatType::Unknown && start.elapsed() < Duration::from_secs(5) {
        tick(&mut node);
    }
    println!("\nRoom");
    println!("  peer id:  {}", node.peer_id());
    println!("  NAT type: {:?} (from {})", node.nat_type(), DEFAULT_STUN_SERVERS.join(", "));
    println!("  public:   {:?}", node.public_addr());
    println!("  socket:   {} ({})", node.local_addr(), if v6 { "IPv6" } else { "IPv4" });
    println!("  joining room {room:?}; run the same command on the other machine (Ctrl-C to stop)");
    // The app name of natcheck, which the doctor was: a peer on an older build still meets this one.
    node.join_room("natcheck", room);
    let t0 = Instant::now();
    loop {
        tick(&mut node);
        while let Some(event) = node.try_recv_event() {
            let at = t0.elapsed().as_secs_f32();
            match event {
                Event::Connected { peer, path, .. } => {
                    println!("[{at:6.1}s] CONNECTED to {peer}: {}", describe(&path));
                    node.send(&peer, Lane::Reliable, format!("ping from {}", node.peer_id()).as_bytes())?;
                }
                Event::PathChanged { peer, path } => println!("[{at:6.1}s] path to {peer} now {}", describe(&path)),
                Event::Message { peer, data, .. } => {
                    println!("[{at:6.1}s] message from {peer}: {}", String::from_utf8_lossy(&data));
                    if data.starts_with(b"ping") {
                        node.send(&peer, Lane::Reliable, b"pong")?;
                    }
                }
                Event::ConnectFailed { peer } => {
                    println!("[{at:6.1}s] FAILED to reach {peer}: {:?}", node.failure(&peer));
                }
                Event::Disconnected { peer, reason } => println!("[{at:6.1}s] {peer} left: {reason:?}"),
                Event::AddrObserved { by, addr } => println!("[{at:6.1}s] {by} sees us at {addr}"),
            }
        }
    }
}

#[allow(dead_code)]
fn describe(path: &Path) -> String {
    match path {
        Path::Direct(a) => format!("DIRECT via {a}"),
        Path::Via(p) => format!("RELAYED through peer {p}"),
    }
}

#[allow(dead_code)]
fn tick(node: &mut Node) {
    node.poll();
    std::thread::sleep(node.next_timeout().unwrap_or(Duration::MAX).min(Duration::from_millis(5)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    const G1: &str = "74.125.250.129:19302";
    const C1: &str = "162.159.207.0:3478";
    const G2: &str = "74.125.250.129:3478";
    const C2: &str = "162.159.207.0:53";
    const LOCAL: &str = "192.168.1.5:40000";

    #[test]
    fn home_wifi_field_result_is_symmetric_with_one_delta() {
        // tsk_vug4qr3b: Google saw :1369, Cloudflare :19000.
        let m = [(a(G1), a("203.145.57.120:1369")), (a(C1), a("203.145.57.120:19000"))];
        let (kind, v) = verdict(a(LOCAL), &m);
        assert_eq!(kind, "dependent");
        assert!(v.starts_with("symmetric;"), "{v}");
        assert!(v.contains("too few"), "{v}");
    }

    #[test]
    fn port_dependent_random() {
        let m = [
            (a(G1), a("203.0.113.9:1369")),
            (a(C1), a("203.0.113.9:19000")),
            (a(G2), a("203.0.113.9:40211")),
            (a(C2), a("203.0.113.9:7777")),
        ];
        let (kind, v) = verdict(a(LOCAL), &m);
        assert_eq!(kind, "address-and-port-dependent");
        assert!(v.contains("address-and-port-dependent") && v.contains("random"), "{v}");
    }

    #[test]
    fn address_dependent_two_ports() {
        let m = [
            (a(G1), a("203.0.113.9:5000")),
            (a(C1), a("203.0.113.9:5001")),
            (a(G2), a("203.0.113.9:5000")),
            (a(C2), a("203.0.113.9:5001")),
        ];
        let (kind, v) = verdict(a(LOCAL), &m);
        assert_eq!(kind, "address-dependent");
        assert!(v.contains("address-dependent (one port") && v.contains("too few"), "{v}");
    }

    #[test]
    fn field_hotspot_pool_keeps_the_port() {
        // Jio hotspot, ab831e5 run: port kept, one destination saw another pool IP.
        let m = [
            (a(G1), a("152.58.117.228:60241")),
            (a(C1), a("152.58.117.228:60241")),
            (a(C2), a("152.58.117.166:60241")),
        ];
        let (kind, v) = verdict(a("10.129.132.88:60241"), &m);
        assert_eq!(kind, "pooled");
        assert!(v.starts_with("port kept for every destination (preserved)") && v.contains("2 addresses"), "{v}");
    }

    #[test]
    fn field_home_wifi_is_port_dependent_random() {
        let m = [
            (a(G1), a("203.145.57.120:32679")),
            (a(C1), a("203.145.57.120:5342")),
            (a(G2), a("203.145.57.120:24211")),
            (a("162.159.207.0:443"), a("203.145.57.120:8499")),
        ];
        let (kind, v) = verdict(a("192.168.1.35:45973"), &m);
        assert_eq!(kind, "address-and-port-dependent");
        assert!(v.contains("random"), "{v}");
    }

    #[test]
    fn cone_open_and_empty() {
        let cone = [(a(G1), a("203.0.113.9:40000")), (a(C1), a("203.0.113.9:40000"))];
        let (kind, v) = verdict(a(LOCAL), &cone);
        assert!(kind == "endpoint-independent" && v.ends_with("port preserved"), "{v}");
        let local = a("[2001:4860::5]:40000");
        let open = [(a(G1), local), (a(C1), local)];
        assert_eq!(verdict(local, &open).0, "no-nat");
        assert_eq!(verdict(a(LOCAL), &[]).0, "blocked");
        assert_eq!(verdict(a(LOCAL), &cone[..1]).0, "unknown");
        assert!(!is_global_v6("fe80::1".parse().unwrap()) && !is_global_v6("fd00::1".parse().unwrap()));
        assert!(is_global_v6("2409:4060::1".parse().unwrap()) && !is_global_v6("2001:db8::1".parse().unwrap()));
    }

    #[test]
    fn redact_masks_public_addresses_only() {
        let cfg = Config { redact: true, ..Config::default() };
        assert_eq!(cfg.show(a("203.0.113.9:40000")), "203.0.x.x:40000");
        assert_eq!(cfg.show(a("[2409:4060::1]:5")), "[2409:4060:x:x:x:x:x:x]:5");
        for private in ["192.168.1.5:1", "10.0.0.1:2", "100.72.0.1:3", "127.0.0.1:4", "[fd00::1]:5", "[fe80::1]:6"] {
            assert_eq!(cfg.show(a(private)), private);
        }
        assert_eq!(Config::default().show(a("203.0.113.9:40000")), "203.0.113.9:40000");
    }

    #[test]
    fn utc_dates() {
        assert_eq!(utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc(1_793_875_203), "2026-11-05T10:40:03Z");
    }
}

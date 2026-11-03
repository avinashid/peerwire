//! Asks public STUN servers, from one socket, for our public address. The same answer
//! from servers at two IPs means our NAT maps endpoint-independently (cone), so hole
//! punching can work; different ports mean a symmetric NAT, one that maps per
//! destination IP if one server's two ports saw one mapping (NB-083). Each name is
//! resolved once, so its ports share an IP. Default: the servers `peerwire` asks.
//!
//! cargo run -p peerwire-core --example stun_probe [-- host:port ...]

use peerwire_core::stun;
use rand_core::{OsRng, RngCore};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Duration;

fn main() -> std::io::Result<()> {
    let mut servers: Vec<String> = std::env::args().skip(1).collect();
    if servers.is_empty() {
        servers = ["stun.l.google.com:19302", "stun.cloudflare.com:3478", "stun.l.google.com:3478"]
            .map(String::from)
            .to_vec();
    }
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_read_timeout(Some(Duration::from_secs(2)))?;
    println!("local socket {}", sock.local_addr()?);
    let mut names: Vec<(&str, IpAddr)> = Vec::new();
    let mut seen: Vec<(SocketAddr, SocketAddr)> = Vec::new();
    for server in &servers {
        let (name, port) = server.rsplit_once(':').unwrap_or((server, ""));
        let known = names.iter().find(|(n, _)| *n == name).map(|&(_, ip)| ip);
        let addr = match (known, port.parse()) {
            (Some(ip), Ok(port)) => SocketAddr::new(ip, port),
            _ => match server.to_socket_addrs()?.find(|a| a.is_ipv4()) {
                Some(a) => a,
                None => {
                    println!("{server}: no IPv4 address");
                    continue;
                }
            },
        };
        if known.is_none() {
            names.push((name, addr.ip()));
        }
        let mut tx = [0u8; 12];
        OsRng.fill_bytes(&mut tx);
        let mut answer = None;
        for _ in 0..3 {
            sock.send_to(&stun::binding_request(&tx), addr)?;
            let mut buf = [0u8; 1500];
            match sock.recv_from(&mut buf) {
                Ok((n, from)) if from == addr => {
                    answer = stun::parse_binding_response(&buf[..n], &tx);
                    if answer.is_some() {
                        break;
                    }
                }
                _ => {}
            }
        }
        println!("{server} ({addr}): we are {answer:?}");
        seen.extend(answer.map(|a| (addr, a)));
    }
    // Whether two answers agree, from two ports of one IP and from two IPs.
    let (mut port_same, mut port_differs, mut ip_same, mut ip_differs) = (false, false, false, false);
    for (i, (s, m)) in seen.iter().enumerate() {
        for (t, n) in &seen[i + 1..] {
            match (s.ip() == t.ip(), m == n) {
                (true, true) => port_same = true,
                (true, false) => port_differs = true,
                (false, true) => ip_same = true,
                (false, false) => ip_differs = true,
            }
        }
    }
    if port_differs {
        println!("different mappings for one IP's two ports: symmetric NAT, a new port per IP and port");
    } else if ip_differs && port_same {
        println!("one mapping per server IP: symmetric NAT that maps per destination IP");
    } else if ip_differs {
        println!("different mappings: symmetric NAT, expect relayed links");
    } else if ip_same {
        println!("same mapping from every server: endpoint-independent (cone) NAT or no NAT");
    } else {
        println!("not enough answers from two IPs to classify");
    }
    Ok(())
}

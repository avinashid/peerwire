//! NAT behaviour, following the RFC 4787 vocabulary.
//!
//! A NAT joins an inside realm to an outside realm (the internet, or another
//! NAT's inside, which is how carrier-grade NAT is modelled). Outbound packets
//! create or refresh a mapping; inbound packets pass only if a live mapping
//! exists and its filter admits the sender.

use crate::SimAddr;
use peerwire_core::Instant;
use std::collections::HashMap;
use std::time::Duration;

/// Which destinations share one external port (mapping) or may send back
/// through it (filtering).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dependence {
    /// Any destination: "full cone" / endpoint-independent.
    Endpoint,
    /// Per destination IP.
    Address,
    /// Per destination IP and port: "symmetric" when used for mapping.
    AddressPort,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortAlloc {
    /// Keep the inside port if free, else the next free one.
    Preserve,
    /// The next port after the last one handed out.
    Sequential,
    Random,
}

#[derive(Clone, Debug)]
pub struct NatConfig {
    pub mapping: Dependence,
    pub filtering: Dependence,
    pub alloc: PortAlloc,
    /// Idle time after which a mapping is forgotten.
    pub timeout: Duration,
    /// Whether inside hosts can reach each other through the outside address.
    pub hairpin: bool,
}

impl NatConfig {
    /// Endpoint-independent mapping and filtering.
    pub fn full_cone() -> Self {
        NatConfig::new(Dependence::Endpoint, Dependence::Endpoint, PortAlloc::Preserve)
    }
    /// Endpoint-independent mapping, address-dependent filtering.
    pub fn restricted_cone() -> Self {
        NatConfig::new(Dependence::Endpoint, Dependence::Address, PortAlloc::Preserve)
    }
    /// Endpoint-independent mapping, address-and-port-dependent filtering. The
    /// most common home router behaviour.
    pub fn port_restricted() -> Self {
        NatConfig::new(Dependence::Endpoint, Dependence::AddressPort, PortAlloc::Preserve)
    }
    /// A new port per destination, allocated in sequence (predictable), letting in
    /// only the address and port a mapping was made for.
    pub fn symmetric_sequential() -> Self {
        NatConfig::new(Dependence::AddressPort, Dependence::AddressPort, PortAlloc::Sequential)
    }
    /// A new random port per destination, letting in only the address and port a
    /// mapping was made for.
    pub fn symmetric_random() -> Self {
        NatConfig::new(Dependence::AddressPort, Dependence::AddressPort, PortAlloc::Random)
    }
    /// One random port per destination IP (address-dependent mapping), letting in
    /// only the addresses and ports a mapping has sent to. STUN asks two servers at
    /// different IPs, so it looks symmetric.
    pub fn address_dependent() -> Self {
        NatConfig::new(Dependence::Address, Dependence::AddressPort, PortAlloc::Random)
    }
    /// The same NAT letting in packets from any port of an IP a mapping has sent to
    /// (address-dependent filtering), as in `symmetric_random().address_filtered()`.
    /// `address_dependent().address_filtered()` is a restricted cone towards each IP.
    pub fn address_filtered(self) -> Self {
        NatConfig { filtering: Dependence::Address, ..self }
    }

    pub fn new(mapping: Dependence, filtering: Dependence, alloc: PortAlloc) -> Self {
        NatConfig { mapping, filtering, alloc, timeout: Duration::from_secs(30), hairpin: false }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct MapKey {
    inside: SimAddr,
    /// The destination part the mapping depends on (zeroed where it doesn't).
    dst: SimAddr,
}

struct Mapping {
    inside: SimAddr,
    port: u16,
    /// Outside addresses this mapping has sent to.
    sent_to: Vec<SimAddr>,
    last_used: Instant,
}

pub(crate) struct Nat {
    pub(crate) config: NatConfig,
    pub(crate) ip: u32,
    pub(crate) outer: usize,
    pub(crate) inner: usize,
    by_key: HashMap<MapKey, u16>,
    by_port: HashMap<u16, Mapping>,
    next_port: u16,
    pub(crate) stats: NatStats,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NatStats {
    pub mappings_created: u64,
    pub inbound_filtered: u64,
    pub inbound_no_mapping: u64,
    pub hairpin_blocked: u64,
}

fn masked(d: Dependence, dst: SimAddr) -> SimAddr {
    match d {
        Dependence::Endpoint => SimAddr::new(0, 0),
        Dependence::Address => SimAddr::new(dst.ip, 0),
        Dependence::AddressPort => dst,
    }
}

impl Nat {
    pub(crate) fn new(config: NatConfig, ip: u32, outer: usize, inner: usize) -> Self {
        Nat {
            config,
            ip,
            outer,
            inner,
            by_key: HashMap::new(),
            by_port: HashMap::new(),
            next_port: 20_000,
            stats: NatStats::default(),
        }
    }

    fn live(&self, port: u16, now: Instant) -> bool {
        self.by_port.get(&port).is_some_and(|m| now < m.last_used + self.config.timeout)
    }

    /// Translates an outbound packet's source; returns the outside source address.
    pub(crate) fn outbound(&mut self, now: Instant, src: SimAddr, dst: SimAddr, rand: u64) -> SimAddr {
        let key = MapKey { inside: src, dst: masked(self.config.mapping, dst) };
        let port = match self.by_key.get(&key) {
            Some(&p) if self.live(p, now) => p,
            _ => {
                if let Some(p) = self.by_key.remove(&key) {
                    self.by_port.remove(&p);
                }
                let p = self.alloc(now, src.port, rand);
                self.by_key.insert(key, p);
                self.by_port.insert(p, Mapping { inside: src, port: p, sent_to: Vec::new(), last_used: now });
                self.stats.mappings_created += 1;
                p
            }
        };
        let m = self.by_port.get_mut(&port).expect("mapping");
        m.last_used = now;
        if !m.sent_to.contains(&dst) {
            m.sent_to.push(dst);
        }
        SimAddr::new(self.ip, m.port)
    }

    fn alloc(&mut self, now: Instant, want: u16, rand: u64) -> u16 {
        let free = |n: &Nat, p: u16| p >= 1024 && !n.live(p, now);
        let mut p = match self.config.alloc {
            PortAlloc::Preserve if free(self, want) => return want,
            PortAlloc::Preserve | PortAlloc::Sequential => self.next_port,
            PortAlloc::Random => 1024 + (rand % (65536 - 1024)) as u16,
        };
        while !free(self, p) {
            p = if p == u16::MAX { 1024 } else { p + 1 };
        }
        if self.config.alloc != PortAlloc::Random {
            self.next_port = if p == u16::MAX { 1024 } else { p + 1 };
        }
        p
    }

    /// Translates an inbound packet to the outside address `dst`; returns the
    /// inside destination, or `None` if it is filtered.
    pub(crate) fn inbound(&mut self, now: Instant, src: SimAddr, dst: SimAddr) -> Option<SimAddr> {
        if !self.live(dst.port, now) {
            self.stats.inbound_no_mapping += 1;
            return None;
        }
        let m = self.by_port.get_mut(&dst.port).expect("live mapping");
        let ok = match self.config.filtering {
            Dependence::Endpoint => true,
            Dependence::Address => m.sent_to.iter().any(|a| a.ip == src.ip),
            Dependence::AddressPort => m.sent_to.contains(&src),
        };
        if !ok {
            self.stats.inbound_filtered += 1;
            return None;
        }
        m.last_used = now;
        Some(m.inside)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each kind's RFC 4787 behaviour: whether one inside socket keeps its port
    /// towards another port of the same IP, and towards another IP; and whether the
    /// mapping it made for `x:1` lets in `x:2`, and `y:1`.
    #[test]
    fn mapping_and_filtering_per_kind() {
        let (x1, x2, y1) = (SimAddr::new(0x0b00_0001, 1), SimAddr::new(0x0b00_0001, 2), SimAddr::new(0x0b00_0002, 1));
        let inside = SimAddr::new(0x0a00_0002, 5000);
        let kinds = [
            ("full-cone", NatConfig::full_cone(), (true, true), (true, true)),
            ("restricted", NatConfig::restricted_cone(), (true, true), (true, false)),
            ("port-restr", NatConfig::port_restricted(), (true, true), (false, false)),
            ("sym-seq", NatConfig::symmetric_sequential(), (false, false), (false, false)),
            ("sym-random", NatConfig::symmetric_random(), (false, false), (false, false)),
            ("sym-seq-af", NatConfig::symmetric_sequential().address_filtered(), (false, false), (true, false)),
            ("sym-rand-af", NatConfig::symmetric_random().address_filtered(), (false, false), (true, false)),
            ("sym-ip-adf", NatConfig::address_dependent().address_filtered(), (true, false), (true, false)),
            ("sym-ip-apdf", NatConfig::address_dependent(), (true, false), (false, false)),
        ];
        for (name, config, keeps, lets_in) in kinds {
            let now = Instant::ZERO;
            let mut nat = Nat::new(config, 0x0b00_0100, 0, 1);
            let p1 = nat.outbound(now, inside, x1, 7).port;
            let got = (nat.outbound(now, inside, x2, 8).port == p1, nat.outbound(now, inside, y1, 9).port == p1);
            assert_eq!(got, keeps, "{name}: same port towards x:2, y:1");
            // A fresh NAT, so that only the mapping for x:1 exists.
            let mut nat = Nat::new(nat.config.clone(), 0x0b00_0100, 0, 1);
            let to = SimAddr::new(nat.ip, nat.outbound(now, inside, x1, 7).port);
            let got = (nat.inbound(now, x2, to) == Some(inside), nat.inbound(now, y1, to) == Some(inside));
            assert_eq!(got, lets_in, "{name}: x:1's mapping lets in x:2, y:1");
            assert_eq!(nat.inbound(now, x1, to), Some(inside), "{name}: the address it sent to");
        }
        // Sequential ports come one after another; random ones are drawn.
        let mut nat = Nat::new(NatConfig::symmetric_sequential().address_filtered(), 0x0b00_0100, 0, 1);
        let ports: Vec<u16> = [x1, x2, y1].iter().map(|d| nat.outbound(Instant::ZERO, inside, *d, 0).port).collect();
        assert_eq!(ports, [20_000, 20_001, 20_002]);
    }
}

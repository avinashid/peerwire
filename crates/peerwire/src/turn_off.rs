//! Without feature `turn`: no TURN client, and the traversal's TURN calls do nothing.

use peerwire_core::{Instant, Transmit};
use std::marker::PhantomData;
use std::net::{IpAddr, SocketAddr};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // as the real one's, which the traversal matches on
pub(crate) enum TurnEvent {
    Allocated(SocketAddr),
    Lost,
    Refused(Option<u16>),
}

pub(crate) struct TurnSet<A>(PhantomData<A>);

impl<A> TurnSet<A> {
    pub fn new(_unix_ms: u64, _seed: [u8; 32]) -> TurnSet<A> {
        TurnSet(PhantomData)
    }
    pub fn has_servers(&self) -> bool {
        false
    }
    pub fn set_on(&mut self, _now: Instant, _on: bool) {}
    pub fn relayed(&self) -> Vec<A> {
        Vec::new()
    }
    pub fn up(&self) -> Vec<(A, SocketAddr)> {
        Vec::new()
    }
    pub fn permit(&mut self, _now: Instant, _ips: &[IpAddr]) {}
    pub fn handle(&mut self, _now: Instant, _from: &A, _data: &[u8]) -> Option<Option<(A, Vec<u8>)>> {
        None
    }
    pub fn send(&mut self, _now: Instant, _dst: &A, _payload: &[u8]) -> bool {
        false
    }
    pub fn poll_transmit(&mut self) -> Option<Transmit<A>> {
        None
    }
    pub fn poll_timeout(&self) -> Option<Instant> {
        None
    }
    pub fn handle_timeout(&mut self, _now: Instant) {}
    pub fn poll_event(&mut self) -> Option<(A, TurnEvent)> {
        None
    }
}

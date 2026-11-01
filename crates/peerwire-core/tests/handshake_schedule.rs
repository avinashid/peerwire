//! Pins the resend schedule that `Config::handshake_retry`'s doc and docs/reference/rust.md give.

use peerwire_core::{Config, Endpoint, Event, Identity, Instant};
use rand_chacha::rand_core::SeedableRng;
use std::net::SocketAddr;

/// A dial nobody answers, with the default `Config`: HS1 goes out at 0, 0.5, 1.5, 3.5, 7.5 and 15.5 s, each
/// wait double the last, and the dial fails at 23.5 s.
#[test]
fn unanswered_dial_resends_then_fails_at_23_5_s() {
    let mut rng = rand_chacha::ChaCha20Rng::seed_from_u64(1);
    let (me, them) = (Identity::generate(&mut rng), Identity::generate(&mut rng));
    let mut ep: Endpoint<SocketAddr> = Endpoint::new(&me, Config::default(), [3; 32]);
    let addr: SocketAddr = "198.51.100.1:9".parse().unwrap();
    let mut now = Instant::from_millis(0);
    ep.connect(now, addr, them.peer_id()).unwrap();
    let (mut sends, mut failed) = (Vec::new(), None);
    // Bounded, so a dial that never fails fails the test rather than hanging it.
    for _ in 0..1000 {
        while let Some(t) = ep.poll_transmit(now) {
            assert_eq!(t.dst, addr);
            sends.push(now.as_micros() / 1000);
        }
        while let Some(e) = ep.poll_event() {
            if matches!(e, Event::ConnectFailed { .. }) {
                failed = Some(now.as_micros() / 1000);
            }
        }
        if failed.is_some() {
            break;
        }
        let Some(next) = ep.poll_timeout() else { break };
        now = next;
        ep.handle_timeout(now);
    }
    assert_eq!(sends, [0, 500, 1500, 3500, 7500, 15500]);
    assert_eq!(failed, Some(23500));
}

//! A Bluetooth LE probe (NB-110c): what NimBLE costs next to a bound Node with Wi-Fi off, how long the chip takes to
//! answer a handshake, and a GATT echo to try from a laptop or a phone. It advertises as `pw-ble` with the service of
//! docs/concepts/bluetooth.md from a non-resolvable random address, sends each write to `rx` back as a notification
//! on `tx`, and logs the free heap, the lowest it has been and the connections every 5 s.
//!
//! The name is only so a person can find the probe; the real peer advertises no name (BLE-R1).
//! Build it with `FEATURES=ble EXAMPLE=ble_probe ./build.sh`, then flash out/ble_probe.bin.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use anyhow::{anyhow, ensure};
use esp32_nimble::enums::{AuthReq, OwnAddrType};
use esp32_nimble::{uuid128, BLEAdvertisementData, BLEDevice, NimbleProperties};
use esp_idf_svc::sys;
use log::info;
use peerwire_core::{Endpoint, Identity};
use peerwire_esp::peerwire::Node;
use peerwire_esp::{config, free_heap, PORT};

fn main() {
    peerwire_esp::run(|| {
        let boot = free_heap();
        info!("ble probe: boot, free heap {boot} B");

        // The Node's socket needs lwIP, which Wi-Fi would start: start it alone, and bind to loopback.
        sys::esp!(unsafe { sys::esp_netif_init() })?;
        let mut node = Node::builder()
            .config(config())
            .stun_servers(Vec::<String>::new())
            .bind(SocketAddr::from(([127, 0, 0, 1], PORT)))?;
        let with_node = free_heap();
        info!("ble probe: Node {} bound, it took {} B, free heap {with_node} B", node.peer_id(), boot - with_node);

        // Before NimBLE, so QEMU (which can't start the radio) runs it too. Its keys are thrown away.
        let handshakes = time_handshakes(6)?;

        let ble = BLEDevice::take();
        ble.set_preferred_mtu(256)?;
        // A fresh non-resolvable private address, never the efuse MAC (BLE-R3). esp32-nimble turns NRPA on only for
        // the original ESP32, so ask NimBLE for one here.
        let mut addr = sys::ble_addr_t { type_: 0, val: [0; 6] };
        let rc = unsafe { sys::ble_hs_id_gen_rnd(1, &mut addr) };
        ensure!(rc == 0, "ble_hs_id_gen_rnd: {rc}");
        let rc = unsafe { sys::ble_hs_id_set_rnd(addr.val.as_ptr()) };
        ensure!(rc == 0, "ble_hs_id_set_rnd: {rc}");
        ble.set_own_addr_type(OwnAddrType::Random);
        // No bonding, MITM or Secure Connections (BLE-R5): a central that pairs anyway gets legacy Just Works, which
        // runs no P-256, and nothing is stored.
        ble.security().set_auth(AuthReq::empty());
        let with_ble = free_heap();
        info!(
            "ble probe: NimBLE up, own address non-resolvable random, it took {} B, free heap {with_ble} B",
            with_node - with_ble
        );

        let conns = Arc::new(AtomicU32::new(0));
        let (up, down) = (conns.clone(), conns.clone());
        let server = ble.get_server();
        server.on_connect(move |_, _| {
            up.fetch_add(1, Ordering::Relaxed);
            info!("ble probe: a central connected, free heap {} B", free_heap());
        });
        server.on_disconnect(move |_, reason| {
            down.fetch_sub(1, Ordering::Relaxed);
            info!("ble probe: a central left ({reason:?}), free heap {} B", free_heap());
        });

        let service = server.create_service(uuid128!("1c3eea41-c2be-493c-ab0b-7707ad2dbed6"));
        let tx = service
            .lock()
            .create_characteristic(uuid128!("1c3eea43-c2be-493c-ab0b-7707ad2dbed6"), NimbleProperties::NOTIFY);
        let rx = service.lock().create_characteristic(
            uuid128!("1c3eea42-c2be-493c-ab0b-7707ad2dbed6"),
            NimbleProperties::WRITE_NO_RSP | NimbleProperties::WRITE,
        );
        // NimBLE calls back on its own task: hand writes to the loop, which notifies them back.
        let (echo, writes) = mpsc::channel::<Vec<u8>>();
        rx.lock().on_write(move |args| {
            let (len, mtu) = (args.recv_data().len(), args.desc().mtu());
            // One write carries at most MTU - 3 bytes. NimBLE hands over a prepared (long) write whole, so its length
            // is what gives it away (BLE-R7); sdkconfig.ble leaves the stack no room to queue one anyway.
            if len > usize::from(mtu.saturating_sub(3)) {
                info!("ble probe: refused a {len} B write, MTU {mtu}");
                args.reject();
                return;
            }
            info!("ble probe: {len} B written, MTU {mtu}");
            let _ = echo.send(args.recv_data().to_vec());
        });

        let advertising = ble.get_advertising();
        advertising.lock().set_data(
            BLEAdvertisementData::new()
                .name("pw-ble")
                .add_service_uuid(uuid128!("1c3eea41-c2be-493c-ab0b-7707ad2dbed6")),
        )?;
        advertising.lock().start()?;
        let advertised = free_heap();
        info!("ble probe: advertising as pw-ble, GATT took {} B, free heap {advertised} B", with_ble - advertised);
        let startup = format!(
            "boot {boot} B, Node took {} B, {handshakes}, NimBLE took {} B, GATT took {} B",
            boot - with_node,
            with_node - with_ble,
            with_ble - advertised
        );

        let (mut next, mut reports) = (Instant::now(), 0u32);
        loop {
            node.poll();
            while let Ok(data) = writes.try_recv() {
                tx.lock().set_value(&data).notify();
            }
            if Instant::now() >= next {
                next += Duration::from_secs(5);
                let lowest = unsafe { sys::esp_get_minimum_free_heap_size() };
                let n = conns.load(Ordering::Relaxed);
                info!("ble probe: {n} connected, free heap {} B, lowest {lowest} B", free_heap());
                // The console over USB loses what the chip prints in the seconds after a reset, until the host opens
                // the port again (NB-110c's first board run): say the startup numbers again, for 30 s then each minute.
                if reports < 6 || reports % 12 == 0 {
                    info!("ble probe: startup: {startup}");
                }
                reports += 1;
            }
            peerwire_esp::idle(&node);
        }
    })
}

/// Times the answering side of a Noise IK handshake on this chip, the CPU cost that BLE-R9's HS1 budget is stated
/// in: an HS1 in and the HS2 out, between two endpoints in memory with the port's caps. Each round is a new dialer
/// from a new address, and is linked to the end outside the timing, so nothing stays half open. Returns a summary.
fn time_handshakes(rounds: u16) -> anyhow::Result<String> {
    let key = identity();
    let mut answerer: Endpoint<SocketAddr> = Endpoint::new(&key, config(), random());
    let at = SocketAddr::from(([10, 0, 0, 1], PORT));
    let (mut times, mut links) = (Vec::new(), 0);
    for round in 0..rounds {
        let now = peerwire_core::Instant::from_millis(u64::from(round) * 1000);
        let from = SocketAddr::from(([10, 0, 1, 1], 5000 + round));
        let dialer_key = identity();
        let mut dialer: Endpoint<SocketAddr> = Endpoint::new(&dialer_key, config(), random());
        dialer.connect(now, at, key.peer_id())?;
        let hs1 = dialer.poll_transmit(now).ok_or_else(|| anyhow!("no HS1"))?;
        let start = Instant::now();
        answerer.handle_datagram(now, from, &hs1.payload);
        let hs2 = answerer.poll_transmit(now).ok_or_else(|| anyhow!("no HS2"))?;
        let took = start.elapsed();
        dialer.handle_datagram(now, at, &hs2.payload);
        for _ in 0..8 {
            let mut quiet = true;
            while let Some(t) = dialer.poll_transmit(now) {
                answerer.handle_datagram(now, from, &t.payload);
                quiet = false;
            }
            while let Some(t) = answerer.poll_transmit(now) {
                dialer.handle_datagram(now, at, &t.payload);
                quiet = false;
            }
            while dialer.poll_event().is_some() || answerer.poll_event().is_some() {}
            if quiet {
                break;
            }
        }
        let linked = answerer.connected_peers().any(|p| *p == dialer_key.peer_id());
        info!("ble probe: handshake {round}: HS1 in to HS2 out took {} us, linked {linked}", took.as_micros());
        times.push(took);
        links += usize::from(linked);
    }
    times.sort();
    let us = |d: &Duration| d.as_micros();
    let summary = format!(
        "handshake HS1 in to HS2 out min {} us, median {} us, max {} us, {links}/{rounds} linked",
        us(&times[0]),
        us(&times[times.len() / 2]),
        us(&times[times.len() - 1])
    );
    info!("ble probe: {summary}, free heap {} B", free_heap());
    Ok(summary)
}

fn identity() -> Identity {
    Identity::from_secret_bytes(&random())
}

fn random() -> [u8; 32] {
    let mut bytes = [0; 32];
    unsafe { sys::esp_fill_random(bytes.as_mut_ptr().cast(), bytes.len()) };
    bytes
}

//! A Bluetooth LE probe (NB-110c): what NimBLE costs next to a bound Node with Wi-Fi off, and a GATT echo to try
//! from a laptop or a phone. It advertises as `pw-ble` with the service below, sends each write to `rx` back as a
//! notification on `tx`, and logs the free heap, the lowest it has been and the connections every 5 s.
//!
//! The UUIDs are placeholders until docs/concepts/bluetooth.md (NB-110a) fixes the real ones.
//! Build it with `FEATURES=ble EXAMPLE=ble_probe ./build.sh`, then flash out/ble_probe.bin.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use esp32_nimble::{uuid128, BLEAdvertisementData, BLEDevice, NimbleProperties};
use esp_idf_svc::sys;
use log::info;
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

        let ble = BLEDevice::take();
        ble.set_preferred_mtu(517)?;
        let with_ble = free_heap();
        info!("ble probe: NimBLE up, it took {} B, free heap {with_ble} B", with_node - with_ble);

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

        let service = server.create_service(uuid128!("7d1a987e-c2f8-4c94-b720-8aa3df32b6ef"));
        let tx = service
            .lock()
            .create_characteristic(uuid128!("99f01448-0f04-4622-8bca-0f48ec22ea49"), NimbleProperties::NOTIFY);
        let rx = service.lock().create_characteristic(
            uuid128!("588d4fa0-84c4-4f81-b055-53190bbc6354"),
            NimbleProperties::WRITE_NO_RSP | NimbleProperties::WRITE,
        );
        // NimBLE calls back on its own task: hand writes to the loop, which notifies them back.
        let (echo, writes) = mpsc::channel::<Vec<u8>>();
        rx.lock().on_write(move |args| {
            info!("ble probe: {} B written, MTU {}", args.recv_data().len(), args.desc().mtu());
            let _ = echo.send(args.recv_data().to_vec());
        });

        let advertising = ble.get_advertising();
        advertising.lock().set_data(
            BLEAdvertisementData::new()
                .name("pw-ble")
                .add_service_uuid(uuid128!("7d1a987e-c2f8-4c94-b720-8aa3df32b6ef")),
        )?;
        advertising.lock().start()?;
        let advertised = free_heap();
        info!("ble probe: advertising as pw-ble, GATT took {} B, free heap {advertised} B", with_ble - advertised);

        let mut next = Instant::now();
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
            }
            peerwire_esp::idle(&node);
        }
    })
}

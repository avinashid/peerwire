fn main() {
    embuild::espidf::sysenv::output();
    // The chip's name is a cfg on its own build only (src/ble.rs has `cfg(esp32)`), so declared for every chip.
    println!("cargo::rustc-check-cfg=cfg(esp32)");
    // Likewise ESP-IDF's, set where the chip has USB-Serial-JTAG (src/console.rs).
    println!("cargo::rustc-check-cfg=cfg(esp_idf_soc_usb_serial_jtag_supported)");
}

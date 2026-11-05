// ESP-IDF's link arguments, and cfg.toml's settings as CFG_WIFI_SSID, CFG_WIFI_PASSWORD and CFG_ROOM.
fn main() {
    embuild::espidf::sysenv::output();
    println!("cargo:rerun-if-changed=cfg.toml");
    let cfg = std::fs::read_to_string("cfg.toml").expect("cfg.toml is missing: copy cfg.toml.example to cfg.toml");
    let cfg: toml::Table = cfg.parse().expect("cfg.toml isn't valid TOML");
    for key in ["wifi_ssid", "wifi_password", "room"] {
        let value = cfg.get(key).and_then(|v| v.as_str()).unwrap_or_else(|| panic!("cfg.toml has no {key}"));
        assert!(!value.contains('\n'), "cfg.toml's {key} has a line break");
        println!("cargo:rustc-env=CFG_{}={value}", key.to_uppercase());
    }
}

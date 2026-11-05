# {{project-name}}

A [Peerwire](https://avinashid.github.io/peerwire/) node on an ESP32: it joins your Wi-Fi,
sets its clock, joins the room in `cfg.toml` and sends back every message a peer sends
it. `src/main.rs` is the whole app; [`peerwire-esp`](https://crates.io/crates/peerwire-esp)
does the rest.

## Build and flash

```sh
./flash.sh
```

That builds the firmware, flashes the board on USB and opens its log (Ctrl+C leaves it).
`./flash.sh build` only builds; options after either go to cargo. The script holds the
chip's build settings, so a plain `cargo build` builds for your PC, and fails. The
first build downloads ESP-IDF and its tools to `~/.espressif`, several GB, and takes 10
to 20 minutes; later builds take seconds. The log shows, in order:

```text
joined on attempt 1 at 20 dBm
on Wi-Fi at 192.168.1.13/24, free heap 191000 B
clock set by 0.pool.ntp.org (…) in 140 ms: 1793836800 s
LAN signals to 239.255.80.87 and 192.168.1.255
3f2a… in room "esp32" at 192.168.1.13:4040
```

`cfg.toml` holds the Wi-Fi network, its password and the room. Git ignores it, but the
firmware image holds the password, so keep images to yourself.

## Talk to it from a PC

On a PC on the same Wi-Fi, run `lan_chat` with the same room and type a line:

```sh
git clone https://github.com/avinashid/peerwire && cd peerwire
cargo run -p peerwire --example lan_chat -- esp32
```

The board logs `linked to …` and answers each line with `echo: …`. The PC's firewall
must let in UDP from the LAN: port 20567 and `lan_chat`'s port, which is random.

## Across the internet

With the `nostr` feature the board also joins its room over the Nostr relay
`wss://nostr.mom`, so peers anywhere link with it:

```sh
./flash.sh --features nostr
cargo run -p peerwire --features nostr --example room_chat -- ROOM   # on a PC anywhere
```

The relay's TLS compiles C with the chip's GCC: `espup install` (without `--std`)
installs it, and `export-esp.sh` puts it on `PATH`. Anyone who knows the room's name can
then link with the board from anywhere and learn your home's public address and the
board's LAN address, so name the room something others can't guess.

## If it doesn't link

- **Clock.** Peers drop hellos more than a minute off their own clock, and a board
  starts at 1970. If the log says `no SNTP answer`, the board can't reach
  `pool.ntp.org` and a PC ignores it. It asks again every minute, and restarts once one
  is answered (`clock: set late`), as the node keeps the clock it was bound with.
- **Network.** 2.4 GHz only, and the PC must be on the same network, not a guest one
  that keeps devices apart. The board also broadcasts, for routers that drop multicast.
- **Wi-Fi join.** `the AP never answered the association request`: the firmware retries
  at lower transmit power, which small C3 boards need; move the board closer.

//! Lines typed on the board's serial console, for commands (NB-119).
//!
//! [`Console::start`] reads the console on a thread of its own, so a node's loop never waits for it, and
//! [`Console::line`] gives each line typed, trimmed. It reads UART0, the console of a board with a USB-to-serial chip,
//! and on a chip with USB-Serial-JTAG (the C3) that too: a C3 board with native USB only prints there, as ESP-IDF's
//! secondary console, which takes no input until its driver is installed. Installing it moves the console's output
//! onto the driver as well, as ESP-IDF's own REPL does. Firmware whose sdkconfig makes USB-Serial-JTAG the primary
//! console has no UART0 console: then it reads the driver alone.
//!
//! A terminal sends each key as it's pressed and the board echoes nothing: what's typed shows once the firmware logs
//! the line. Anyone with a cable to the board can type commands, as anyone with one can reflash it.

use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::time::Duration;

use esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration;
use esp_idf_svc::sys;
use log::warn;

/// The longest line kept: a longer one is dropped, with a warning.
pub const MAX_LINE: usize = 128;

/// How long the thread sleeps between reads. UART0's FIFO holds 128 B, 11 ms at 115200 baud.
const POLL: Duration = Duration::from_millis(10);

/// Lines read but not yet taken: more are dropped, with a warning.
const QUEUE: usize = 8;

/// The serial console's lines ([module docs](self)).
pub struct Console {
    lines: mpsc::Receiver<String>,
}

impl Console {
    /// Starts reading the console, on a thread at the main task's priority, 1.
    pub fn start() -> anyhow::Result<Console> {
        // A read with nothing to read then returns at once.
        #[cfg(not(esp_idf_esp_console_usb_serial_jtag))]
        unsafe {
            sys::fcntl(0, sys::F_SETFL as i32, sys::O_NONBLOCK as i32)
        };
        #[cfg(esp_idf_soc_usb_serial_jtag_supported)]
        if !unsafe { sys::usb_serial_jtag_is_driver_installed() } {
            let mut config = sys::usb_serial_jtag_driver_config_t { tx_buffer_size: 256, rx_buffer_size: 256 };
            sys::esp!(unsafe { sys::usb_serial_jtag_driver_install(&mut config) })?;
            // The console's output through the driver too, so the two don't write its FIFO at once.
            unsafe { sys::esp_vfs_usb_serial_jtag_use_driver() };
        }
        let (tx, lines) = mpsc::sync_channel(QUEUE);
        ThreadSpawnConfiguration { name: Some(c"console"), priority: 1, ..Default::default() }.set()?;
        let spawned = std::thread::Builder::new().stack_size(4096).spawn(move || read(tx));
        ThreadSpawnConfiguration::default().set()?;
        spawned?;
        Ok(Console { lines })
    }

    /// The next line typed, if there is one.
    pub fn line(&self) -> Option<String> {
        self.lines.try_recv().ok()
    }
}

/// The console's thread: it reads each source, and sends `lines` each line typed, until the [`Console`] is dropped.
fn read(lines: SyncSender<String>) {
    let mut buf = [0u8; 128];
    // fd 0 is UART0, unless USB-Serial-JTAG is the primary console: then it's the driver read below.
    #[cfg(not(esp_idf_esp_console_usb_serial_jtag))]
    let mut uart = Line::default();
    #[cfg(esp_idf_soc_usb_serial_jtag_supported)]
    let mut usb = Line::default();
    loop {
        #[cfg(not(esp_idf_esp_console_usb_serial_jtag))]
        {
            // -1 with nothing to read.
            let n = unsafe { sys::read(0, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 && !uart.take(&buf[..n as usize], &lines) {
                return;
            }
        }
        #[cfg(esp_idf_soc_usb_serial_jtag_supported)]
        {
            let n = unsafe { sys::usb_serial_jtag_read_bytes(buf.as_mut_ptr().cast(), buf.len() as u32, 0) };
            if n > 0 && !usb.take(&buf[..n as usize], &lines) {
                return;
            }
        }
        std::thread::sleep(POLL);
    }
}

/// A line being typed on one source.
#[derive(Default)]
struct Line {
    bytes: Vec<u8>,
    /// Longer than [`MAX_LINE`]: dropped at its end.
    long: bool,
}

impl Line {
    /// Takes `input`, and sends `lines` each line it ends that isn't blank: false once the [`Console`] is gone.
    fn take(&mut self, input: &[u8], lines: &SyncSender<String>) -> bool {
        for &b in input {
            match b {
                b'\r' | b'\n' => {
                    let bytes = std::mem::take(&mut self.bytes);
                    if std::mem::take(&mut self.long) {
                        warn!("console: a line over {MAX_LINE} B, dropped");
                        continue;
                    }
                    let line = String::from_utf8_lossy(&bytes).trim().to_string();
                    if line.is_empty() {
                        continue;
                    }
                    match lines.try_send(line) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) => warn!("console: a line dropped, as {QUEUE} wait already"),
                        Err(TrySendError::Disconnected(_)) => return false,
                    }
                }
                // Backspace and delete.
                0x08 | 0x7f => {
                    self.bytes.pop();
                }
                // Other control bytes, such as an arrow key's escape.
                0..=0x1f => {}
                _ if self.bytes.len() >= MAX_LINE => self.long = true,
                _ => self.bytes.push(b),
            }
        }
        true
    }
}

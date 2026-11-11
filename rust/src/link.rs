//! USB link to the ESP32-DATV firmware (the ESP32-C3's native USB Serial/JTAG, a COM port).
//!
//! A port of host/esp_link.py by SP8ESA. Text commands go down, a raw symbol stream goes
//! down after the start command, and the ESP sends back 4-byte fill reports
//! (B7, fill_lo, fill_hi, underruns) between its text lines.
//!
//! `SimLink` stands in for the board in dry runs: it takes symbols at the rate the ESP
//! would and transmits nothing.

use std::io::Write;
use std::time::{Duration, Instant};

#[derive(Default, Clone, Copy)]
pub struct Fill {
    /// Symbol pairs (2 bytes) in the ESP ring at the last report.
    pub fill: i64,
    /// Ring underruns (saturates at 255).
    pub under: u8,
    /// Pairs sent since the last report.
    pub sent_since: i64,
    pub reports: u64,
}

pub trait Link: Send {
    fn poll(&mut self);
    fn state(&mut self) -> &mut Fill;
    /// Sends the start command and returns the firmware's OK line.
    fn start(&mut self, cmd: &str) -> Result<String, String>;
    fn send(&mut self, data: &[u8]) -> Result<(), String>;
    /// Stops the transmission and returns the firmware's summary.
    fn finish(&mut self, send_stop: bool) -> String;
}

/// A serial port that may be the board: (name, description, is an Espressif USB device).
pub fn ports() -> Vec<(String, String, bool)> {
    let mut v: Vec<(String, String, bool)> = serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|p| match p.port_type {
            serialport::SerialPortType::UsbPort(u) => {
                let esp = u.vid == 0x303A;
                let what = u.product.unwrap_or_else(|| format!("USB {:04X}:{:04X}", u.vid, u.pid));
                (p.port_name, what, esp)
            }
            _ => (p.port_name, String::new(), false),
        })
        .collect();
    v.sort_by_key(|p| !p.2);
    v
}

pub struct SerialLink {
    s: Box<dyn serialport::SerialPort>,
    buf: Vec<u8>,
    text: Vec<u8>,
    st: Fill,
}

impl SerialLink {
    /// Opens the port and waits for the firmware to answer INFO.
    pub fn open(port: &str) -> Result<Self, String> {
        let mut s = serialport::new(port, 115_200)
            .timeout(Duration::from_millis(1000))
            .open()
            .map_err(|e| format!("cannot open {port}: {e}"))?;
        std::thread::sleep(Duration::from_millis(1200)); // opening the port may reset the C3
        let mut found = false;
        'tries: for _ in 0..8 {
            s.write_all(b"\nINFO\n").map_err(|e| format!("{port}: {e}"))?;
            let t0 = Instant::now();
            let mut got = Vec::new();
            while t0.elapsed() < Duration::from_millis(700) {
                read_available(&mut *s, &mut got);
                if got.windows(9).any(|w| w == b"ESP32DATV") {
                    found = true;
                    break 'tries;
                }
                if got.windows(9).any(|w| w == b"TDECKDATV") {
                    return Err("this is a T-Deck: it transmits on its own, this app drives the ESP32-C3 firmware".into());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        if !found {
            return Err(format!("the ESP on {port} does not answer INFO (is the DATV firmware flashed?)"));
        }
        std::thread::sleep(Duration::from_millis(100));
        let _ = s.clear(serialport::ClearBuffer::Input);
        Ok(SerialLink { s, buf: Vec::new(), text: Vec::new(), st: Fill::default() })
    }
}

fn read_available(s: &mut dyn serialport::SerialPort, into: &mut Vec<u8>) {
    let n = s.bytes_to_read().unwrap_or(0) as usize;
    if n > 0 {
        let at = into.len();
        into.resize(at + n, 0);
        let got = s.read(&mut into[at..]).unwrap_or(0);
        into.truncate(at + got);
    }
}

impl Link for SerialLink {
    fn poll(&mut self) {
        read_available(&mut *self.s, &mut self.buf);
        let b = std::mem::take(&mut self.buf);
        let mut i = 0;
        while i < b.len() {
            if b[i] == 0xB7 {
                if i + 4 > b.len() {
                    break;
                }
                self.st.fill = b[i + 1] as i64 | (b[i + 2] as i64) << 8;
                self.st.under = b[i + 3];
                self.st.sent_since = 0;
                self.st.reports += 1;
                i += 4;
            } else {
                self.text.push(b[i]);
                i += 1;
            }
        }
        self.buf = b[i..].to_vec();
    }

    fn state(&mut self) -> &mut Fill {
        &mut self.st
    }

    fn start(&mut self, cmd: &str) -> Result<String, String> {
        self.s.write_all(format!("{cmd}\n").as_bytes()).map_err(|e| e.to_string())?;
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(8) {
            self.poll();
            for ln in String::from_utf8_lossy(&self.text).lines() {
                if ln.starts_with("OK QPSKT") {
                    return Ok(ln.to_string());
                }
                if ln.starts_with("ERR") {
                    return Err(format!("ESP: {ln}"));
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let tail = String::from_utf8_lossy(&self.text[self.text.len().saturating_sub(200)..]).into_owned();
        Err(format!("no answer from the ESP: {tail:?}"))
    }

    fn send(&mut self, data: &[u8]) -> Result<(), String> {
        self.s.write_all(data).map_err(|e| format!("USB write: {e}"))
    }

    fn finish(&mut self, send_stop: bool) -> String {
        if send_stop {
            let _ = self.s.write_all(&[0xA5, 0xFF]);
        }
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(5) {
            self.poll();
            let t = String::from_utf8_lossy(&self.text);
            if t.contains("TX END") && t.trim_end().ends_with(')') {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let lines: Vec<String> =
            String::from_utf8_lossy(&self.text).lines().filter(|l| l.contains("TX END")).map(String::from).collect();
        if lines.is_empty() {
            "no summary from the ESP".into()
        } else {
            lines.join("\n")
        }
    }
}

/// No board and no RF: consumes symbol bytes at the ESP's rate.
pub struct SimLink {
    bytes_per_sec: f64,
    buffered: f64,
    last: Instant,
    last_report: Instant,
    dry: bool,
    total: u64,
    started: bool,
    st: Fill,
}

impl SimLink {
    pub fn new(bytes_per_sec: f64) -> Self {
        let now = Instant::now();
        SimLink {
            bytes_per_sec,
            buffered: 0.0,
            last: now,
            last_report: now,
            dry: false,
            total: 0,
            started: false,
            st: Fill::default(),
        }
    }
}

impl Link for SimLink {
    fn poll(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        if self.started {
            self.buffered -= dt * self.bytes_per_sec;
            if self.buffered <= 0.0 {
                if !self.dry {
                    self.st.under = self.st.under.saturating_add(1); // once per time it runs dry
                }
                self.dry = true;
                self.buffered = 0.0;
            } else {
                self.dry = false;
            }
        }
        if now.duration_since(self.last_report) >= Duration::from_millis(4) {
            self.last_report = now;
            self.st.fill = (self.buffered / 2.0) as i64;
            self.st.sent_since = 0;
            self.st.reports += 1;
        }
    }

    fn state(&mut self) -> &mut Fill {
        &mut self.st
    }

    fn start(&mut self, cmd: &str) -> Result<String, String> {
        self.last = Instant::now();
        Ok(format!("DRY RUN, nothing is transmitted. The board would get: {cmd}"))
    }

    fn send(&mut self, data: &[u8]) -> Result<(), String> {
        self.started = true; // the ESP starts once its buffer has data
        self.buffered += data.len() as f64;
        self.total += data.len() as u64;
        Ok(())
    }

    fn finish(&mut self, _send_stop: bool) -> String {
        format!("dry run end: {} symbol bytes, {} underruns", self.total, self.st.under)
    }
}

//! Transmit plan: samples per symbol, the symbol rate the firmware really produces and the
//! start command. A port of the planning part of host/tx_dvbs.py by SP8ESA.

use crate::{dvbs, dvbs2};

pub const CPU_HZ: u64 = 160_000_000;
const P8_MIN_PERIOD: u64 = 75;
const A16_MIN_PERIOD: u64 = 80;
/// MHz; the firmware refuses anything else too.
pub const BAND: (f64, f64) = (2300.0, 2450.0);

#[derive(Clone, Debug, PartialEq)]
pub struct RfConfig {
    pub freq_mhz: f64,
    pub baud: u64,
    pub dvbs2: bool,
    /// "qpsk", "8psk", "16apsk"
    pub modulation: String,
    pub fec: String,
    /// "normal" or "short"
    pub frame: String,
    pub pilots: bool,
    pub invert: bool,
    pub swap_iq: bool,
    pub ppm: f64,
    /// 0 = automatic
    pub amp: u32,
    pub ifm: i64,
    /// DC trim [DAC codes], Q gain, Q phase [degrees]
    pub dc: [f64; 2],
    pub iq_gain: f64,
    pub iq_phase_deg: f64,
}

impl Default for RfConfig {
    fn default() -> Self {
        RfConfig {
            freq_mhz: 2402.0,
            baud: 333_000,
            dvbs2: true,
            modulation: "qpsk".into(),
            fec: "1/2".into(),
            frame: "normal".into(),
            pilots: false,
            invert: false,
            swap_iq: false,
            ppm: 0.0,
            amp: 0,
            ifm: 0,
            dc: [0.0, 0.0],
            iq_gain: 1.0,
            iq_phase_deg: 0.0,
        }
    }
}

impl RfConfig {
    /// The modulation in use (DVB-S is always QPSK).
    pub fn modulation(&self) -> &str {
        if self.dvbs2 {
            &self.modulation
        } else {
            "qpsk"
        }
    }
}

fn rdiv(a: u64, b: u64) -> u64 {
    // Python's round(): halves to even
    let (q, r) = (a / b, a % b);
    if 2 * r > b || (2 * r == b && q % 2 == 1) {
        q + 1
    } else {
        q
    }
}

fn err(period: u64, sps: u64, baud: u64) -> f64 {
    (CPU_HZ as f64 / (period * sps) as f64 / baud as f64 - 1.0).abs()
}

fn auto_sps_qpsk(baud: u64) -> u64 {
    if baud == 1_000_000 {
        return 8;
    }
    let mut best: Option<(f64, u64)> = None;
    for period in [20, 24] {
        let sps = rdiv(CPU_HZ, period * baud);
        if (16..=232).contains(&sps) {
            let e = err(period, sps, baud);
            if e <= 0.01 && best.map_or(true, |b| e < b.0 - 1e-9) {
                best = Some((e, sps));
            }
        }
    }
    if let Some(b) = best {
        return b.1;
    }
    if baud * 16 <= 4_000_000 {
        16
    } else if baud * 8 <= 4_000_000 {
        8
    } else {
        4
    }
}

fn auto_sps_16apsk(baud: u64) -> Result<u64, String> {
    if baud == 1_000_000 {
        return Ok(8);
    }
    if !(2000..=500_000).contains(&baud) {
        return Err("16APSK: symbol rate must be 2000 .. 500000 Bd or 1000000 Bd".into());
    }
    let sps = rdiv(CPU_HZ, 20 * baud);
    if (16..=24).contains(&sps) && err(20, sps, baud) <= 0.01 {
        return Ok(sps);
    }
    (4..=24)
        .rev()
        .find(|sps| CPU_HZ / (baud * sps) >= A16_MIN_PERIOD)
        .ok_or_else(|| "16APSK: no sample rate fits the firmware timing budget".into())
}

fn auto_sps_8psk(baud: u64) -> Result<u64, String> {
    if baud == 1_000_000 {
        return Ok(8);
    }
    let sps = rdiv(CPU_HZ, 20 * baud);
    if (16..=64).contains(&sps) && err(20, sps, baud) <= 0.01 {
        return Ok(sps);
    }
    (16..=64).rev().find(|sps| CPU_HZ / (baud * sps) >= P8_MIN_PERIOD).ok_or_else(|| {
        "8PSK: symbol rate must be 1000000 Bd, 125000 .. 500000 Bd, or lower down to about 10000 Bd".into()
    })
}

/// The symbol rate the firmware's sample clock gives.
fn output_baud(baud: u64, sps: u64, m: &str) -> f64 {
    let sample_hz = baud * sps;
    let period = (CPU_HZ + sample_hz / 2) / sample_hz;
    if m == "8psk" && period != 20 && (16..=64).contains(&sps) && CPU_HZ / sample_hz >= P8_MIN_PERIOD {
        return baud as f64;
    }
    if m == "16apsk" && period != 20 && (4..=24).contains(&sps) && CPU_HZ / sample_hz >= A16_MIN_PERIOD {
        return baud as f64;
    }
    let generic = (m == "8psk" && (16..=64).contains(&sps) && period == 20)
        || (m == "qpsk" && (16..=232).contains(&sps) && (period == 20 || period == 24))
        || (m == "16apsk" && (16..=24).contains(&sps) && period == 20);
    if generic && CPU_HZ / baud == period * sps {
        return baud as f64;
    }
    CPU_HZ as f64 / (period * sps) as f64
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub sps: u64,
    pub baud_act: f64,
    /// Transport stream capacity [bit/s]
    pub capacity: f64,
    pub amp: u32,
    /// ESP buffer fill to hold [pairs of 2 bytes]
    pub target: i64,
    pub min_todo: i64,
    pub max_chunk: i64,
    /// 16APSK at 1 MBd: whole 64-byte USB packets, no stop marker
    pub a16s8: bool,
    pub if_hz: i64,
    /// Symbol bytes the ESP consumes per second
    pub bytes_per_sec: f64,
    /// DVB-S2: symbol bytes of one PLFRAME and the transport stream packets it carries
    /// (DVB-S has no frames to fill: 0).
    pub frame_bytes: usize,
    pub frame_packets: usize,
    pub notes: Vec<String>,
}

pub fn plan(c: &RfConfig) -> Result<Plan, String> {
    if !(BAND.0..=BAND.1).contains(&c.freq_mhz) {
        return Err("transmission only in the 13 cm band (2300..2450 MHz)".into());
    }
    if !(2000..=1_000_000).contains(&c.baud) {
        return Err("symbol rate must be 2000 .. 1000000 Bd".into());
    }
    let m = c.modulation();
    let sps = match m {
        "16apsk" => auto_sps_16apsk(c.baud)?,
        "8psk" => auto_sps_8psk(c.baud)?,
        _ => auto_sps_qpsk(c.baud),
    };
    let a16s8 = m == "16apsk" && c.baud == 1_000_000;
    let amp = if c.amp != 0 {
        c.amp
    } else if m != "qpsk" {
        420
    } else {
        300
    };
    if !(1..=480).contains(&amp) {
        return Err("amplitude must be 1 .. 480 DAC codes".into());
    }
    let target = match m {
        "8psk" if sps == 8 => 6000,
        "16apsk" if a16s8 => 7000,
        "16apsk" => 2500,
        _ => 3000,
    };
    let baud_act = output_baud(c.baud, sps, m);
    let mut notes = Vec::new();
    if m != "qpsk" && baud_act > 460_000.0 && baud_act < 1_000_000.0 {
        notes.push(format!(
            "{} at {:.0} kBd needs {:.0} kB/s over USB: watch the ESP buffer, lower the symbol rate if it runs dry",
            m.to_uppercase(),
            baud_act / 1e3,
            baud_act / 2e3
        ));
    }
    if (baud_act / c.baud as f64 - 1.0).abs() > 0.01 {
        notes.push(format!("{sps} samples per symbol give {baud_act:.1} Bd, not {}", c.baud));
    }
    let capacity = if c.dvbs2 {
        dvbs2::ts_rate(baud_act, &c.fec, &c.frame, c.pilots, m)?
    } else {
        dvbs::ts_rate(baud_act, &c.fec).ok_or_else(|| format!("DVB-S code rate: {}", dvbs::FECS.join(", ")))?
    };
    let (min_todo, max_chunk) = if a16s8 {
        (400, 4096)
    } else if sps == 8 && m == "8psk" {
        (400, 2048)
    } else {
        (32, 1024)
    };
    let bytes_per_symbol = match m {
        "qpsk" => 0.25,
        "8psk" if sps == 8 => 0.375,
        _ => 0.5,
    };
    let (frame_bytes, frame_packets) = if c.dvbs2 {
        let fi = dvbs2::frame_info(&c.fec, &c.frame, c.pilots, m)?;
        ((fi.plframe as f64 * bytes_per_symbol).ceil() as usize, fi.dfl.div_ceil(1504))
    } else {
        (0, 0)
    };
    Ok(Plan {
        frame_bytes,
        frame_packets,
        sps,
        baud_act,
        capacity,
        amp,
        target,
        min_todo,
        max_chunk,
        a16s8,
        if_hz: (c.ifm as f64 * baud_act).round() as i64,
        bytes_per_sec: baud_act * bytes_per_symbol,
        notes,
    })
}

/// The firmware start command (QPSKT / PSK8T / A16T), as host/tx_dvbs.py sends it.
pub fn start_command(c: &RfConfig, p: &Plan, seconds: u64) -> String {
    let m = c.modulation();
    let khz = ((c.freq_mhz * 1e6 - p.if_hz as f64) / 1000.0 / (1.0 + c.ppm * 1e-6)).round();
    let cmd = match m {
        "8psk" => "PSK8T",
        "16apsk" => "A16T",
        _ => "QPSKT",
    };
    let gam = match (m, dvbs2::apsk16_gamma(&c.fec)) {
        ("16apsk", Some(g)) => format!(" {}", (100.0 * g).round()),
        _ => String::new(),
    };
    format!(
        "{cmd} {:.3} {} {} {} {} {} {} {} {} {} {}{gam}",
        khz / 1000.0,
        c.baud,
        p.sps,
        p.amp,
        seconds,
        c.ifm,
        p.target,
        (16.0 * c.dc[0]).round(),
        (16.0 * c.dc[1]).round(),
        (c.iq_gain * 1e4).round(),
        (c.iq_phase_deg * 1e3).round()
    )
}

pub enum AnyEncoder {
    S(dvbs::Encoder),
    S2(dvbs2::Encoder),
}

impl AnyEncoder {
    pub fn new(c: &RfConfig, p: &Plan) -> Result<Self, String> {
        Ok(if c.dvbs2 {
            AnyEncoder::S2(dvbs2::Encoder::new(
                &c.fec,
                &c.frame,
                c.pilots,
                c.swap_iq,
                c.invert,
                &c.modulation,
                p.sps == 8,
            )?)
        } else {
            AnyEncoder::S(dvbs::Encoder::new(&c.fec, c.swap_iq, c.invert)?)
        })
    }

    pub fn encode(&mut self, ts: &[u8]) -> Vec<u8> {
        match self {
            AnyEncoder::S(e) => e.encode(ts),
            AnyEncoder::S2(e) => e.encode(ts),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Values worked out from auto_sps and output_baud in host/tx_dvbs.py.
    #[test]
    fn sps_and_rates() {
        let mut c = RfConfig { dvbs2: false, baud: 1_000_000, ..Default::default() };
        let p = plan(&c).unwrap();
        assert_eq!((p.sps, p.target), (8, 3000));
        assert!((p.capacity - 1e6 * 188.0 / 204.0).abs() < 1e-6);
        c.baud = 333_000; // 160e6 / (20 * 24) = 333333
        assert_eq!(plan(&c).unwrap().sps, 24);
        c.baud = 33_000; // 8 MS/s gives 242 > 232; 6.67 MS/s gives 202
        assert_eq!(plan(&c).unwrap().sps, 202);
        c = RfConfig { modulation: "16apsk".into(), fec: "2/3".into(), baud: 1_000_000, ..Default::default() };
        let p = plan(&c).unwrap();
        assert!(p.a16s8 && p.target == 7000);
        assert!(start_command(&c, &p, 86400).ends_with(" 315"));
        c.modulation = "8psk".into();
        c.fec = "1/2".into();
        assert!(plan(&c).is_err());
    }
}

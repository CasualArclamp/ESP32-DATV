//! Settings: kept as JSON in %APPDATA%\datv-tx\prefs.json, and set from the command line.

use crate::plan::RfConfig;
use crate::source::{Codec, MediaConfig, SourceKind};
use crate::tx::TxConfig;
use serde_json::{json, Value};

/// DC and I/Q trim measured on the firmware author's board (host/cal.json): a few DAC
/// codes, harmless on other boards, and what host/tx_dvbs.py applies by default.
pub fn example_cal() -> ([f64; 2], f64, f64) {
    let v: Value = serde_json::from_str(include_str!("../../host/cal.json")).unwrap_or(Value::Null);
    let f = |x: &Value, d: f64| x.as_f64().unwrap_or(d);
    ([f(&v["dc"][0], 0.0), f(&v["dc"][1], 0.0)], f(&v["iq_gain"], 1.0), f(&v["iq_phase_deg"], 0.0))
}

pub fn default_config() -> TxConfig {
    let mut c = TxConfig {
        rf: RfConfig::default(),
        media: MediaConfig::default(),
        port: String::new(),
        dry_run: false,
        ts_out: String::new(),
        seconds: 0.0,
    };
    set_cal(&mut c.rf, true);
    c
}

pub fn set_cal(rf: &mut RfConfig, on: bool) {
    let (dc, g, p) = if on { example_cal() } else { ([0.0, 0.0], 1.0, 0.0) };
    rf.dc = dc;
    rf.iq_gain = g;
    rf.iq_phase_deg = p;
}

pub fn has_cal(rf: &RfConfig) -> bool {
    rf.dc != [0.0, 0.0] || rf.iq_gain != 1.0 || rf.iq_phase_deg != 0.0
}

pub fn to_json(c: &TxConfig) -> Value {
    json!({
        "port": c.port, "freq_mhz": c.rf.freq_mhz, "baud": c.rf.baud, "dvbs2": c.rf.dvbs2,
        "modulation": c.rf.modulation, "fec": c.rf.fec, "frame": c.rf.frame, "pilots": c.rf.pilots,
        "invert": c.rf.invert, "swap_iq": c.rf.swap_iq, "ppm": c.rf.ppm, "amp": c.rf.amp, "ifm": c.rf.ifm,
        "cal": has_cal(&c.rf),
        "ffmpeg": c.media.ffmpeg, "source": c.media.source.key(), "path": c.media.path,
        "codec": c.media.codec.key(), "encoder": c.media.encoder, "width": c.media.width,
        "video_kbps": c.media.video_kbps, "camera": c.media.camera, "microphone": c.media.microphone,
        "service_name": c.media.service_name, "provider": c.media.provider,
    })
}

pub fn from_json(v: &Value) -> TxConfig {
    let mut c = default_config();
    let s = |k: &str, d: &mut String| {
        if let Some(x) = v[k].as_str() {
            *d = x.to_string();
        }
    };
    let b = |k: &str, d: &mut bool| {
        if let Some(x) = v[k].as_bool() {
            *d = x;
        }
    };
    s("port", &mut c.port);
    s("modulation", &mut c.rf.modulation);
    s("fec", &mut c.rf.fec);
    s("frame", &mut c.rf.frame);
    s("ffmpeg", &mut c.media.ffmpeg);
    s("path", &mut c.media.path);
    s("encoder", &mut c.media.encoder);
    s("camera", &mut c.media.camera);
    s("microphone", &mut c.media.microphone);
    s("service_name", &mut c.media.service_name);
    s("provider", &mut c.media.provider);
    b("dvbs2", &mut c.rf.dvbs2);
    b("pilots", &mut c.rf.pilots);
    b("invert", &mut c.rf.invert);
    b("swap_iq", &mut c.rf.swap_iq);
    if let Some(x) = v["freq_mhz"].as_f64() {
        c.rf.freq_mhz = x;
    }
    if let Some(x) = v["baud"].as_u64() {
        c.rf.baud = x;
    }
    if let Some(x) = v["ppm"].as_f64() {
        c.rf.ppm = x;
    }
    if let Some(x) = v["amp"].as_u64() {
        c.rf.amp = x as u32;
    }
    if let Some(x) = v["ifm"].as_i64() {
        c.rf.ifm = x;
    }
    if let Some(x) = v["cal"].as_bool() {
        set_cal(&mut c.rf, x);
    }
    if let Some(x) = v["width"].as_u64() {
        c.media.width = x as u32;
    }
    if let Some(x) = v["video_kbps"].as_u64() {
        c.media.video_kbps = x as u32;
    }
    if let Some(x) = v["source"].as_str().and_then(SourceKind::from_key) {
        c.media.source = x;
    }
    if let Some(x) = v["codec"].as_str().and_then(Codec::from_key) {
        c.media.codec = x;
    }
    c
}

fn path() -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from(std::env::var_os("APPDATA")?).join("datv-tx").join("prefs.json"))
}

pub fn load() -> TxConfig {
    path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .map(|v| from_json(&v))
        .unwrap_or_else(default_config)
}

pub fn save(c: &TxConfig) {
    if let Some(p) = path() {
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        let _ = std::fs::write(p, serde_json::to_string_pretty(&to_json(c)).unwrap_or_default());
    }
}

pub const USAGE: &str = "\
  --freq MHZ          centre frequency, 2300..2450 (default 2402)
  --baud BD           symbol rate, 2000..1000000 (default 333000)
  --dvbs | --dvbs2    standard (default DVB-S2)
  --mod M             DVB-S2 modulation: qpsk, 8psk, 16apsk
  --fec R             code rate, e.g. 1/2, 2/3, 3/4
  --frame F           DVB-S2 frame: normal, short
  --pilots            DVB-S2 pilots
  --invert, --swap-iq spectrum inversion, I/Q swap
  --ppm N             crystal error of the board
  --amp N             DAC amplitude 1..480 (0 = automatic)
  --no-cal            no DC / I/Q trim
  --codec C           video codec: h264, hevc (h265), vvc (h266), av1
  --encoder NAME      FFmpeg encoder, e.g. libx265, hevc_nvenc (default: the software one)
  --film FILE         encode and loop a video file
  --test              test pattern and tone (default)
  --screen            the screen
  --camera NAME       a DirectShow camera
  --mic NAME          sound for --screen / --camera
  --ts FILE           loop a ready constant bit rate transport stream
  --null, --cw        null packets only, unmodulated carrier
  --width N           picture width (0 = from the channel capacity)
  --video-k N         video bit rate in kb/s (0 = from the channel capacity)
  --name TEXT         service name
  --ffmpeg PATH       the ffmpeg to run (default: ffmpeg on PATH)
  --seconds N         stop after N seconds
  --ts-out FILE       copy the transport stream that goes on air to a file
  --dry-run           no board, no RF: everything runs into a simulated ESP
  --port COMn         the board (needed unless --dry-run)";

fn num<T: std::str::FromStr>(k: &str, v: String) -> Result<T, String> {
    v.parse().map_err(|_| format!("{k}: {v} is not a number"))
}

/// Applies command line options to a configuration.
pub fn apply_args(c: &mut TxConfig, args: &[String]) -> Result<(), String> {
    let mut i = 0;
    while i < args.len() {
        let k = args[i].as_str();
        i += 1;
        let mut val = || {
            let v = args.get(i).cloned().ok_or_else(|| format!("{k} needs a value"));
            i += 1;
            v
        };
        match k {
            "--freq" => c.rf.freq_mhz = num(k, val()?)?,
            "--baud" => c.rf.baud = num(k, val()?)?,
            "--dvbs" => c.rf.dvbs2 = false,
            "--dvbs2" => c.rf.dvbs2 = true,
            "--mod" => c.rf.modulation = val()?.to_lowercase(),
            "--fec" => c.rf.fec = val()?,
            "--frame" => c.rf.frame = val()?,
            "--pilots" => c.rf.pilots = true,
            "--invert" => c.rf.invert = true,
            "--swap-iq" => c.rf.swap_iq = true,
            "--ppm" => c.rf.ppm = num(k, val()?)?,
            "--amp" => c.rf.amp = num(k, val()?)?,
            "--no-cal" => set_cal(&mut c.rf, false),
            "--codec" => {
                let v = val()?;
                c.media.codec = Codec::from_key(&v).ok_or(format!("--codec {v}: h264, hevc, vvc or av1"))?;
                c.media.encoder.clear();
            }
            "--encoder" => c.media.encoder = val()?,
            "--film" => {
                c.media.source = SourceKind::File;
                c.media.path = val()?;
            }
            "--ts" => {
                c.media.source = SourceKind::TsFile;
                c.media.path = val()?;
            }
            "--test" => c.media.source = SourceKind::TestPattern,
            "--screen" => c.media.source = SourceKind::Screen,
            "--camera" => {
                c.media.source = SourceKind::Camera;
                c.media.camera = val()?;
            }
            "--mic" => c.media.microphone = val()?,
            "--null" => c.media.source = SourceKind::Null,
            "--cw" => c.media.source = SourceKind::Carrier,
            "--width" => c.media.width = num(k, val()?)?,
            "--video-k" => c.media.video_kbps = num(k, val()?)?,
            "--name" => c.media.service_name = val()?,
            "--ffmpeg" => c.media.ffmpeg = val()?,
            "--seconds" => c.seconds = num(k, val()?)?,
            "--ts-out" => c.ts_out = val()?,
            "--dry-run" => c.dry_run = true,
            "--port" => c.port = val()?,
            _ => return Err(format!("unknown option {k}")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip() {
        let mut c = default_config();
        let a = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        apply_args(&mut c, &a("--codec h266 --baud 500000 --dvbs2 --mod 8psk --fec 3/5 --pilots --no-cal --screen"))
            .unwrap();
        assert_eq!(c.media.codec, Codec::Vvc);
        assert!(!has_cal(&c.rf));
        let back = from_json(&to_json(&c));
        assert!(back == c);
        assert!(apply_args(&mut c, &a("--codec mpeg2")).is_err());
    }
}

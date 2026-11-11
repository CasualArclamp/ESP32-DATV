//! Transport stream sources. FFmpeg (the one installed on the PC) encodes the picture and
//! sound and multiplexes them at a rate that fits the channel; the video codec and the
//! encoder are the user's choice. After the source handling in host/tx_dvbs.py by SP8ESA.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use crate::av1ts::{Av1Fix, Packet};
use crate::dvbs::NULL_PACKET;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Vvc,
    Av1,
}

impl Codec {
    pub const ALL: [Codec; 4] = [Codec::H264, Codec::Hevc, Codec::Vvc, Codec::Av1];

    pub fn label(self) -> &'static str {
        match self {
            Codec::H264 => "H.264 (AVC)",
            Codec::Hevc => "H.265 (HEVC)",
            Codec::Vvc => "H.266 (VVC)",
            Codec::Av1 => "AV1",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
            Codec::Vvc => "vvc",
            Codec::Av1 => "av1",
        }
    }

    pub fn from_key(k: &str) -> Option<Codec> {
        Codec::ALL.into_iter().find(|c| {
            c.key() == k.to_ascii_lowercase()
                || matches!(
                    (c, k.to_ascii_lowercase().as_str()),
                    (Codec::H264, "avc" | "h.264")
                        | (Codec::Hevc, "h265" | "h.265")
                        | (Codec::Vvc, "h266" | "h.266")
                )
        })
    }

    /// FFmpeg encoders for this codec, the preferred (software) one first.
    pub fn known_encoders(self) -> &'static [&'static str] {
        match self {
            Codec::H264 => &["libx264", "h264_nvenc", "h264_qsv", "h264_amf", "h264_mf"],
            Codec::Hevc => &["libx265", "hevc_nvenc", "hevc_qsv", "hevc_amf", "hevc_mf"],
            Codec::Vvc => &["libvvenc"],
            Codec::Av1 => &["libsvtav1", "av1_nvenc", "av1_qsv", "av1_amf", "libaom-av1"],
        }
    }

    /// The encoders of this codec that the installed FFmpeg has.
    pub fn encoders(self, installed: &[String]) -> Vec<String> {
        self.known_encoders().iter().filter(|e| installed.iter().any(|i| i == *e)).map(|e| e.to_string()).collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioCodec {
    Mp2,
    Mp3,
    Aac,
    /// HE-AAC (AAC with spectral band replication), made by libfdk_aac.
    HeAac,
    /// HE-AAC v2 (with parametric stereo).
    HeAacV2,
    Opus,
    Ac3,
    Eac3,
    /// No sound at all.
    Off,
}

impl AudioCodec {
    pub const ALL: [AudioCodec; 9] = [
        AudioCodec::Mp2,
        AudioCodec::Mp3,
        AudioCodec::Aac,
        AudioCodec::HeAac,
        AudioCodec::HeAacV2,
        AudioCodec::Opus,
        AudioCodec::Ac3,
        AudioCodec::Eac3,
        AudioCodec::Off,
    ];

    pub fn label(self) -> &'static str {
        match self {
            AudioCodec::Mp2 => "MPEG Layer II (MP2)",
            AudioCodec::Mp3 => "MP3",
            AudioCodec::Aac => "AAC",
            AudioCodec::HeAac => "HE-AAC",
            AudioCodec::HeAacV2 => "HE-AAC v2",
            AudioCodec::Opus => "Opus",
            AudioCodec::Ac3 => "Dolby Digital (AC-3)",
            AudioCodec::Eac3 => "Dolby Digital Plus (E-AC-3)",
            AudioCodec::Off => "No sound",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            AudioCodec::Mp2 => "mp2",
            AudioCodec::Mp3 => "mp3",
            AudioCodec::Aac => "aac",
            AudioCodec::HeAac => "heaac",
            AudioCodec::HeAacV2 => "heaac2",
            AudioCodec::Opus => "opus",
            AudioCodec::Ac3 => "ac3",
            AudioCodec::Eac3 => "eac3",
            AudioCodec::Off => "none",
        }
    }

    pub fn from_key(k: &str) -> Option<AudioCodec> {
        let k = k.to_ascii_lowercase();
        AudioCodec::ALL.into_iter().find(|c| c.key() == k || (*c == AudioCodec::Off && k == "off"))
    }

    /// The FFmpeg encoder; None for no sound.
    pub fn encoder(self) -> Option<&'static str> {
        Some(match self {
            AudioCodec::Mp2 => "mp2",
            AudioCodec::Mp3 => "libmp3lame",
            AudioCodec::Aac => "aac",
            // FFmpeg's own AAC encoder makes AAC-LC only
            AudioCodec::HeAac | AudioCodec::HeAacV2 => "libfdk_aac",
            AudioCodec::Opus => "libopus",
            AudioCodec::Ac3 => "ac3",
            AudioCodec::Eac3 => "eac3",
            AudioCodec::Off => return None,
        })
    }

    pub fn available(self, installed: &[String]) -> bool {
        self.encoder().map_or(true, |e| installed.iter().any(|i| i == e))
    }

    /// Bit rate [kb/s] and sample rate this codec can do, nearest to what the channel
    /// budget asks for. Opus always runs at 48 kHz; AC-3 and E-AC-3 start at 32 kHz and
    /// 32 kb/s (ATSC A/52 table 5.18 has no smaller frame).
    fn fit(self, kbps: u32, rate: u32) -> (u32, u32) {
        match self {
            AudioCodec::Opus => (kbps.clamp(6, 256), 48000),
            AudioCodec::Ac3 | AudioCodec::Eac3 => (kbps.max(32), rate.max(32000)),
            // the band replication doubles the rate of the AAC core underneath
            AudioCodec::HeAac | AudioCodec::HeAacV2 => (kbps.clamp(8, 128), rate.max(32000)),
            _ => (kbps, rate),
        }
    }

    /// Options after the encoder name.
    fn extra_args(self) -> &'static [&'static str] {
        match self {
            AudioCodec::HeAac => &["-profile:a", "aac_he"],
            AudioCodec::HeAacV2 => &["-profile:a", "aac_he_v2"],
            _ => &[],
        }
    }

    /// Why this codec is a poor choice for a multiplex of this size, if it is. AC-3 and
    /// E-AC-3 cannot go below 32 kb/s and are tuned for several times that: on a narrow
    /// channel they take the bits the picture needs and still sound worse than Opus or
    /// HE-AAC at 16 kb/s.
    pub fn unsuited(self, mux_bps: u32) -> Option<&'static str> {
        match self {
            AudioCodec::Ac3 | AudioCodec::Eac3 if mux_bps < 200_000 => {
                Some("needs at least 32 kb/s: too much of a channel under 200 kb/s")
            }
            _ => None,
        }
    }
}

/// Encoder effort, 0 (fastest) .. 4 (slowest, best picture for the bit rate).
pub const SPEEDS: [&str; 5] = ["Fastest", "Fast", "Balanced", "Slow", "Slowest"];
pub const DEFAULT_SPEED: u8 = 1;

/// The speed setting of an encoder as FFmpeg options.
pub fn speed_args(encoder: &str, speed: u8) -> Vec<&'static str> {
    let i = (speed as usize).min(4);
    let pick = |opt: &'static str, v: [&'static str; 5]| vec![opt, v[i]];
    match encoder {
        "libx264" | "libx265" => pick("-preset", ["ultrafast", "veryfast", "fast", "medium", "slow"]),
        "libvvenc" => pick("-preset", ["faster", "faster", "fast", "medium", "slow"]),
        "libsvtav1" => pick("-preset", ["12", "10", "8", "6", "4"]),
        "libaom-av1" => pick("-cpu-used", ["10", "9", "8", "7", "6"]),
        e if e.ends_with("_nvenc") => pick("-preset", ["p1", "p3", "p4", "p6", "p7"]),
        e if e.ends_with("_qsv") => pick("-preset", ["veryfast", "faster", "medium", "slow", "veryslow"]),
        e if e.ends_with("_amf") => pick("-quality", ["speed", "speed", "balanced", "quality", "quality"]),
        _ => Vec::new(),
    }
}

/// Seconds between key frames when the choice is left to the program. A key frame costs
/// many times an ordinary frame, so the less the channel carries the further apart they
/// go; the price is the time a receiver needs to show a picture after it locks.
pub fn auto_gop_seconds(video_bps: u32) -> f32 {
    match video_bps {
        600_000.. => 2.0,
        250_000.. => 4.0,
        80_000.. => 6.0,
        _ => 10.0,
    }
}

/// B frames can be switched for this encoder (the others decide for themselves: SVT-AV1
/// and libaom run without reordering for a constant bit rate, vvenc has no such option).
pub fn has_bframes_option(encoder: &str) -> bool {
    matches!(encoder, "libx264" | "libx265")
        || ["_nvenc", "_qsv", "_amf"].iter().any(|s| encoder.ends_with(s)) && !encoder.starts_with("av1")
}

pub fn encoder_label(e: &str) -> String {
    let what = match e {
        "libx264" | "libx265" | "libvvenc" | "libsvtav1" | "libaom-av1" => "software",
        _ if e.ends_with("_nvenc") => "NVIDIA GPU",
        _ if e.ends_with("_qsv") => "Intel GPU",
        _ if e.ends_with("_amf") => "AMD GPU",
        _ if e.ends_with("_mf") => "Windows Media Foundation",
        _ => "",
    };
    if what.is_empty() {
        e.to_string()
    } else {
        format!("{e} ({what})")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SourceKind {
    TestPattern,
    /// A video file, looped.
    File,
    /// A ready constant bit rate transport stream file, looped.
    TsFile,
    Screen,
    Camera,
    /// Null packets only.
    Null,
    /// Unmodulated carrier.
    Carrier,
}

impl SourceKind {
    pub const ALL: [SourceKind; 7] = [
        SourceKind::TestPattern,
        SourceKind::File,
        SourceKind::Screen,
        SourceKind::Camera,
        SourceKind::TsFile,
        SourceKind::Null,
        SourceKind::Carrier,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            SourceKind::TestPattern => "Test pattern",
            SourceKind::File => "Video file",
            SourceKind::TsFile => "Transport stream file (.ts)",
            SourceKind::Screen => "Screen",
            SourceKind::Camera => "Camera",
            SourceKind::Null => "Null packets (lock test)",
            SourceKind::Carrier => "Unmodulated carrier",
        }
    }

    pub fn key(&self) -> &'static str {
        match self {
            SourceKind::TestPattern => "test",
            SourceKind::File => "file",
            SourceKind::TsFile => "ts",
            SourceKind::Screen => "screen",
            SourceKind::Camera => "camera",
            SourceKind::Null => "null",
            SourceKind::Carrier => "cw",
        }
    }

    pub fn from_key(k: &str) -> Option<SourceKind> {
        SourceKind::ALL.into_iter().find(|s| s.key() == k)
    }

    /// FFmpeg encodes this source.
    pub fn encoded(&self) -> bool {
        matches!(self, SourceKind::TestPattern | SourceKind::File | SourceKind::Screen | SourceKind::Camera)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MediaConfig {
    pub ffmpeg: String,
    pub source: SourceKind,
    pub path: String,
    pub codec: Codec,
    /// Empty = the first one FFmpeg has for the codec.
    pub encoder: String,
    /// Picture width; 0 = from the channel capacity.
    pub width: u32,
    /// Frames per second; 0 = from the channel capacity.
    pub fps: u32,
    /// Video bit rate [kb/s]; 0 = from the channel capacity.
    pub video_kbps: u32,
    /// DirectShow device names.
    pub camera: String,
    /// Video file: encode all of it to a transport stream before transmitting, instead of
    /// while transmitting. The encoder may then be slower than real time.
    pub pre_encode: bool,
    /// Size of the encoder's rate buffer (VBV) in seconds of video bit rate; 0 = half a second.
    pub vbv_s: f32,
    /// Packets kept ready between FFmpeg and the DVB coder, in seconds; 0 = half a second.
    pub buffer_s: f32,
    /// Lower the video bit rate while transmitting when the encoder makes more than the
    /// multiplex holds.
    pub watchdog: bool,
    /// Seconds between key frames; 0 = from the bit rate (auto_gop_seconds).
    pub gop_s: f32,
    pub bframes: bool,
    /// Encoder effort, an index into SPEEDS.
    pub speed: u8,
    pub audio: AudioCodec,
    /// Sound bit rate [kb/s]; 0 = from the channel capacity.
    pub audio_kbps: u32,
    /// Empty = no sound for the screen and camera sources.
    pub microphone: String,
    pub service_name: String,
    pub provider: String,
}

impl Default for MediaConfig {
    fn default() -> Self {
        MediaConfig {
            ffmpeg: "ffmpeg".into(),
            source: SourceKind::TestPattern,
            path: String::new(),
            codec: Codec::H264,
            encoder: String::new(),
            width: 0,
            fps: 0,
            video_kbps: 0,
            camera: String::new(),
            pre_encode: false,
            watchdog: true,
            vbv_s: 0.0,
            buffer_s: 0.0,
            gop_s: 0.0,
            bframes: true,
            speed: DEFAULT_SPEED,
            audio: AudioCodec::Mp2,
            audio_kbps: 0,
            microphone: String::new(),
            service_name: "ESP32-C3 DATV".into(),
            provider: "ESP32-DATV".into(),
        }
    }
}

fn hidden(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

/// Names of the video and audio encoders the installed FFmpeg has.
pub fn installed_encoders(ffmpeg: &str) -> Result<Vec<String>, String> {
    let out = hidden(Command::new(ffmpeg).args(["-hide_banner", "-encoders"]))
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {ffmpeg}: {e} (install FFmpeg or set its path)"))?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let flags = w.next()?;
            let name = w.next()?;
            (flags.len() == 6 && (flags.starts_with('V') || flags.starts_with('A')) && name != "=").then(|| name.to_string())
        })
        .collect())
}

/// DirectShow devices: (cameras, microphones).
pub fn dshow_devices(ffmpeg: &str) -> (Vec<String>, Vec<String>) {
    let out = hidden(Command::new(ffmpeg).args(["-hide_banner", "-list_devices", "true", "-f", "dshow", "-i", "dummy"]))
        .stdin(Stdio::null())
        .output();
    let (mut v, mut a) = (Vec::new(), Vec::new());
    if let Ok(out) = out {
        for l in String::from_utf8_lossy(&out.stderr).lines() {
            let Some(q1) = l.find('"') else { continue };
            let Some(q2) = l[q1 + 1..].find('"') else { continue };
            let name = l[q1 + 1..q1 + 1 + q2].to_string();
            let rest = &l[q1 + 1 + q2..];
            if rest.contains("(video)") {
                v.push(name);
            } else if rest.contains("(audio)") {
                a.push(name);
            }
        }
    }
    (v, a)
}

/// How the encoder is doing, from FFmpeg's progress reports.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct EncStats {
    /// Frames per second it encodes.
    pub fps: f32,
    /// Encoding speed against real time: 1 keeps up, below 1 falls behind.
    pub speed: f32,
    /// Seconds of video encoded so far.
    pub out_s: f64,
    pub drops: u64,
    pub dups: u64,
}

/// Reads one line of `-progress` output into the statistics. False if it is something else
/// (a message for the log).
pub fn parse_progress(line: &str, st: &mut EncStats) -> bool {
    let Some((k, v)) = line.split_once('=') else { return false };
    let v = v.trim();
    match k.trim() {
        "fps" => st.fps = v.parse().unwrap_or(st.fps),
        "speed" => {} // FFmpeg's figure is the average since the start: see track_progress
        "out_time_us" => st.out_s = v.parse::<f64>().map(|u| u / 1e6).unwrap_or(st.out_s),
        "drop_frames" => st.drops = v.parse().unwrap_or(st.drops),
        "dup_frames" => st.dups = v.parse().unwrap_or(st.dups),
        "frame" | "bitrate" | "total_size" | "out_time_ms" | "out_time" | "progress" => {}
        k if k.starts_with("stream_") => {}
        _ => return false,
    }
    true
}

/// Takes a line of FFmpeg's stderr: true if it was a progress report. At the end of each
/// report the speed is worked out from the video encoded since the last one (smoothed), so
/// it shows how the encoder is doing now.
fn track_progress(line: &str, stats: &Mutex<EncStats>, last: &mut Option<(f64, std::time::Instant)>) -> bool {
    let mut st = stats.lock().unwrap();
    if !parse_progress(line, &mut st) {
        return false;
    }
    if line.starts_with("progress=") {
        let now = std::time::Instant::now();
        match *last {
            Some((out, t)) if now.duration_since(t).as_secs_f64() >= 0.3 => {
                let inst = ((st.out_s - out) / now.duration_since(t).as_secs_f64()) as f32;
                st.speed = if st.speed > 0.0 { 0.8 * st.speed + 0.2 * inst } else { inst };
                *last = Some((st.out_s, now));
            }
            None if st.out_s > 0.0 => *last = Some((st.out_s, now)),
            _ => {}
        }
    }
    true
}

/// Packets the queue between FFmpeg and the DVB coder holds for a channel capacity.
pub fn queue_packets(capacity: f64, buffer_s: f32) -> usize {
    let s = if buffer_s > 0.0 { buffer_s.clamp(0.1, 10.0) } else { 0.5 };
    ((capacity / 1504.0 * s as f64) as usize).clamp(16, 20_000)
}

/// What FFmpeg is asked to make.
#[derive(Clone, Debug)]
pub struct EncodePlan {
    pub args: Vec<String>,
    pub encoder: String,
    pub width: u32,
    pub fps: u32,
    pub video_bps: u32,
    pub audio_kbps: u32,
    pub audio: AudioCodec,
    pub mux_bps: u32,
    /// Frames between key frames.
    pub gop: u32,
    pub bframes: bool,
    /// The whole file is encoded before transmitting.
    pub pre_encode: bool,
}

impl EncodePlan {
    pub fn summary(&self, codec: Codec) -> String {
        format!(
            "{} with {}: {} px wide, {} kb/s at {} fps, key frame every {:.1} s{}, {}, multiplex {} kb/s",
            codec.label(),
            self.encoder,
            self.width,
            self.video_bps / 1000,
            self.fps,
            self.gop as f32 / self.fps as f32,
            if self.bframes { ", B frames" } else { "" },
            if self.audio_kbps == 0 {
                "no sound".to_string()
            } else {
                format!("{} sound {} kb/s", self.audio.key().to_uppercase(), self.audio_kbps)
            },
            self.mux_bps / 1000
        )
    }
}

/// The FFmpeg command line for a channel capacity [bit/s]. The budget (sound, picture
/// size, frame rate, PSI repetition) is the one of host/tx_dvbs.py.
pub fn encode_plan(m: &MediaConfig, capacity: f64, installed: &[String]) -> Result<EncodePlan, String> {
    let mux = (capacity * 0.965) as u32; // slightly below capacity: the rest is null padding
    let (aud, ach, ar, fps, pat, wmax) = if mux >= 600_000 {
        (96, 2, 48000, 25, 0.2, 640)
    } else if mux >= 200_000 {
        (32, 1, 24000, 15, 0.5, 320)
    } else {
        (if mux < 40_000 { 8 } else { 16 }, 1, 16000, 10, 1.0, 160)
    };
    let fps = if m.fps > 0 { m.fps.clamp(1, 60) } else { fps };
    let live = matches!(m.source, SourceKind::Screen | SourceKind::Camera);
    let sound = m.audio != AudioCodec::Off && (!live || !m.microphone.is_empty());
    if sound && !m.audio.available(installed) {
        return Err(format!("this FFmpeg has no {} encoder ({})", m.audio.label(), m.audio.encoder().unwrap_or("")));
    }
    let (aud, ar) = m.audio.fit(if m.audio_kbps > 0 { m.audio_kbps } else { aud }, ar);
    let ach = if m.audio == AudioCodec::HeAacV2 { 2 } else { ach }; // parametric stereo needs two channels
    let aud_bps = if sound { aud * 1000 } else { 0 };
    let w = if m.width == 0 { wmax } else { m.width } / 2 * 2;
    let psi = (3.0 * 188.0 * 8.0 / pat) as i64;
    let vb = if m.video_kbps > 0 {
        m.video_kbps as i64 * 1000
    } else {
        // Every picture is its own PES packet: a header and on average half a transport
        // packet of stuffing, about 110 bytes, whatever the bit rate. On a narrow channel
        // that is a good part of it (tx_dvbs.py allows a flat 12 %, too little below 100 kb/s).
        let packing = fps as i64 * 900;
        ((mux as i64 - aud_bps as i64 * 103 / 100 - psi - packing) as f64 * 0.95) as i64
    };
    if vb < 6000 {
        return Err(format!(
            "channel capacity {:.1} kb/s is too small for video: use a higher symbol rate or FEC",
            capacity / 1000.0
        ));
    }
    let avail = m.codec.encoders(installed);
    let enc = if m.encoder.is_empty() {
        avail.first().cloned().ok_or_else(|| {
            format!("this FFmpeg has no {} encoder ({})", m.codec.label(), m.codec.known_encoders().join(", "))
        })?
    } else if avail.contains(&m.encoder) {
        m.encoder.clone()
    } else {
        return Err(format!("this FFmpeg has no encoder {}", m.encoder));
    };
    let s = |x: &str| x.to_string();
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-nostdin"].map(s).to_vec();
    a.extend(["-progress", "pipe:2", "-stats_period", "0.5"].map(s));
    let pre = m.pre_encode && m.source == SourceKind::File;
    let mut vf = format!("scale={w}:-2,fps={fps}");
    match m.source {
        SourceKind::TestPattern => {
            vf.clear();
            a.extend(["-re", "-f", "lavfi", "-i"].map(s));
            a.push(format!("testsrc2=size={w}x{}:rate={fps}", w * 9 / 16 / 2 * 2));
            a.extend(["-f", "lavfi", "-i"].map(s));
            a.push(format!("sine=frequency=800:sample_rate={ar}"));
        }
        SourceKind::File => {
            if m.path.is_empty() {
                return Err("choose a video file".into());
            }
            if !pre {
                a.extend(["-re", "-stream_loop", "-1"].map(s));
            }
            a.push(s("-i"));
            a.push(m.path.clone());
        }
        SourceKind::Screen => {
            a.extend(["-f", "gdigrab", "-framerate"].map(s));
            a.push(fps.to_string());
            a.extend(["-i", "desktop"].map(s));
            if sound {
                a.extend(["-f", "dshow", "-i"].map(s));
                a.push(format!("audio={}", m.microphone));
            }
        }
        SourceKind::Camera => {
            if m.camera.is_empty() {
                return Err("choose a camera".into());
            }
            a.extend(["-f", "dshow", "-rtbufsize", "64M", "-i"].map(s));
            a.push(if sound {
                format!("video={}:audio={}", m.camera, m.microphone)
            } else {
                format!("video={}", m.camera)
            });
        }
        _ => return Err("this source is not encoded".into()),
    }
    if !vf.is_empty() {
        a.push(s("-vf"));
        a.push(vf);
    }
    let gop_s = if m.gop_s > 0.0 { m.gop_s.clamp(0.2, 30.0) } else { auto_gop_seconds(vb as u32) };
    let g = ((gop_s * fps as f32).round() as u32).max(1);
    let bframes = m.bframes && has_bframes_option(&enc);
    let bf = if bframes { "2" } else { "0" };
    let vbv_s = if m.vbv_s > 0.0 { m.vbv_s.clamp(0.1, 5.0) } else { 0.5 };
    let (mut vbs, half, kb) = (vb.to_string(), ((vb as f32 * vbv_s) as i64).to_string(), vb / 1000);
    a.push(s("-c:v"));
    a.push(enc.clone());
    let mut pix = "yuv420p";
    a.extend(speed_args(&enc, m.speed).into_iter().map(s));
    match enc.as_str() {
        "libx264" => {
            a.extend(["-profile:v", "main", "-bf", bf].map(s));
            a.extend(["-x264-params", "nal-hrd=cbr:force-cfr=1"].map(s));
        }
        "libx265" => {
            a.push(s("-x265-params"));
            a.push(format!(
                "log-level=error:hrd=1:vbv-maxrate={kb}:vbv-bufsize={}:keyint={g}:min-keyint={g}:bframes={bf}:repeat-headers=1",
                ((kb as f32 * vbv_s) as i64).max(1)
            ));
        }
        "libvvenc" => {
            // vvenc takes 10 bit input only; only its fastest preset keeps up beyond small pictures
            pix = "yuv420p10le";
            a.extend(["-period", &(gop_s.ceil() as u32).max(1).to_string(), "-vvenc-params"].map(s));
            a.push(format!("verbosity=0:maxrate={vb}"));
            // vvenc wants the maximum at least 1.5 times the target: aim lower, the channel is the maximum
            vbs = (vb * 2 / 3).to_string();
        }
        "libsvtav1" => {
            a.push(s("-svtav1-params"));
            a.push(s("rc=2:pred-struct=1:lookahead=0"));
            vbs = (vb * 9 / 10).to_string(); // the constant bit rate mode of SVT-AV1 runs about a tenth over
        }
        "libaom-av1" => {
            a.extend(["-usage", "realtime", "-lag-in-frames", "0", "-end-usage", "cbr"].map(s));
        }
        e if e.ends_with("_nvenc") => {
            a.extend(["-rc", "cbr"].map(s));
            // B frames in HEVC need a recent GPU: ask only H.264 for them, but always honour the tick being off
            if !m.bframes || e == "h264_nvenc" {
                a.extend(["-bf", bf].map(s));
            }
        }
        e if has_bframes_option(e) => a.extend(["-bf", bf].map(s)),
        _ => {}
    }
    a.extend(["-b:v", &vbs, "-g", &g.to_string()].map(s));
    if !matches!(enc.as_str(), "libx265" | "libvvenc" | "libsvtav1") {
        a.extend(["-maxrate", &vbs, "-bufsize", &half].map(s));
    }
    a.extend(["-pix_fmt", pix].map(s));
    if sound {
        a.extend(["-c:a", m.audio.encoder().unwrap_or("mp2")].map(s));
        a.extend(m.audio.extra_args().iter().map(|x| s(x)));
        a.push(s("-b:a"));
        a.push(format!("{aud}k"));
        a.extend(["-ac", &ach.to_string(), "-ar", &ar.to_string()].map(s));
    } else {
        a.push(s("-an"));
    }
    // a file encoded ahead is sent at exactly the channel rate, so it plays in real time
    a.extend(["-f", "mpegts", "-muxrate"].map(s));
    a.push(if pre { (capacity as u32).to_string() } else { mux.to_string() });
    // the multiplexer must hold back the clock by more than the rate buffer can delay a frame
    a.push(s("-muxdelay"));
    a.push(format!("{:.2}", (vbv_s + 0.2).max(0.7)));
    a.extend(["-pcr_period", if aud > 50 { "40" } else { "100" }, "-pat_period"].map(s));
    a.push(pat.to_string());
    // without this FFmpeg hands over its output 32 kB at a time: seconds of a narrow channel in one lump
    a.extend(["-flush_packets", "1"].map(s));
    a.extend(["-mpegts_flags", "+resend_headers", "-metadata"].map(s));
    a.push(format!("service_provider={}", m.provider));
    a.push(s("-metadata"));
    a.push(format!("service_name={}", m.service_name));
    a.push(s("-"));
    Ok(EncodePlan { args: a, encoder: enc, width: w, fps, video_bps: vb as u32, audio_kbps: aud_bps / 1000, audio: m.audio, mux_bps: mux, gop: g, bframes, pre_encode: pre })
}

/// Lines for the user: FFmpeg's messages and the transmitter's own.
#[derive(Clone, Default)]
pub struct Log(Arc<Mutex<VecDeque<String>>>);

impl Log {
    pub fn push(&self, line: impl Into<String>) {
        let mut q = self.0.lock().unwrap();
        q.push_back(line.into());
        while q.len() > 400 {
            q.pop_front();
        }
    }

    pub fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().iter().cloned().collect()
    }
}

/// What the source has delivered: all packets, and the null packets among them. FFmpeg
/// fills its constant rate multiplex with null packets; when it sends none, picture and
/// sound take all of it and the stream starts to run late.
#[derive(Default)]
pub struct Flow {
    pub packets: AtomicU64,
    pub nulls: AtomicU64,
}

/// The counters of a source that a replacement source carries on.
#[derive(Clone)]
pub struct Shared {
    pub stats: Arc<Mutex<EncStats>>,
    pub queued: Arc<AtomicUsize>,
    pub flow: Arc<Flow>,
}

/// Delivers 188-byte packets; `take` pads with null packets when the source runs dry.
pub struct TsSource {
    rx: Option<Receiver<Packet>>,
    child: Option<Child>,
    stop: Arc<AtomicBool>,
    pub pkts: Arc<AtomicU64>,
    pub nulls: Arc<AtomicU64>,
    tap: Option<std::fs::File>,
    preview: Option<SyncSender<Vec<u8>>>,
    /// Blocks of packets the preview decoder was too slow for.
    pub preview_lost: Arc<AtomicU64>,
    pub stats: Arc<Mutex<EncStats>>,
    /// Packets waiting between the source and the DVB coder, and how many fit.
    pub queued: Arc<AtomicUsize>,
    pub queue_cap: usize,
    pub flow: Arc<Flow>,
}

/// Aligns a byte stream on the 0x47 sync bytes and sends whole packets.
struct Framer {
    buf: Vec<u8>,
    tx: SyncSender<Packet>,
    fix: Option<Av1Fix>,
    stop: Arc<AtomicBool>,
    out: Vec<Packet>,
    queued: Arc<AtomicUsize>,
    flow: Arc<Flow>,
}

impl Framer {
    /// False when the receiver is gone.
    fn push(&mut self, d: &[u8]) -> bool {
        self.buf.extend_from_slice(d);
        let mut i = 0;
        while self.buf.len() - i >= 376 {
            if self.buf[i] != 0x47 || self.buf[i + 188] != 0x47 {
                i += 1;
                continue;
            }
            let mut p = [0u8; 188];
            p.copy_from_slice(&self.buf[i..i + 188]);
            i += 188;
            match &mut self.fix {
                Some(f) => f.push(&p, &mut self.out),
                None => self.out.push(p),
            }
            for mut q in self.out.drain(..) {
                loop {
                    match self.tx.try_send(q) {
                        Ok(()) => {
                            self.flow.packets.fetch_add(1, Ordering::Relaxed);
                            if q[1] & 0x1F == 0x1F && q[2] == 0xFF {
                                self.flow.nulls.fetch_add(1, Ordering::Relaxed);
                            }
                            self.queued.fetch_add(1, Ordering::Relaxed);
                            break;
                        }
                        Err(TrySendError::Full(back)) => {
                            if self.stop.load(Ordering::Relaxed) {
                                return false;
                            }
                            q = back;
                            std::thread::sleep(std::time::Duration::from_millis(2));
                        }
                        Err(TrySendError::Disconnected(_)) => return false,
                    }
                }
            }
        }
        self.buf.drain(..i);
        true
    }
}

impl TsSource {
    fn empty() -> Self {
        TsSource {
            rx: None,
            child: None,
            stop: Arc::new(AtomicBool::new(false)),
            pkts: Default::default(),
            nulls: Default::default(),
            tap: None,
            preview: None,
            preview_lost: Default::default(),
            stats: Default::default(),
            queued: Default::default(),
            queue_cap: 0,
            flow: Default::default(),
        }
    }

    /// Null packets only.
    pub fn null() -> Self {
        Self::empty()
    }

    /// Copies everything that goes on air to a file (for checking a dry run).
    pub fn tap(&mut self, path: &str) -> Result<(), String> {
        self.tap = Some(std::fs::File::create(path).map_err(|e| format!("{path}: {e}"))?);
        Ok(())
    }

    /// Sends a copy of the packets (without the null packets) to the preview decoder.
    pub fn preview(&mut self, to: SyncSender<Vec<u8>>) {
        self.preview = Some(to);
    }

    /// A transport stream file, looped. `av1`: FFmpeg's AV1 that still needs its carriage
    /// rewritten (a file encoded ahead by this program). `queue`: packets kept ready.
    pub fn ts_file(path: &str, log: &Log, av1: bool, queue: usize) -> Result<Self, String> {
        let mut f = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
        let mut s = Self::empty();
        let (tx, rx) = sync_channel(queue);
        s.rx = Some(rx);
        s.queue_cap = queue;
        let fix = av1.then(Av1Fix::new);
        let mut fr = Framer {
            buf: Vec::new(),
            tx,
            fix,
            stop: s.stop.clone(),
            out: Vec::new(),
            queued: s.queued.clone(),
            flow: s.flow.clone(),
        };
        let (log, path) = (log.clone(), path.to_string());
        std::thread::spawn(move || {
            use std::io::Seek;
            let mut d = vec![0u8; 188 * 64];
            loop {
                match f.read(&mut d) {
                    Ok(0) => {
                        if f.stream_position().unwrap_or(0) < 376 || f.rewind().is_err() {
                            log.push(format!("{path}: no transport stream packets"));
                            return;
                        }
                    }
                    Ok(n) => {
                        if !fr.push(&d[..n]) {
                            return;
                        }
                    }
                    Err(e) => {
                        log.push(format!("{path}: {e}"));
                        return;
                    }
                }
            }
        });
        Ok(s)
    }

    /// The counters, for a source that will take this one's place.
    pub fn shared(&self) -> Shared {
        Shared { stats: self.stats.clone(), queued: self.queued.clone(), flow: self.flow.clone() }
    }

    /// Puts another source in this one's place (the encoder started again with other
    /// settings): its packets from now on, the same counters, copy and preview. The old
    /// FFmpeg ends.
    pub fn replace(&mut self, mut new: TsSource) {
        std::mem::swap(&mut self.rx, &mut new.rx);
        std::mem::swap(&mut self.child, &mut new.child);
        std::mem::swap(&mut self.stop, &mut new.stop);
        self.queue_cap = new.queue_cap;
        drop(new); // holds the old FFmpeg now
    }

    /// FFmpeg encoding the configured source. `queue`: packets kept ready. `carry`: the
    /// counters of the source this one replaces.
    pub fn ffmpeg(
        m: &MediaConfig,
        plan: &EncodePlan,
        log: &Log,
        queue: usize,
        carry: Option<Shared>,
    ) -> Result<Self, String> {
        let mut child = hidden(Command::new(&m.ffmpeg).args(&plan.args))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run {}: {e} (install FFmpeg or set its path)", m.ffmpeg))?;
        let mut out = child.stdout.take().unwrap();
        let err = child.stderr.take().unwrap();
        let mut s = Self::empty();
        let (tx, rx) = sync_channel(queue);
        s.rx = Some(rx);
        s.queue_cap = queue;
        if let Some(c) = carry {
            c.queued.store(0, Ordering::Relaxed); // what the old source had waiting is dropped
            *c.stats.lock().unwrap() = EncStats::default();
            (s.stats, s.queued, s.flow) = (c.stats, c.queued, c.flow);
        }
        let fix = (m.codec == Codec::Av1).then(Av1Fix::new);
        let mut fr = Framer {
            buf: Vec::new(),
            tx,
            fix,
            stop: s.stop.clone(),
            out: Vec::new(),
            queued: s.queued.clone(),
            flow: s.flow.clone(),
        };
        let l = log.clone();
        let stats = s.stats.clone();
        std::thread::spawn(move || {
            let mut last = None;
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                if track_progress(&line, &stats, &mut last) {
                    continue;
                }
                if !line.trim().is_empty() && !line.starts_with("Svt[") {
                    l.push(format!("ffmpeg: {line}"));
                }
            }
        });
        let (l, stop) = (log.clone(), s.stop.clone());
        std::thread::spawn(move || {
            let mut d = vec![0u8; 188 * 16];
            loop {
                match out.read(&mut d) {
                    Ok(0) | Err(_) => {
                        if !stop.load(Ordering::Relaxed) {
                            l.push("ffmpeg stopped: null packets from now on");
                        }
                        return;
                    }
                    Ok(n) => {
                        if !fr.push(&d[..n]) {
                            return;
                        }
                    }
                }
            }
        });
        s.child = Some(child);
        Ok(s)
    }

    /// Packets waiting to be taken. A source without packets of its own (null packets,
    /// carrier) always has enough.
    pub fn ready(&self) -> usize {
        if self.rx.is_some() {
            self.queued.load(Ordering::Relaxed)
        } else {
            usize::MAX
        }
    }

    /// n packets, null packets where the source has none ready.
    pub fn take(&mut self, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(188 * n);
        let mut copy = Vec::new();
        for _ in 0..n {
            match self.rx.as_ref().and_then(|r| r.try_recv().ok()) {
                Some(p) => {
                    out.extend_from_slice(&p);
                    let _ = self.queued.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |q| q.checked_sub(1));
                    if self.preview.is_some() && (p[1] & 0x1F != 0x1F || p[2] != 0xFF) {
                        copy.extend_from_slice(&p);
                    }
                    self.pkts.fetch_add(1, Ordering::Relaxed);
                }
                None => {
                    out.extend_from_slice(&NULL_PACKET);
                    self.nulls.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        if let (Some(to), false) = (&self.preview, copy.is_empty()) {
            if to.try_send(copy).is_err() {
                // a preview that is behind loses packets, the air does not
                self.preview_lost.fetch_add(1, Ordering::Relaxed);
            }
        }
        if let Some(f) = &mut self.tap {
            let _ = f.write_all(&out);
        }
        out
    }
}

/// Encodes the whole file to a transport stream on disk and returns its path. An earlier
/// encoding of the same file with the same settings is used again. `progress` is called a
/// few times a second; `stop` ends it early.
pub fn pre_encode(
    m: &MediaConfig,
    plan: &EncodePlan,
    stop: &AtomicBool,
    log: &Log,
    progress: &mut dyn FnMut(EncStats),
) -> Result<String, String> {
    use std::hash::{Hash, Hasher};
    let meta = std::fs::metadata(&m.path).map_err(|e| format!("{}: {e}", m.path))?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    plan.args.hash(&mut h);
    meta.len().hash(&mut h);
    meta.modified().ok().hash(&mut h);
    let dir = std::env::temp_dir().join("datv-tx-cache");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let done = dir.join(format!("{:016x}.ts", h.finish()));
    if std::fs::metadata(&done).map(|d| d.len() > 376).unwrap_or(false) {
        log.push("this file was encoded with these settings before: using that");
        return Ok(done.display().to_string());
    }
    // one file is kept: the cache does not grow
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let part = dir.join("encoding.part");
    // FFmpeg writes to a pipe and the file is written here: should this program die, the
    // pipe breaks and FFmpeg ends with it instead of encoding on unseen
    let mut file = std::fs::File::create(&part).map_err(|e| format!("{}: {e}", part.display()))?;
    let mut child = hidden(Command::new(&m.ffmpeg).args(&plan.args))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {}: {e} (install FFmpeg or set its path)", m.ffmpeg))?;
    let err = child.stderr.take().unwrap();
    let mut out = child.stdout.take().unwrap();
    let writer = std::thread::spawn(move || std::io::copy(&mut out, &mut file).is_ok() && file.flush().is_ok());
    let stats = Arc::new(Mutex::new(EncStats::default()));
    let (st, l) = (stats.clone(), log.clone());
    let reader = std::thread::spawn(move || {
        let mut last = None;
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            if track_progress(&line, &st, &mut last) {
                continue;
            }
            if !line.trim().is_empty() && !line.starts_with("Svt[") {
                l.push(format!("ffmpeg: {line}"));
            }
        }
    });
    let status = loop {
        if stop.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&part);
            return Err("stopped while encoding the file".into());
        }
        progress(*stats.lock().unwrap());
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(200)),
            Err(e) => return Err(format!("ffmpeg: {e}")),
        }
    };
    let _ = reader.join();
    let written = writer.join().unwrap_or(false);
    progress(*stats.lock().unwrap());
    if !written {
        let _ = std::fs::remove_file(&part);
        return Err(format!("could not write {}", part.display()));
    }
    if !status.success() {
        let _ = std::fs::remove_file(&part);
        return Err("FFmpeg could not encode the file (see the messages)".into());
    }
    std::fs::rename(&part, &done).map_err(|e| format!("{}: {e}", done.display()))?;
    Ok(done.display().to_string())
}

impl Drop for TsSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

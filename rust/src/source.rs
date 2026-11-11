//! Transport stream sources. FFmpeg (the one installed on the PC) encodes the picture and
//! sound and multiplexes them at a rate that fits the channel; the video codec and the
//! encoder are the user's choice. After the source handling in host/tx_dvbs.py by SP8ESA.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// Video bit rate [kb/s]; 0 = from the channel capacity.
    pub video_kbps: u32,
    /// DirectShow device names.
    pub camera: String,
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
            video_kbps: 0,
            camera: String::new(),
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

/// Names of the video encoders the installed FFmpeg has.
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
            (flags.len() == 6 && flags.starts_with('V') && name != "=").then(|| name.to_string())
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

/// What FFmpeg is asked to make.
#[derive(Clone, Debug)]
pub struct EncodePlan {
    pub args: Vec<String>,
    pub encoder: String,
    pub width: u32,
    pub fps: u32,
    pub video_bps: u32,
    pub audio_kbps: u32,
    pub mux_bps: u32,
}

impl EncodePlan {
    pub fn summary(&self, codec: Codec) -> String {
        format!(
            "{} with {}: {} px wide, {} kb/s at {} fps, sound {} kb/s, multiplex {} kb/s",
            codec.label(),
            self.encoder,
            self.width,
            self.video_bps / 1000,
            self.fps,
            self.audio_kbps,
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
    let live = matches!(m.source, SourceKind::Screen | SourceKind::Camera);
    let sound = !live || !m.microphone.is_empty();
    let aud_bps = if sound { aud * 1000 } else { 0 };
    let w = if m.width == 0 { wmax } else { m.width } / 2 * 2;
    let psi = (3.0 * 188.0 * 8.0 / pat) as i64;
    let vb = if m.video_kbps > 0 {
        m.video_kbps as i64 * 1000
    } else {
        ((mux as i64 - aud_bps as i64 - psi) as f64 * 0.88) as i64
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
            a.extend(["-re", "-stream_loop", "-1", "-i"].map(s));
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
    let g = 2 * fps;
    let (mut vbs, half, kb) = (vb.to_string(), (vb / 2).to_string(), vb / 1000);
    a.push(s("-c:v"));
    a.push(enc.clone());
    let mut pix = "yuv420p";
    match enc.as_str() {
        "libx264" => {
            a.extend(["-preset", "veryfast", "-profile:v", "main", "-bf", "2"].map(s));
            a.extend(["-x264-params", "nal-hrd=cbr:force-cfr=1"].map(s));
        }
        "libx265" => {
            a.extend(["-preset", "veryfast", "-x265-params"].map(s));
            a.push(format!(
                "log-level=error:hrd=1:vbv-maxrate={kb}:vbv-bufsize={}:keyint={g}:min-keyint={g}:bframes=2:repeat-headers=1",
                (kb / 2).max(1)
            ));
        }
        "libvvenc" => {
            // vvenc takes 10 bit input only; its fastest preset keeps up with small pictures
            pix = "yuv420p10le";
            a.extend(["-preset", "faster", "-period", "2", "-vvenc-params"].map(s));
            a.push(format!("verbosity=0:maxrate={vb}"));
            // vvenc wants the maximum at least 1.5 times the target: aim lower, the channel is the maximum
            vbs = (vb * 2 / 3).to_string();
        }
        "libsvtav1" => {
            a.extend(["-preset", "10", "-svtav1-params"].map(s));
            a.push(s("rc=2:pred-struct=1:lookahead=0"));
            vbs = (vb * 9 / 10).to_string(); // the constant bit rate mode of SVT-AV1 runs about a tenth over
        }
        "libaom-av1" => {
            a.extend(["-cpu-used", "9", "-usage", "realtime", "-lag-in-frames", "0", "-end-usage", "cbr"].map(s));
        }
        e if e.ends_with("_nvenc") => {
            a.extend(["-preset", "p4", "-rc", "cbr"].map(s));
        }
        _ => {}
    }
    a.extend(["-b:v", &vbs, "-g", &g.to_string()].map(s));
    if !matches!(enc.as_str(), "libx265" | "libvvenc" | "libsvtav1") {
        a.extend(["-maxrate", &vbs, "-bufsize", &half].map(s));
    }
    a.extend(["-pix_fmt", pix].map(s));
    if sound {
        a.extend(["-c:a", "mp2", "-b:a"].map(s));
        a.push(format!("{aud}k"));
        a.extend(["-ac", &ach.to_string(), "-ar", &ar.to_string()].map(s));
    } else {
        a.push(s("-an"));
    }
    a.extend(["-f", "mpegts", "-muxrate"].map(s));
    a.push(mux.to_string());
    a.extend(["-pcr_period", if aud > 50 { "40" } else { "100" }, "-pat_period"].map(s));
    a.push(pat.to_string());
    a.extend(["-mpegts_flags", "+resend_headers", "-metadata"].map(s));
    a.push(format!("service_provider={}", m.provider));
    a.push(s("-metadata"));
    a.push(format!("service_name={}", m.service_name));
    a.push(s("-"));
    Ok(EncodePlan { args: a, encoder: enc, width: w, fps, video_bps: vb as u32, audio_kbps: aud_bps / 1000, mux_bps: mux })
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

/// Delivers 188-byte packets; `take` pads with null packets when the source runs dry.
pub struct TsSource {
    rx: Option<Receiver<Packet>>,
    child: Option<Child>,
    stop: Arc<AtomicBool>,
    pub pkts: Arc<AtomicU64>,
    pub nulls: Arc<AtomicU64>,
    tap: Option<std::fs::File>,
}

/// Aligns a byte stream on the 0x47 sync bytes and sends whole packets.
struct Framer {
    buf: Vec<u8>,
    tx: SyncSender<Packet>,
    fix: Option<Av1Fix>,
    stop: Arc<AtomicBool>,
    out: Vec<Packet>,
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
                        Ok(()) => break,
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

    /// A transport stream file, looped.
    pub fn ts_file(path: &str, log: &Log) -> Result<Self, String> {
        let mut f = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
        let mut s = Self::empty();
        let (tx, rx) = sync_channel(400);
        s.rx = Some(rx);
        let mut fr = Framer { buf: Vec::new(), tx, fix: None, stop: s.stop.clone(), out: Vec::new() };
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

    /// FFmpeg encoding the configured source.
    pub fn ffmpeg(m: &MediaConfig, plan: &EncodePlan, log: &Log) -> Result<Self, String> {
        let mut child = hidden(Command::new(&m.ffmpeg).args(&plan.args))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run {}: {e} (install FFmpeg or set its path)", m.ffmpeg))?;
        let mut out = child.stdout.take().unwrap();
        let err = child.stderr.take().unwrap();
        let mut s = Self::empty();
        let (tx, rx) = sync_channel(400);
        s.rx = Some(rx);
        let fix = (m.codec == Codec::Av1).then(Av1Fix::new);
        let mut fr = Framer { buf: Vec::new(), tx, fix, stop: s.stop.clone(), out: Vec::new() };
        let l = log.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
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

    /// n packets, null packets where the source has none ready.
    pub fn take(&mut self, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(188 * n);
        for _ in 0..n {
            match self.rx.as_ref().and_then(|r| r.try_recv().ok()) {
                Some(p) => {
                    out.extend_from_slice(&p);
                    self.pkts.fetch_add(1, Ordering::Relaxed);
                }
                None => {
                    out.extend_from_slice(&NULL_PACKET);
                    self.nulls.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        if let Some(f) = &mut self.tap {
            let _ = f.write_all(&out);
        }
        out
    }
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

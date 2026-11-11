//! Preview of what is on air: a second FFmpeg decodes a copy of the transport stream that
//! goes into the DVB coder (after encoding, so the picture shows the codec at work, as the
//! Flutter app does) and hands back small RGB pictures for the window.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};

pub const W: usize = 480;
pub const H: usize = 270;

#[derive(Default)]
pub struct Frame {
    /// Counts the pictures; 0 = none yet.
    pub seq: u64,
    /// W x H RGB24.
    pub rgb: Vec<u8>,
}

pub type Shared = Arc<Mutex<Frame>>;

pub struct Preview {
    child: Child,
    tx: SyncSender<Vec<u8>>,
}

/// AV1 as this program sends it (AOM draft carriage, see av1ts.rs) -> a plain OBU stream.
/// FFmpeg cannot demultiplex that carriage yet, so the preview undoes it here.
#[derive(Default)]
struct Av1Unpack {
    pid: Option<u16>,
    pes: Vec<u8>,
}

impl Av1Unpack {
    fn push(&mut self, ts: &[u8], out: &mut Vec<u8>) {
        for p in ts.chunks_exact(188) {
            let pid = ((p[1] & 0x1F) as u16) << 8 | p[2] as u16;
            let at = match p[3] >> 4 & 3 {
                1 => 4,
                3 if (p[4] as usize) < 183 => 5 + p[4] as usize,
                _ => continue,
            };
            let d = &p[at..];
            let start = p[1] & 0x40 != 0;
            if self.pid.is_none() && start && d.len() > 12 && d[..4] == [0, 0, 1, 0xBD] {
                // private stream 1 whose data begins with a start code (AC-3 begins 0B 77)
                let body = 9 + d[8] as usize;
                if d.len() >= body + 3 && d[body..body + 3] == [0, 0, 1] {
                    self.pid = Some(pid);
                }
            }
            if Some(pid) != self.pid {
                continue;
            }
            if start {
                self.flush(out);
            }
            self.pes.extend_from_slice(d);
        }
    }

    fn flush(&mut self, out: &mut Vec<u8>) {
        let pes = std::mem::take(&mut self.pes);
        if pes.len() < 9 || 9 + pes[8] as usize > pes.len() {
            return;
        }
        let b = &pes[9 + pes[8] as usize..];
        out.extend_from_slice(&[0x12, 0x00]); // temporal delimiter
        let mut i = 0;
        while i < b.len() {
            if b[i..].starts_with(&[0, 0, 1]) {
                i += 3; // OBU start code
            } else if b[i..].starts_with(&[0, 0, 3]) {
                out.extend_from_slice(&[0, 0]); // emulation prevention byte
                i += 3;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
    }
}

impl Preview {
    /// `av1`: the stream carries AV1 from this program. `capacity` [bit/s] sizes the probing,
    /// so a narrow channel does not wait long for its first picture.
    pub fn start(ffmpeg: &str, av1: bool, capacity: f64, shared: Shared) -> Result<Self, String> {
        let probe = ((capacity / 8.0 * 1.5) as u64).clamp(16_000, 500_000).to_string();
        let vf = format!("fps=5,scale={W}:{H}:force_original_aspect_ratio=decrease,pad={W}:{H}:(ow-iw)/2:(oh-ih)/2");
        let mut cmd = Command::new(ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "quiet", "-flags", "low_delay"]);
        if !av1 {
            cmd.args(["-fflags", "nobuffer"]); // with the OBU reader this option breaks frames
        }
        cmd.args(["-probesize", &probe, "-analyzeduration", "1000000"]);
        cmd.args(["-f", if av1 { "obu" } else { "mpegts" }, "-i", "-", "-an", "-vf", &vf]);
        cmd.args(["-f", "rawvideo", "-pix_fmt", "rgb24", "-"]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("preview: cannot run {ffmpeg}: {e}"))?;
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let (tx, rx) = sync_channel::<Vec<u8>>(256);
        std::thread::spawn(move || {
            let mut unpack = av1.then(Av1Unpack::default);
            let mut obu = Vec::new();
            for d in rx {
                let r = match &mut unpack {
                    Some(u) => {
                        obu.clear();
                        u.push(&d, &mut obu);
                        stdin.write_all(&obu)
                    }
                    None => stdin.write_all(&d),
                };
                if r.is_err() {
                    return;
                }
            }
        });
        std::thread::spawn(move || {
            let mut rgb = vec![0u8; W * H * 3];
            // for tests: DATV_PREVIEW_DUMP names a file that gets every preview picture
            let mut dump = std::env::var_os("DATV_PREVIEW_DUMP").and_then(|p| std::fs::File::create(p).ok());
            while stdout.read_exact(&mut rgb).is_ok() {
                if let Some(f) = &mut dump {
                    let _ = f.write_all(&rgb);
                }
                let mut f = shared.lock().unwrap();
                f.seq += 1;
                f.rgb.clone_from(&rgb);
            }
        });
        Ok(Preview { child, tx })
    }

    /// Where the source sends its packets (dropped when the decoder is behind).
    pub fn sender(&self) -> SyncSender<Vec<u8>> {
        self.tx.clone()
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

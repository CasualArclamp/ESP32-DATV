//! The transmit session: source -> DVB coder -> ESP, keeping the ESP's symbol buffer filled.
//! A port of the main loop of host/tx_dvbs.py by SP8ESA.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::link::{Link, SerialLink, SimLink};
use crate::plan::{plan, start_command, AnyEncoder, RfConfig};
use crate::preview::{Preview, Shared};
use crate::source::{
    encode_plan, installed_encoders, pre_encode, queue_packets, Codec, EncStats, Log, MediaConfig, SourceKind, TsSource,
};

#[derive(Clone, Debug, PartialEq)]
pub struct TxConfig {
    pub rf: RfConfig,
    pub media: MediaConfig,
    /// COM port of the board.
    pub port: String,
    /// No board, no RF: the whole chain runs into a simulated ESP.
    pub dry_run: bool,
    /// Copy the transport stream that goes on air to this file.
    pub ts_out: String,
    /// Decode what goes on air for the picture in the window.
    pub preview: bool,
    /// Stop after this many seconds; 0 = until stopped.
    pub seconds: f64,
}

#[derive(Clone, Default, Debug)]
pub struct Status {
    pub running: bool,
    /// One line: what is on air.
    pub info: String,
    pub encode: String,
    pub error: String,
    pub fill_min: i64,
    pub fill_max: i64,
    pub target: i64,
    pub underruns: u8,
    pub pkts: u64,
    pub nulls: u64,
    pub seconds: f64,
    /// The file is being encoded; nothing is on air yet.
    pub preparing: bool,
    /// FFmpeg is encoding while transmitting (its speed matters).
    pub live_encode: bool,
    pub enc: EncStats,
    /// Frames per second asked of the encoder.
    pub fps: u32,
    /// Packets waiting between the source and the DVB coder, and how many fit.
    pub queue: usize,
    pub queue_cap: usize,
    /// Null packets on air in the last second, per cent: the room left in the channel.
    pub padding: f32,
    /// The encoder makes more than the channel carries: the stream is running late.
    pub late: bool,
    /// The ESP ran dry during the last second.
    pub underruns_rising: bool,
    /// Video bit rate now [kb/s] and how often the watchdog has lowered it.
    pub video_kbps: u32,
    pub lowered: u32,
    /// Transport stream capacity [bit/s].
    pub capacity: f64,
}

pub struct Session {
    /// The latest picture of the on-air preview.
    pub picture: Shared,
    pub status: Arc<Mutex<Status>>,
    pub log: Log,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Session {
    pub fn start(cfg: TxConfig, log: Log) -> Session {
        let status = Arc::new(Mutex::new(Status { running: true, ..Default::default() }));
        let stop = Arc::new(AtomicBool::new(false));
        let picture = Shared::default();
        let pic = picture.clone();
        let (st, sp, lg) = (status.clone(), stop.clone(), log.clone());
        let thread = std::thread::spawn(move || {
            if let Err(e) = run(&cfg, &st, &sp, &lg, pic) {
                lg.push(format!("error: {e}"));
                st.lock().unwrap().error = e;
            }
            st.lock().unwrap().running = false;
        });
        Session { picture, status, log, stop, thread: Some(thread) }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn running(&self) -> bool {
        self.status.lock().unwrap().running
    }

    pub fn join(&mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
        self.join();
    }
}

fn run(cfg: &TxConfig, status: &Mutex<Status>, stop: &AtomicBool, log: &Log, picture: Shared) -> Result<(), String> {
    let p = plan(&cfg.rf)?;
    for n in &p.notes {
        log.push(format!("note: {n}"));
    }
    let mut enc = AnyEncoder::new(&cfg.rf, &p)?;
    let m = cfg.rf.modulation().to_string();
    let cw = cfg.media.source == SourceKind::Carrier;

    // a DVB-S2 frame is filled in one go: the source must be able to have a few ready
    let queue = queue_packets(p.capacity, cfg.media.buffer_s).max(3 * p.frame_packets);
    // what a watchdog needs to start the encoder again: the settings and FFmpeg's encoders
    let mut live: Option<(MediaConfig, Vec<String>, u32)> = None;
    status.lock().unwrap().capacity = p.capacity;
    let mut src = match cfg.media.source {
        SourceKind::Null | SourceKind::Carrier => TsSource::null(),
        SourceKind::TsFile => TsSource::ts_file(&cfg.media.path, log, false, queue)?,
        _ => {
            let installed = installed_encoders(&cfg.media.ffmpeg)?;
            let ep = encode_plan(&cfg.media, p.capacity, &installed)?;
            let line = ep.summary(cfg.media.codec);
            log.push(format!("{line} (channel capacity {:.0} kb/s)", p.capacity / 1e3));
            {
                let mut s = status.lock().unwrap();
                s.encode = line;
                s.fps = ep.fps;
            }
            if ep.pre_encode {
                // encode all of it first: nothing is on air and the board is not opened yet
                log.push("encoding the whole file before transmitting ...");
                status.lock().unwrap().preparing = true;
                let t0 = Instant::now();
                let made = pre_encode(&cfg.media, &ep, stop, log, &mut |e| {
                    let mut s = status.lock().unwrap();
                    s.enc = e;
                    s.seconds = t0.elapsed().as_secs_f64();
                });
                status.lock().unwrap().preparing = false;
                let path = made?;
                log.push(format!("encoded in {:.0} s: {path}", t0.elapsed().as_secs_f64()));
                TsSource::ts_file(&path, log, cfg.media.codec == Codec::Av1, queue)?
            } else {
                {
                    let mut s = status.lock().unwrap();
                    s.live_encode = true;
                    s.video_kbps = ep.video_bps / 1000;
                }
                live = Some((cfg.media.clone(), installed, ep.video_bps));
                TsSource::ffmpeg(&cfg.media, &ep, log, queue, None)?
            }
        }
    };
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }
    let av1 = cfg.media.source.encoded() && cfg.media.codec == Codec::Av1;
    let _preview = if cfg.preview && (cfg.media.source.encoded() || cfg.media.source == SourceKind::TsFile) {
        match Preview::start(&cfg.media.ffmpeg, av1, p.capacity, picture) {
            Ok(pv) => {
                src.preview(pv.sender());
                Some(pv)
            }
            Err(e) => {
                log.push(e);
                None
            }
        }
    } else {
        None
    };
    if !cfg.ts_out.is_empty() {
        src.tap(&cfg.ts_out)?;
    }
    let (pkts, nulls, lost) = (src.pkts.clone(), src.nulls.clone(), src.preview_lost.clone());
    let (enc_stats, src_queued, queue_cap) = (src.stats.clone(), src.queued.clone(), src.queue_cap);
    let shared = src.shared();
    let (swap_tx, swap_rx) = channel::<TsSource>();
    let (mut last_flow, mut last_flow_nulls, mut last_under) = (0u64, 0u64, 0u8);
    // seconds since the encoder (re)started
    let mut settled = 0u32;
    let mut window: std::collections::VecDeque<(u64, u64, bool)> = Default::default();
    let (mut last_pkts, mut last_nulls) = (0u64, 0u64);

    let mut link: Box<dyn Link> = if cfg.dry_run {
        Box::new(SimLink::new(p.bytes_per_sec))
    } else {
        if cfg.port.is_empty() {
            return Err("choose the board's COM port".into());
        }
        log.push(format!("opening {} ...", cfg.port));
        Box::new(SerialLink::open(&cfg.port)?)
    };
    let secs = if cfg.seconds > 0.0 { cfg.seconds as u64 + 30 } else { 86400 };
    let info = link.start(&start_command(&cfg.rf, &p, secs))?;
    let std = if cfg.rf.dvbs2 {
        format!(
            "DVB-S2 {} {} frame{}",
            m.to_uppercase(),
            cfg.rf.frame,
            if cfg.rf.pilots { ", pilots" } else { "" }
        )
    } else {
        "DVB-S".to_string()
    };
    let on_air = format!(
        "{}{:.3} MHz, {:.1} Bd, {std} FEC {}, RRC 0.35 occupies {:.0} kHz, TS capacity {:.0} kb/s",
        if cw { "UNMODULATED CARRIER, " } else { "" },
        cfg.rf.freq_mhz,
        p.baud_act,
        cfg.rf.fec,
        p.baud_act * 1.35 / 1e3,
        p.capacity / 1e3
    );
    log.push(info);
    log.push(on_air.clone());
    {
        let mut s = status.lock().unwrap();
        s.info = on_air;
        s.target = p.target;
    }

    // The coder runs ahead of the USB writes in its own thread (as in tx_dvbs.py), but only
    // by a quarter of a second of symbols: what it takes from the source is on air that much
    // later, and at low symbol rates a longer queue would be many seconds of delay.
    let (ready_tx, ready_rx) = channel::<Vec<u8>>();
    let queued = Arc::new(AtomicUsize::new(0));
    // (For DVB-S2 at least a frame and a half: the next frame is begun while the last is
    // still going out, which is the time there is to wait for the source.)
    let limit = ((p.bytes_per_sec * 0.25) as usize).max(p.frame_bytes * 3 / 2);
    // With less than this ready for the ESP nothing may wait any longer.
    let low_water = ((p.bytes_per_sec * 0.1) as usize).max(p.frame_bytes / 2);
    let enc_stop = Arc::new(AtomicBool::new(false));
    let (es, qd) = (enc_stop.clone(), queued.clone());
    let cw_byte = if m == "qpsk" { 0xFFu8 } else { 0x00 };
    let coder = std::thread::spawn(move || {
        while !es.load(Ordering::Relaxed) {
            if let Ok(new) = swap_rx.try_recv() {
                src.replace(new);
            }
            if qd.load(Ordering::Relaxed) > limit {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            // The source runs a little slower than the channel. While enough symbols are
            // ready, wait for its packets; only when the ESP would run short, fill up with
            // null packets. (Taking whatever is there at once pads long before it is needed,
            // and every needless null packet makes the stream later.)
            if !cw && qd.load(Ordering::Relaxed) > low_water && src.ready() < 8 {
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
            let data = if cw { vec![cw_byte; 1024] } else { enc.encode(&src.take(8)) };
            qd.fetch_add(data.len(), Ordering::Relaxed);
            if !data.is_empty() && ready_tx.send(data).is_err() {
                return;
            }
        }
    });

    let t_start = Instant::now();
    let mut t_rep = t_start;
    let (mut fmin, mut fmax) = (i64::MAX, 0i64);
    let mut buf: Vec<u8> = Vec::new();
    let mut result = Ok(());
    while !stop.load(Ordering::Relaxed) && (cfg.seconds <= 0.0 || t_start.elapsed().as_secs_f64() < cfg.seconds) {
        link.poll();
        let st = *link.state();
        if st.reports > 0 {
            fmin = fmin.min(st.fill);
            fmax = fmax.max(st.fill);
        }
        let todo = p.target - (st.fill + st.sent_since); // pairs of 2 bytes
        if todo >= p.min_todo {
            let mut nb = 2 * todo.min(p.max_chunk) as usize;
            while buf.len() < nb {
                match ready_rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(d) => {
                        buf.extend_from_slice(&d);
                    }
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            nb = nb.min(buf.len());
            if p.a16s8 {
                nb -= nb % 64; // whole USB packets: the 1 MS/s loop drains eight-byte groups
            }
            if nb > 0 {
                if let Err(e) = link.send(&buf[..nb]) {
                    result = Err(e);
                    break;
                }
                buf.drain(..nb);
                queued.fetch_sub(nb, Ordering::Relaxed); // counted until it has gone to the ESP
                link.state().sent_since += nb as i64 / 2;
            }
        } else {
            std::thread::sleep(Duration::from_micros(500));
        }
        if t_rep.elapsed() >= Duration::from_secs(1) {
            let mut s = status.lock().unwrap();
            if fmin != i64::MAX {
                s.fill_min = fmin;
                s.fill_max = fmax;
            }
            s.underruns = st.under;
            s.pkts = pkts.load(Ordering::Relaxed);
            s.nulls = nulls.load(Ordering::Relaxed);
            let (dp, dn) = (s.pkts - last_pkts, s.nulls - last_nulls);
            (last_pkts, last_nulls) = (s.pkts, s.nulls);
            // null packets on air: the ones added here and the ones FFmpeg put in its multiplex
            let (fp, fnl) = (shared.flow.packets.load(Ordering::Relaxed), shared.flow.nulls.load(Ordering::Relaxed));
            let (dfp, dfn) = (fp - last_flow, fnl - last_flow_nulls);
            (last_flow, last_flow_nulls) = (fp, fnl);
            s.padding = if dp + dn > 0 { 100.0 * (dn + dfn.min(dp)) as f32 / (dp + dn) as f32 } else { 0.0 };
            s.underruns_rising = st.under != last_under;
            last_under = st.under;
            // Watchdog: FFmpeg's multiplex has a fixed rate. When the encoder makes more than
            // was asked (it cannot squeeze the picture any further), FFmpeg has no null packets
            // left to send, the stream runs later and later and its packets pile up here. Both
            // seen over 6 seconds, the encoder is started again with 15 % less. (No null
            // packets alone is not it: a well filled multiplex has few, and at low rates they
            // come in bursts. A full buffer alone is not it either: every encoder starts so.)
            settled += 1;
            let piled_up = src_queued.load(Ordering::Relaxed) * 10 >= queue_cap * 9;
            window.push_back((dfp, dfn, piled_up));
            if window.len() > 6 {
                window.pop_front();
            }
            let (wp, wn) = window.iter().fold((0, 0), |a, w| (a.0 + w.0, a.1 + w.1));
            s.late = window.len() == 6 && window.iter().all(|w| w.2) && wp > 0 && wn * 200 < wp;

            if let (Some((media, installed, vb)), true) = (&mut live, cfg.media.watchdog && settled > 8 && s.late) {
                let lower = (*vb as f32 * 0.85) as u32;
                if lower >= 6000 {
                    media.video_kbps = (lower / 1000).max(6);
                    match encode_plan(media, p.capacity, installed)
                        .and_then(|ep| Ok((TsSource::ffmpeg(media, &ep, log, queue, Some(shared.clone()))?, ep)))
                    {
                        Ok((new, ep)) => {
                            log.push(format!(
                                "watchdog: the encoder makes more than the channel carries: video {} -> {} kb/s",
                                *vb / 1000,
                                ep.video_bps / 1000
                            ));
                            *vb = ep.video_bps;
                            s.video_kbps = ep.video_bps / 1000;
                            s.lowered += 1;
                            s.encode = ep.summary(media.codec);
                            let _ = swap_tx.send(new);
                        }
                        Err(e) => log.push(format!("watchdog: {e}")),
                    }
                } else if s.lowered < 1000 {
                    log.push("watchdog: the video bit rate cannot go lower: use a smaller picture or a lower frame rate");
                    s.lowered += 1000; // said once
                }
                settled = 0;
                window.clear();
            }
            s.enc = *enc_stats.lock().unwrap();
            s.queue = src_queued.load(Ordering::Relaxed);
            s.queue_cap = queue_cap;
            s.seconds = t_start.elapsed().as_secs_f64();
            fmin = i64::MAX;
            fmax = 0;
            t_rep = Instant::now();
        }
    }
    enc_stop.store(true, Ordering::Relaxed);
    drop(ready_rx);
    let _ = coder.join(); // ends FFmpeg with the source
    log.push(link.finish(!p.a16s8));
    if lost.load(Ordering::Relaxed) > 0 {
        log.push(format!("preview: {} blocks of packets skipped (decoder behind)", lost.load(Ordering::Relaxed)));
    }
    let mut s = status.lock().unwrap();
    s.pkts = pkts.load(Ordering::Relaxed);
    s.nulls = nulls.load(Ordering::Relaxed);
    s.seconds = t_start.elapsed().as_secs_f64();
    result
}

//! The transmit session: source -> DVB coder -> ESP, keeping the ESP's symbol buffer filled.
//! A port of the main loop of host/tx_dvbs.py by SP8ESA.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::link::{Link, SerialLink, SimLink};
use crate::plan::{plan, start_command, AnyEncoder, RfConfig};
use crate::preview::{Preview, Shared};
use crate::source::{encode_plan, installed_encoders, Codec, Log, MediaConfig, SourceKind, TsSource};

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

    let mut src = match cfg.media.source {
        SourceKind::Null | SourceKind::Carrier => TsSource::null(),
        SourceKind::TsFile => TsSource::ts_file(&cfg.media.path, log)?,
        _ => {
            let installed = installed_encoders(&cfg.media.ffmpeg)?;
            let ep = encode_plan(&cfg.media, p.capacity, &installed)?;
            let line = ep.summary(cfg.media.codec);
            log.push(format!("{line} (channel capacity {:.0} kb/s)", p.capacity / 1e3));
            status.lock().unwrap().encode = line;
            TsSource::ffmpeg(&cfg.media, &ep, log)?
        }
    };
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
    let limit = (p.bytes_per_sec * 0.25) as usize;
    let enc_stop = Arc::new(AtomicBool::new(false));
    let (es, qd) = (enc_stop.clone(), queued.clone());
    let cw_byte = if m == "qpsk" { 0xFFu8 } else { 0x00 };
    let coder = std::thread::spawn(move || {
        while !es.load(Ordering::Relaxed) {
            if qd.load(Ordering::Relaxed) > limit {
                std::thread::sleep(Duration::from_millis(1));
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
                        queued.fetch_sub(d.len(), Ordering::Relaxed);
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

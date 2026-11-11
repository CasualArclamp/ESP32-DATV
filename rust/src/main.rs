//! DATV TX: a Windows transmitter for the ESP32-DATV firmware with a choice of video codec.
//!
//! FFmpeg (installed on the PC) encodes the picture as H.264, H.265 (HEVC), H.266 (VVC) or
//! AV1 and the sound as MPEG Layer II; this program does the DVB-S / DVB-S2 coding and feeds
//! the ESP32-C3 over USB, like host/tx_dvbs.py of the ESP32-DATV project by SP8ESA.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use datv_tx::plan::{plan, Plan};
use datv_tx::prefs;
use datv_tx::source::{dshow_devices, encode_plan, encoder_label, installed_encoders, AudioCodec, Codec, Log, SourceKind, SPEEDS};
use datv_tx::tx::{Session, TxConfig};
use datv_tx::{dvbs, dvbs2, link, preview};
use eframe::egui;

const RATES: [u64; 7] = [33_000, 66_000, 125_000, 250_000, 333_000, 500_000, 1_000_000];

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut cfg = prefs::load();
    cfg.dry_run = false; // never start on the air without it being chosen in this run
    let mut start = false;
    let rest: Vec<String> = args
        .into_iter()
        .filter(|a| {
            let s = a == "--start";
            start |= s;
            !s
        })
        .collect();
    let mut arg_error = prefs::apply_args(&mut cfg, &rest).err();
    if start && !cfg.dry_run {
        // the command line cannot key the transmitter: Start on the air is a click in the window
        arg_error = Some("--start needs --dry-run".into());
        start = false;
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 720.0])
            .with_min_inner_size([760.0, 520.0])
            .with_title("DATV TX for ESP32-DATV"),
        ..Default::default()
    };
    eframe::run_native(
        "DATV TX",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            let mut app = App::new(cfg);
            if let Some(e) = arg_error {
                app.log.push(format!("command line: {e}"));
            }
            if start {
                app.start();
            }
            Ok(Box::new(app))
        }),
    )
}

struct App {
    cfg: TxConfig,
    saved: TxConfig,
    session: Option<Session>,
    log: Log,
    encoders: Result<Vec<String>, String>,
    ports: Vec<(String, String, bool)>,
    cameras: Vec<String>,
    mics: Vec<String>,
    /// The preview picture and the number of the one it shows.
    picture: Option<egui::TextureHandle>,
    picture_seq: u64,
}

impl App {
    fn new(cfg: TxConfig) -> Self {
        let mut a = App {
            saved: cfg.clone(),
            encoders: installed_encoders(&cfg.media.ffmpeg),
            cfg,
            session: None,
            log: Log::default(),
            ports: Vec::new(),
            cameras: Vec::new(),
            mics: Vec::new(),
            picture: None,
            picture_seq: 0,
        };
        a.refresh_devices();
        a
    }

    fn refresh_devices(&mut self) {
        self.ports = link::ports();
        if self.cfg.port.is_empty() {
            if let Some(p) = self.ports.iter().find(|p| p.2) {
                self.cfg.port = p.0.clone();
            }
        }
        self.encoders = installed_encoders(&self.cfg.media.ffmpeg);
        if self.encoders.is_ok() {
            (self.cameras, self.mics) = dshow_devices(&self.cfg.media.ffmpeg);
        }
    }

    fn running(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.running())
    }

    fn start(&mut self) {
        self.session = None; // ends a finished one
        self.picture = None;
        self.picture_seq = 0;
        prefs::save(&self.cfg);
        self.saved = self.cfg.clone();
        self.log.push(if self.cfg.dry_run { "--- dry run ---" } else { "--- transmit ---" });
        self.session = Some(Session::start(self.cfg.clone(), self.log.clone()));
    }

    /// The code rates of the chosen standard, modulation and frame; keeps the choice valid.
    fn fecs(&mut self) -> Vec<&'static str> {
        let rf = &mut self.cfg.rf;
        let list: Vec<&'static str> =
            if rf.dvbs2 { dvbs2::rates(&rf.modulation, &rf.frame) } else { dvbs::FECS.to_vec() };
        if !list.contains(&rf.fec.as_str()) {
            rf.fec = list.iter().find(|f| **f == "2/3").or(list.first()).unwrap_or(&"1/2").to_string();
        }
        list
    }

    fn settings(&mut self, ui: &mut egui::Ui) {
        let fecs = self.fecs();
        ui.heading("Board");
        egui::Grid::new("board").num_columns(2).show(ui, |ui| {
            ui.label("Port");
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("port").selected_text(self.cfg.port.clone()).width(170.0).show_ui(
                    ui,
                    |ui| {
                        for (name, what, esp) in &self.ports {
                            let label = format!("{name}  {what}{}", if *esp { "  (Espressif)" } else { "" });
                            ui.selectable_value(&mut self.cfg.port, name.clone(), label);
                        }
                    },
                );
                if ui.button("Refresh").on_hover_text("Look again for ports, cameras and FFmpeg encoders").clicked() {
                    self.refresh_devices();
                }
            });
            ui.end_row();
        });

        ui.add_space(8.0);
        ui.heading("Signal");
        egui::Grid::new("rf").num_columns(2).show(ui, |ui| {
            let rf = &mut self.cfg.rf;
            ui.label("Frequency");
            ui.add(egui::DragValue::new(&mut rf.freq_mhz).range(2300.0..=2450.0).speed(0.05).fixed_decimals(3).suffix(" MHz"));
            ui.end_row();
            ui.label("Standard");
            ui.horizontal(|ui| {
                ui.selectable_value(&mut rf.dvbs2, false, "DVB-S");
                ui.selectable_value(&mut rf.dvbs2, true, "DVB-S2");
            });
            ui.end_row();
            if rf.dvbs2 {
                ui.label("Modulation");
                ui.horizontal(|ui| {
                    for (k, l) in [("qpsk", "QPSK"), ("8psk", "8PSK"), ("16apsk", "16APSK")] {
                        ui.selectable_value(&mut rf.modulation, k.to_string(), l);
                    }
                });
                ui.end_row();
            }
            ui.label("Symbol rate");
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("baud").selected_text(format!("{} kS/s", rf.baud as f64 / 1000.0)).show_ui(
                    ui,
                    |ui| {
                        for r in RATES {
                            ui.selectable_value(&mut rf.baud, r, format!("{} kS/s", r / 1000));
                        }
                    },
                );
                ui.add(egui::DragValue::new(&mut rf.baud).range(2000..=1_000_000).speed(100).suffix(" Bd"));
            });
            ui.end_row();
            ui.label("FEC");
            egui::ComboBox::from_id_salt("fec").selected_text(rf.fec.clone()).show_ui(ui, |ui| {
                for f in &fecs {
                    ui.selectable_value(&mut rf.fec, f.to_string(), *f);
                }
            });
            ui.end_row();
            if rf.dvbs2 {
                ui.label("Frame");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut rf.frame, "normal".to_string(), "Normal");
                    ui.selectable_value(&mut rf.frame, "short".to_string(), "Short");
                    ui.checkbox(&mut rf.pilots, "Pilots");
                });
                ui.end_row();
            }
        });
        ui.collapsing("More", |ui| {
            let rf = &mut self.cfg.rf;
            egui::Grid::new("rf2").num_columns(2).show(ui, |ui| {
                ui.label("Crystal error");
                ui.add(egui::DragValue::new(&mut rf.ppm).speed(0.1).range(-200.0..=200.0).suffix(" ppm"));
                ui.end_row();
                ui.label("Amplitude");
                ui.add(egui::DragValue::new(&mut rf.amp).range(0..=480).custom_formatter(|v, _| {
                    if v == 0.0 {
                        "Automatic".to_string()
                    } else {
                        format!("{v:.0}")
                    }
                }))
                    .on_hover_text("DAC codes, 1..480. 0 = automatic: 300 for QPSK, 420 for 8PSK and 16APSK");
                ui.end_row();
                ui.label("LO offset");
                ui.add(egui::DragValue::new(&mut rf.ifm).range(-4..=4).suffix(" x symbol rate"))
                    .on_hover_text("Puts the LO leakage outside the signal");
                ui.end_row();
            });
            ui.checkbox(&mut rf.invert, "Invert the spectrum");
            ui.checkbox(&mut rf.swap_iq, "Swap I and Q");
            let mut cal = prefs::has_cal(rf);
            if ui
                .checkbox(&mut cal, "Example I/Q calibration")
                .on_hover_text("The DC and I/Q trim of host/cal.json, as host/tx_dvbs.py applies by default")
                .changed()
            {
                prefs::set_cal(rf, cal);
            }
        });

        ui.add_space(8.0);
        ui.heading("Source");
        let m = &mut self.cfg.media;
        egui::ComboBox::from_id_salt("source").selected_text(m.source.label()).width(250.0).show_ui(ui, |ui| {
            for s in SourceKind::ALL {
                let l = s.label();
                ui.selectable_value(&mut m.source, s, l);
            }
        });
        match m.source {
            SourceKind::File | SourceKind::TsFile => {
                ui.horizontal(|ui| {
                    if ui.button("Browse...").clicked() {
                        let d = rfd::FileDialog::new();
                        let d = if m.source == SourceKind::TsFile {
                            d.add_filter("Transport stream", &["ts"])
                        } else {
                            d.add_filter("Video", &["mp4", "mkv", "mov", "avi", "webm", "ts", "m4v", "mpg"])
                        };
                        if let Some(p) = d.add_filter("All files", &["*"]).pick_file() {
                            m.path = p.display().to_string();
                        }
                    }
                    ui.add(egui::TextEdit::singleline(&mut m.path).hint_text("file").desired_width(f32::INFINITY));
                });
            }
            SourceKind::Screen | SourceKind::Camera => {
                egui::Grid::new("dev").num_columns(2).show(ui, |ui| {
                    if m.source == SourceKind::Camera {
                        ui.label("Camera");
                        egui::ComboBox::from_id_salt("cam").selected_text(m.camera.clone()).width(220.0).show_ui(ui, |ui| {
                            for c in &self.cameras {
                                ui.selectable_value(&mut m.camera, c.clone(), c);
                            }
                        });
                        ui.end_row();
                    }
                    ui.label("Sound");
                    let none = "None".to_string();
                    let shown = if m.microphone.is_empty() { none.clone() } else { m.microphone.clone() };
                    egui::ComboBox::from_id_salt("mic").selected_text(shown).width(220.0).show_ui(ui, |ui| {
                        ui.selectable_value(&mut m.microphone, String::new(), none);
                        for c in &self.mics {
                            ui.selectable_value(&mut m.microphone, c.clone(), c);
                        }
                    });
                    ui.end_row();
                });
            }
            _ => {}
        }

        if m.source.encoded() {
            ui.add_space(8.0);
            ui.heading("Video and sound");
            let installed = self.encoders.clone().unwrap_or_default();
            egui::Grid::new("video").num_columns(2).show(ui, |ui| {
                ui.label("Codec");
                let before = m.codec;
                egui::ComboBox::from_id_salt("codec").selected_text(m.codec.label()).width(220.0).show_ui(ui, |ui| {
                    for c in Codec::ALL {
                        let have = !c.encoders(&installed).is_empty();
                        let label = if have { c.label().to_string() } else { format!("{} (not in this FFmpeg)", c.label()) };
                        ui.add_enabled_ui(have, |ui| ui.selectable_value(&mut m.codec, c, label));
                    }
                });
                let list = m.codec.encoders(&installed);
                if m.codec != before || (!m.encoder.is_empty() && !list.contains(&m.encoder)) {
                    m.encoder.clear();
                }
                ui.end_row();
                ui.label("Encoder");
                let shown = match (m.encoder.is_empty(), list.first()) {
                    (true, Some(f)) => encoder_label(f),
                    (true, None) => "none".to_string(),
                    _ => encoder_label(&m.encoder),
                };
                egui::ComboBox::from_id_salt("encoder").selected_text(shown).width(220.0).show_ui(ui, |ui| {
                    for (i, e) in list.iter().enumerate() {
                        let value = if i == 0 { String::new() } else { e.clone() };
                        ui.selectable_value(&mut m.encoder, value, encoder_label(e));
                    }
                });
                ui.end_row();
                ui.label("Speed");

                ui.horizontal(|ui| {
                    ui.add(egui::Slider::new(&mut m.speed, 0..=4).show_value(false));
                    ui.label(SPEEDS[m.speed.min(4) as usize]);
                })
                .response
                .on_hover_text(
                    "Encoder effort. Slower gives a better picture for the same bit rate but needs more \
                     processor; if the picture stutters or the padding drops to nothing, go faster.",
                );

                ui.end_row();
                ui.label("Width");
                egui::ComboBox::from_id_salt("width")
                    .selected_text(if m.width == 0 { "Automatic".to_string() } else { format!("{} px", m.width) })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut m.width, 0, "Automatic");
                        for w in [160, 320, 480, 640, 854, 960, 1280, 1920] {
                            ui.selectable_value(&mut m.width, w, format!("{w} px"));
                        }
                    });
                ui.end_row();
                ui.label("Frame rate");
                egui::ComboBox::from_id_salt("fps")
                    .selected_text(if m.fps == 0 { "Automatic".to_string() } else { format!("{} fps", m.fps) })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut m.fps, 0, "Automatic");
                        for f in [5, 10, 15, 24, 25, 30, 50, 60] {
                            ui.selectable_value(&mut m.fps, f, format!("{f} fps"));
                        }
                    })
                    .response
                    .on_hover_text("Automatic: 25, 15 or 10 fps by channel capacity");
                ui.end_row();
                ui.label("Video bit rate");
                ui.add(egui::DragValue::new(&mut m.video_kbps).range(0..=3000).custom_formatter(|v, _| {
                    if v == 0.0 {
                        "Automatic".to_string()
                    } else {
                        format!("{v:.0} kb/s")
                    }
                }))
                    .on_hover_text("0 = as much as the channel carries");
                ui.end_row();
                ui.label("Sound codec");
                egui::ComboBox::from_id_salt("audio").selected_text(m.audio.label()).width(220.0).show_ui(ui, |ui| {
                    for c in AudioCodec::ALL {
                        let have = c.available(&installed);
                        let label = if have { c.label().to_string() } else { format!("{} (not in this FFmpeg)", c.label()) };
                        ui.add_enabled_ui(have, |ui| ui.selectable_value(&mut m.audio, c, label));
                    }
                });
                ui.end_row();
                if m.audio != AudioCodec::Off {
                    ui.label("Sound bit rate");
                    ui.add(egui::DragValue::new(&mut m.audio_kbps).range(0..=320).custom_formatter(|v, _| {
                        if v == 0.0 {
                            "Automatic".to_string()
                        } else {
                            format!("{v:.0} kb/s")
                        }
                    }))
                    .on_hover_text("Automatic: 96, 32, 16 or 8 kb/s by channel capacity (AC-3 at least 32)");
                    ui.end_row();
                }
                ui.label("Service name");
                ui.add(egui::TextEdit::singleline(&mut m.service_name).desired_width(220.0));
                ui.end_row();
            });
            ui.collapsing("FFmpeg", |ui| {
                ui.horizontal(|ui| {
                    ui.label("Program");
                    if ui.add(egui::TextEdit::singleline(&mut m.ffmpeg).desired_width(220.0)).lost_focus() {
                        self.encoders = installed_encoders(&m.ffmpeg);
                    }
                });
            });
        }
    }

    /// What the settings give: the channel and what FFmpeg will be asked for, or why not.
    fn preview(&self) -> (Result<Plan, String>, Option<Result<String, String>>) {
        let p = plan(&self.cfg.rf);
        let enc = match (&p, self.cfg.media.source.encoded()) {
            (Ok(p), true) => Some(match &self.encoders {
                Ok(inst) => encode_plan(&self.cfg.media, p.capacity, inst).map(|e| e.summary(self.cfg.media.codec)),
                Err(e) => Err(e.clone()),
            }),
            _ => None,
        };
        (p, enc)
    }

    fn main_panel(&mut self, ui: &mut egui::Ui) {
        let running = self.running();
        let (p, enc) = self.preview();
        let red = egui::Color32::from_rgb(230, 90, 80);
        match &p {
            Ok(p) => {
                ui.label(format!(
                    "Channel: {:.1} kS/s, occupies {:.0} kHz, carries {:.0} kb/s",
                    p.baud_act / 1e3,
                    p.baud_act * 1.35 / 1e3,
                    p.capacity / 1e3
                ));
                for n in &p.notes {
                    ui.label(n);
                }
            }
            Err(e) => {
                ui.colored_label(red, e);
            }
        }
        match &enc {
            Some(Ok(s)) => {
                ui.label(s);
            }
            Some(Err(e)) => {
                ui.colored_label(red, e);
            }
            None => {}
        }
        if self.cfg.media.source.encoded() && self.cfg.media.codec != Codec::H264 {
            ui.weak(match self.cfg.media.codec {
                Codec::Hevc => "The receiver must decode H.265.",
                Codec::Vvc => "The receiver must decode H.266 (VVC); few do yet. It is encoded with 10 bits.",
                _ => "The receiver must read AV1 in a transport stream (AOM draft signalling); few do yet.",
            });
        }
        ui.add_space(8.0);
        let ok = p.is_ok() && !matches!(enc, Some(Err(_)));
        ui.horizontal(|ui| {
            if running {
                if ui.add_sized([140.0, 36.0], egui::Button::new("Stop")).clicked() {
                    if let Some(s) = &self.session {
                        s.stop();
                    }
                }
            } else {
                let label = if self.cfg.dry_run { "Start dry run" } else { "Transmit" };
                if ui.add_enabled(ok, egui::Button::new(label).min_size([140.0, 36.0].into())).clicked() {
                    self.start();
                }
            }
            ui.add_enabled(!running, egui::Checkbox::new(&mut self.cfg.dry_run, "Dry run (no board, no RF)"))
                .on_hover_text("Runs FFmpeg and the DVB coder into a simulated ESP. Nothing is transmitted.");
            ui.add_enabled(!running, egui::Checkbox::new(&mut self.cfg.preview, "Preview"))
                .on_hover_text("Decodes a copy of the stream that goes on air and shows its picture.");
        });
        ui.add_space(8.0);
        if let Some(s) = &self.session {
            {
                let f = s.picture.lock().unwrap();
                if f.seq != self.picture_seq && f.rgb.len() == preview::W * preview::H * 3 {
                    self.picture_seq = f.seq;
                    let img = egui::ColorImage::from_rgb([preview::W, preview::H], &f.rgb);
                    match &mut self.picture {
                        Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                        None => self.picture = Some(ui.ctx().load_texture("on air", img, egui::TextureOptions::LINEAR)),
                    }
                }
            }
            let st = s.status.lock().unwrap().clone();
            if st.running {
                let on_air = if self.saved.dry_run { "DRY RUN" } else { "ON AIR" };
                ui.colored_label(
                    if self.saved.dry_run { egui::Color32::from_rgb(120, 180, 240) } else { red },
                    format!("{on_air}  {:.0} s", st.seconds),
                );
            }
            if !st.info.is_empty() {
                ui.label(&st.info);
            }
            if !st.encode.is_empty() {
                ui.label(&st.encode);
            }
            if st.target > 0 {
                ui.horizontal(|ui| {
                    ui.label("ESP buffer");
                    ui.add(
                        egui::ProgressBar::new(st.fill_max as f32 / st.target as f32)
                            .desired_width(200.0)
                            .text(format!("{}..{} of {}", st.fill_min, st.fill_max, st.target)),
                    );
                    ui.label(format!("underruns {}", st.underruns));
                });
                let total = (st.pkts + st.nulls).max(1);
                ui.label(format!(
                    "Transport stream: {} packets from the source, {} null ({:.0} % padding)",
                    st.pkts,
                    st.nulls,
                    100.0 * st.nulls as f64 / total as f64
                ));
            }
            if !st.error.is_empty() {
                ui.colored_label(red, &st.error);
            }
            if let Some(t) = &self.picture {
                ui.add_space(4.0);
                let size = egui::vec2(preview::W as f32, preview::H as f32);
                ui.add(egui::Image::from_texture(t).fit_to_exact_size(size));
                ui.weak("What is on air, decoded from the transmitted stream.");
            } else if st.running && self.saved.preview && self.saved.media.source.encoded() {
                ui.weak("Preview: waiting for the first picture...");
            }
        }
        ui.separator();
        egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
            for l in self.log.lines() {
                ui.label(egui::RichText::new(l).monospace().size(12.0));
            }
        });
    }
}

impl eframe::App for App {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.session = None; // stops the transmission and FFmpeg
        let mut c = self.cfg.clone();
        c.dry_run = false;
        c.seconds = 0.0;
        c.ts_out.clear();
        prefs::save(&c);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let running = self.running();
        if running {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
        }
        egui::Panel::left("settings").default_size(360.0).resizable(true).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_enabled_ui(!running, |ui| self.settings(ui));
                ui.add_space(8.0);
                ui.weak("Amateur radio use only, within the limits of your licence.");
            });
        });
        egui::CentralPanel::default().show(ui, |ui| self.main_panel(ui));
    }
}

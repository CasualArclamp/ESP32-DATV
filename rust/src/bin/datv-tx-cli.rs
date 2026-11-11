//! Command line transmitter: the same engine as the window, for scripts and tests.
use datv_tx::prefs::{apply_args, default_config, USAGE};
use datv_tx::source::Log;
use datv_tx::tx::Session;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut c = default_config();
    c.preview = false; // no window to show it in
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        println!("datv-tx-cli: ESP32-DATV transmitter. Amateur radio use only, within your licence.\n{USAGE}");
        return;
    }
    if let Err(e) = apply_args(&mut c, &args) {
        eprintln!("{e}\n{USAGE}");
        std::process::exit(2);
    }
    if !c.dry_run && c.port.is_empty() {
        eprintln!("give the board's port (--port COMn) or --dry-run");
        std::process::exit(2);
    }
    let log = Log::default();
    let mut s = Session::start(c, log.clone());
    let mut shown = 0;
    let mut last = 0.0;
    loop {
        let running = s.running();
        let l = log.lines();
        for x in &l[shown.min(l.len())..] {
            println!("{x}");
        }
        shown = l.len();
        let st = s.status.lock().unwrap().clone();
        if st.seconds >= last + 2.0 {
            last = st.seconds;
            println!(
                "  ESP buffer {}..{} of {} pairs, underruns {}, TS packets: {} from the source, {} null",
                st.fill_min, st.fill_max, st.target, st.underruns, st.pkts, st.nulls
            );
        }
        if !running {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    s.join();
    if !s.status.lock().unwrap().error.is_empty() {
        std::process::exit(1);
    }
}

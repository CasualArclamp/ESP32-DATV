//! The coders must give the bytes of the Python reference (host/dvbs.py, host/dvbs2.py):
//! sha256 hashes from app/test/fixtures/dvb_goldens.json (written by app/tool/gen_dvb_goldens.py).
use datv_tx::{dvbs, dvbs2};
use sha2::{Digest, Sha256};

fn golden_ts(n: usize) -> Vec<u8> {
    let mut x: u32 = 0x2545F491;
    let mut out = Vec::new();
    for _ in 0..n {
        out.push(0x47);
        for _ in 0..187 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            out.push((x >> 11) as u8);
        }
    }
    out
}

fn hex(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

fn goldens() -> serde_json::Value {
    serde_json::from_str(include_str!("../../app/test/fixtures/dvb_goldens.json")).unwrap()
}

#[test]
fn dvbs_matches_python() {
    let g = goldens();
    let ts = golden_ts(400);
    for (key, want) in g["dvbs"].as_object().unwrap() {
        let p: Vec<&str> = key.split('|').collect();
        let mut e = dvbs::Encoder::new(p[0], p[1] == "1", p[2] == "1").unwrap();
        let mut out = e.encode(&ts[..188 * 7]);
        out.extend(e.encode(&ts[188 * 7..]));
        assert_eq!(hex(&out), want.as_str().unwrap(), "DVB-S {key}");
    }
}

#[test]
fn dvbs2_matches_python() {
    let g = goldens();
    let ts = golden_ts(400);
    let list = g["dvbs2"].as_array().unwrap();
    assert!(list.len() > 100);
    for c in list {
        let b = |k: &str| c[k].as_bool().unwrap();
        let s = |k: &str| c[k].as_str().unwrap();
        let mut e =
            dvbs2::Encoder::new(s("fec"), s("frame"), b("pilots"), b("swap"), b("invert"), s("mod"), b("bits3")).unwrap();
        let out = e.encode(&ts);
        assert_eq!(out.len() as u64, c["len"].as_u64().unwrap(), "length {c}");
        assert_eq!(hex(&out), s("sha256"), "{c}");
    }
}

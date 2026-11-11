//! DVB-S2 encoder (ETSI EN 302 307): MPEG transport stream -> QPSK, 8PSK or 16APSK symbols.
//!
//! A Rust port of host/dvbs2.py by SP8ESA (same chain, same output bytes): mode
//! adaptation (CRC-8 in the sync byte position, BBHEADER, clause 5.1), BB scrambler
//! (5.2.2), BCH (5.3.1), LDPC (5.3.2, annex B and C tables from host/dvbs2_ldpc.json),
//! bit interleaver (5.3.3), mapping (5.4), PLHEADER (5.5.2), pilots (5.5.3),
//! PL scrambler (5.5.4, Gold code 0).
//!
//! Output packing is what the firmware reads. QPSK: 4 symbols per byte, bit 0 = I
//! level, bit 1 = Q level, first symbol in the low bits. 8PSK: angle index k of
//! e^(j pi k / 4), 2 symbols per byte, low nibble first (or 3 bits per symbol with
//! `bits3`, used at 1 MBd). 16APSK: the bit quadruple v, 2 symbols per byte.

use std::collections::HashMap;
use std::sync::OnceLock;

const LDPC_JSON: &str = include_str!("../../host/dvbs2_ldpc.json");

pub const FEC_RATES: [&str; 11] = ["1/4", "1/3", "2/5", "1/2", "3/5", "2/3", "3/4", "4/5", "5/6", "8/9", "9/10"];

/// (k_bch, t), EN 302 307 tables 5a (normal) and 5b (short).
fn params(frame: &str, fec: &str) -> Option<(usize, usize)> {
    Some(match (frame, fec) {
        ("normal", "1/4") => (16008, 12),
        ("normal", "1/3") => (21408, 12),
        ("normal", "2/5") => (25728, 12),
        ("normal", "1/2") => (32208, 12),
        ("normal", "3/5") => (38688, 12),
        ("normal", "2/3") => (43040, 10),
        ("normal", "3/4") => (48408, 12),
        ("normal", "4/5") => (51648, 12),
        ("normal", "5/6") => (53840, 10),
        ("normal", "8/9") => (57472, 8),
        ("normal", "9/10") => (58192, 8),
        ("short", "1/4") => (3072, 12),
        ("short", "1/3") => (5232, 12),
        ("short", "2/5") => (6312, 12),
        ("short", "1/2") => (7032, 12),
        ("short", "3/5") => (9552, 12),
        ("short", "2/3") => (10632, 12),
        ("short", "3/4") => (11712, 12),
        ("short", "4/5") => (12432, 12),
        ("short", "5/6") => (13152, 12),
        ("short", "8/9") => (14232, 12),
        _ => return None,
    })
}

/// MODCOD numbers, EN 302 307 table 12.
fn modcod(modulation: &str, fec: &str) -> Option<u32> {
    let i = FEC_RATES.iter().position(|f| *f == fec)? as u32;
    match modulation {
        "qpsk" => Some(i + 1),
        "8psk" => ["3/5", "2/3", "3/4", "5/6", "8/9", "9/10"].iter().position(|f| *f == fec).map(|i| 12 + i as u32),
        "16apsk" => ["2/3", "3/4", "4/5", "5/6", "8/9", "9/10"].iter().position(|f| *f == fec).map(|i| 18 + i as u32),
        _ => None,
    }
}

/// 16APSK ring ratio R2 / R1 per code rate, EN 302 307 table 9.
pub fn apsk16_gamma(fec: &str) -> Option<f64> {
    Some(match fec {
        "2/3" => 3.15,
        "3/4" => 2.85,
        "4/5" => 2.75,
        "5/6" => 2.70,
        "8/9" => 2.60,
        "9/10" => 2.57,
        _ => return None,
    })
}

fn bits_per_symbol(modulation: &str) -> Option<usize> {
    Some(match modulation {
        "qpsk" => 2,
        "8psk" => 3,
        "16apsk" => 4,
        _ => return None,
    })
}

/// 8PSK: bit triple (b0 b1 b2) -> angle index k (as gr-dtv's modulator).
const PSK8_K: [u8; 8] = [1, 0, 4, 5, 2, 7, 3, 6];
/// 16APSK: bit quadruple v -> angle in degrees; v 0..11 outer ring, 12..15 inner.
const APSK16_ANGLE: [f64; 16] = [
    45.0, -45.0, 135.0, -135.0, 15.0, -15.0, 165.0, -165.0, 75.0, -75.0, 105.0, -105.0, 45.0, -45.0, 135.0, -135.0,
];

/// The code rates that exist for this modulation and frame size.
pub fn rates(modulation: &str, frame: &str) -> Vec<&'static str> {
    FEC_RATES
        .iter()
        .copied()
        .filter(|f| params(frame, f).is_some() && modcod(modulation, f).is_some())
        .collect()
}

pub struct FrameInfo {
    pub kbch: usize,
    pub t: usize,
    pub nldpc: usize,
    pub dfl: usize,
    pub plframe: usize,
}

pub fn frame_info(fec: &str, frame: &str, pilots: bool, modulation: &str) -> Result<FrameInfo, String> {
    let bps = bits_per_symbol(modulation).ok_or("modulation: qpsk, 8psk, 16apsk")?;
    if !rates(modulation, frame).contains(&fec) {
        return Err(format!(
            "DVB-S2 {} {} frame: FEC {}",
            modulation.to_uppercase(),
            frame,
            rates(modulation, frame).join(", ")
        ));
    }
    let (kbch, t) = params(frame, fec).unwrap();
    let n = if frame == "normal" { 64800 } else { 16200 };
    let nsym_data = n / bps;
    let slots = nsym_data / 90;
    let plframe = 90 + nsym_data + if pilots { 36 * ((slots - 1) / 16) } else { 0 };
    Ok(FrameInfo { kbch, t, nldpc: n, dfl: kbch - 80, plframe })
}

/// Useful transport stream bit rate [bit/s].
pub fn ts_rate(baud: f64, fec: &str, frame: &str, pilots: bool, modulation: &str) -> Result<f64, String> {
    let fi = frame_info(fec, frame, pilots, modulation)?;
    Ok(baud * fi.dfl as f64 / fi.plframe as f64)
}

// ---- CRC-8, x^8+x^7+x^6+x^4+x^2+1 (clause 5.1.4)
fn crc8_table() -> [u8; 256] {
    let mut tab = [0u8; 256];
    for b in 0..256usize {
        let mut c = b as u8;
        for _ in 0..8 {
            c = if c & 0x80 != 0 { (c << 1) ^ 0xD5 } else { c << 1 };
        }
        tab[b] = c;
    }
    tab
}

fn crc8(tab: &[u8; 256], data: &[u8]) -> u8 {
    data.iter().fold(0u8, |c, &b| tab[(c ^ b) as usize])
}

/// BB scrambler bits: 1 + x^14 + x^15, initial 100101010000000.
fn bb_prbs(nbits: usize) -> Vec<u8> {
    let mut reg = [1u8, 0, 0, 1, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0];
    (0..nbits)
        .map(|_| {
            let o = reg[13] ^ reg[14];
            reg.copy_within(0..14, 1);
            reg[0] = o;
            o
        })
        .collect()
}

// ---- BCH: g(x) = product of the minimal polynomials of a^1, a^3, ..., a^(2t-1)
/// Coefficients of g(x), index = power of x.
fn bch_generator(m: u32, prim: u32, t: usize) -> Vec<u8> {
    let n = (1usize << m) - 1;
    let mut exp = vec![0u32; n];
    let mut log = vec![0usize; n + 1];
    let mut x = 1u32;
    for i in 0..n {
        exp[i] = x;
        log[x as usize] = i;
        x <<= 1;
        if x >> m != 0 {
            x ^= prim;
        }
    }
    let mut g = vec![1u8];
    let mut seen = std::collections::HashSet::new();
    let mut i = 1;
    while i < 2 * t {
        let mut coset = Vec::new();
        let mut j = i;
        while !coset.contains(&j) {
            coset.push(j);
            j = (2 * j) % n;
        }
        if seen.insert(coset[0]) {
            let mut poly = vec![1u32];
            for &j in &coset {
                let mut new = vec![0u32; poly.len() + 1];
                for (k, &c) in poly.iter().enumerate() {
                    new[k + 1] ^= c;
                    if c != 0 {
                        new[k] ^= exp[(log[c as usize] + j) % n];
                    }
                }
                poly = new;
            }
            debug_assert!(poly.iter().all(|&c| c <= 1));
            let mut ng = vec![0u8; g.len() + poly.len() - 1];
            for (a, &ca) in g.iter().enumerate() {
                if ca != 0 {
                    for (b, &cb) in poly.iter().enumerate() {
                        ng[a + b] ^= cb as u8;
                    }
                }
            }
            g = ng;
        }
        i += 2;
    }
    g
}

struct Bch {
    deg: usize,
    /// g(x) without the leading term, bit i of word i / 64 = coefficient of x^i.
    g: [u64; 3],
}

impl Bch {
    fn new(frame: &str, t: usize) -> Self {
        let (m, prim) = if frame == "normal" { (16, 0x1002D) } else { (14, 0x402B) };
        let gp = bch_generator(m, prim, t);
        let deg = gp.len() - 1;
        let mut g = [0u64; 3];
        for (i, &c) in gp[..deg].iter().enumerate() {
            if c != 0 {
                g[i / 64] |= 1 << (i % 64);
            }
        }
        Bch { deg, g }
    }

    /// Message bits (highest power first) -> parity bits, highest power first.
    fn parity(&self, bits: &[u8]) -> Vec<u8> {
        let mut s = [0u64; 3];
        let top = self.deg - 1;
        for &b in bits {
            let fb = ((s[top / 64] >> (top % 64)) as u8 & 1) ^ b;
            s[2] = s[2] << 1 | s[1] >> 63;
            s[1] = s[1] << 1 | s[0] >> 63;
            s[0] <<= 1;
            if fb != 0 {
                s[0] ^= self.g[0];
                s[1] ^= self.g[1];
                s[2] ^= self.g[2];
            }
        }
        (0..self.deg).rev().map(|i| (s[i / 64] >> (i % 64)) as u8 & 1).collect()
    }
}

// ---- LDPC: irregular repeat-accumulate encoder
fn ldpc_tables() -> &'static HashMap<String, HashMap<String, Vec<Vec<u32>>>> {
    static T: OnceLock<HashMap<String, HashMap<String, Vec<Vec<u32>>>>> = OnceLock::new();
    T.get_or_init(|| {
        let v: serde_json::Value = serde_json::from_str(LDPC_JSON).expect("dvbs2_ldpc.json");
        let mut out = HashMap::new();
        for (frame, fecs) in v.as_object().unwrap() {
            let mut m = HashMap::new();
            if let Some(fecs) = fecs.as_object() {
                for (fec, rows) in fecs {
                    let rows: Vec<Vec<u32>> = rows
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|r| r.as_array().unwrap().iter().map(|x| x.as_u64().unwrap() as u32).collect())
                        .collect();
                    m.insert(fec.clone(), rows);
                }
            }
            out.insert(frame.clone(), m);
        }
        out
    })
}

struct Ldpc {
    rows: &'static Vec<Vec<u32>>,
    k: usize,
    nk: usize,
}

impl Ldpc {
    fn new(frame: &str, fec: &str) -> Self {
        let rows = &ldpc_tables()[frame][fec];
        let k = rows.len() * 360;
        let n = if frame == "normal" { 64800 } else { 16200 };
        Ldpc { rows, k, nk: n - k }
    }

    /// k information bits -> the nk parity bits.
    fn parity(&self, bits: &[u8]) -> Vec<u8> {
        let nk = self.nk;
        let q = nk / 360;
        let mut p = vec![0u8; nk];
        for (g, row) in self.rows.iter().enumerate() {
            let bg = &bits[360 * g..360 * g + 360];
            for &x in row {
                let mut a = x as usize % nk;
                for &b in bg {
                    p[a] ^= b;
                    a += q;
                    if a >= nk {
                        a -= nk;
                    }
                }
            }
        }
        for i in 1..nk {
            p[i] ^= p[i - 1];
        }
        p
    }
}

// ---- physical layer
fn bits_of(value: u64, n: usize) -> Vec<u8> {
    (0..n).map(|i| (value >> (n - 1 - i)) as u8 & 1).collect()
}

/// The 64 PLSCODE bits (clause 5.5.2.4).
fn plscode(modcod: u32, short: bool, pilots: bool) -> Vec<u8> {
    const RM_ROWS: [u32; 6] = [0x55555555, 0x33333333, 0x0F0F0F0F, 0x00FF00FF, 0x0000FFFF, 0xFFFFFFFF];
    let pls = (modcod << 2) | if short { 2 } else { 0 } | pilots as u32;
    let b = bits_of(pls as u64, 7);
    let mut cw = 0u32;
    for i in 0..6 {
        if b[i] != 0 {
            cw ^= RM_ROWS[i];
        }
    }
    let w = bits_of(cw as u64, 32);
    let scr = bits_of(0x719D83C953422DFA, 64);
    let mut out = Vec::with_capacity(64);
    for i in 0..32 {
        out.push(w[i]);
        out.push(w[i] ^ b[6]);
    }
    out.iter().zip(scr).map(|(o, s)| o ^ s).collect()
}

/// PL scrambling sequence R(i) in 0..3 for Gold code 0 (clause 5.5.4).
fn rotation() -> &'static Vec<u8> {
    static R: OnceLock<Vec<u8>> = OnceLock::new();
    R.get_or_init(|| {
        const N: usize = (1 << 18) - 1;
        let mut x = vec![0u8; N + 20];
        let mut y = vec![0u8; N + 20];
        x[0] = 1;
        y[..18].fill(1);
        for i in 0..N + 2 - 18 {
            x[i + 18] = x[i + 7] ^ x[i];
            y[i + 18] = y[i + 10] ^ y[i + 7] ^ y[i + 5] ^ y[i];
        }
        let z = |i: usize| x[i % N] ^ y[i % N];
        (0..33282).map(|i| z(i) + 2 * z(i + 131072)).collect()
    })
}

fn apsk16_points(g: f64) -> [(f64, f64); 16] {
    let r1 = 4.0 / (4.0 + 12.0 * g * g).sqrt();
    let mut z = [(0.0, 0.0); 16];
    for v in 0..16 {
        let r = if v < 12 { g * r1 } else { r1 };
        let a = APSK16_ANGLE[v].to_radians();
        z[v] = (r * a.cos(), r * a.sin());
    }
    z
}

/// The permutation v -> v' of the 16 points for a symmetry f of the constellation.
fn apsk16_map(f: impl Fn((f64, f64)) -> (f64, f64)) -> [u8; 16] {
    let z = apsk16_points(3.15);
    let mut m = [0u8; 16];
    for v in 0..16 {
        let w = f(z[v]);
        let mut best = (f64::MAX, 0);
        for (u, p) in z.iter().enumerate() {
            let d = (p.0 - w.0).powi(2) + (p.1 - w.1).powi(2);
            if d < best.0 {
                best = (d, u);
            }
        }
        m[v] = best.1 as u8;
    }
    m
}

#[derive(Clone, Copy, PartialEq)]
enum Mod {
    Qpsk,
    Psk8,
    Apsk16,
}

pub struct Encoder {
    modulation: Mod,
    fec35: bool,
    pilots: bool,
    bits3: bool,
    swap_iq: bool,
    invert: bool,
    kbch: usize,
    dfl: usize,
    pub nsym: usize,
    bch: Bch,
    ldpc: Ldpc,
    crc: [u8; 256],
    prbs: Vec<u8>,
    crc_prev: u8,
    pend: Vec<u8>,
    stream: std::collections::VecDeque<u8>,
    pos: usize,
    /// Header symbols: QPSK as level pairs (bit 0 = I, bit 1 = Q), 8PSK as k, 16APSK as v.
    hdr: Vec<u8>,
    rot16: [u8; 16],
    conj16: [u8; 16],
    swap16: [u8; 16],
}

impl Encoder {
    pub fn new(
        fec: &str,
        frame: &str,
        pilots: bool,
        swap_iq: bool,
        invert: bool,
        modulation: &str,
        bits3: bool,
    ) -> Result<Self, String> {
        let fi = frame_info(fec, frame, pilots, modulation)?;
        let m = match modulation {
            "8psk" => Mod::Psk8,
            "16apsk" => Mod::Apsk16,
            _ => Mod::Qpsk,
        };
        let bch = Bch::new(frame, fi.t);
        let ldpc = Ldpc::new(frame, fec);
        assert_eq!(ldpc.k, fi.kbch + bch.deg);
        let mut hb = bits_of(0x18D2E82, 26); // SOF
        hb.extend(plscode(modcod(modulation, fec).unwrap(), frame == "short", pilots));
        // pi/2 BPSK: even index (1+j)/sqrt2, odd index (-1+j)/sqrt2, times (1 - 2 b)
        let hdr = hb
            .iter()
            .enumerate()
            .map(|(i, &b)| {
                let a = 1 - 2 * b as i8;
                let (hi, hq) = (if i % 2 == 0 { a } else { -a }, a);
                let k = match (hi, hq) {
                    (1, 1) => 1u8,
                    (-1, 1) => 3,
                    (-1, -1) => 5,
                    _ => 7,
                };
                match m {
                    Mod::Qpsk => (hi > 0) as u8 | ((hq > 0) as u8) << 1,
                    Mod::Psk8 => k,
                    Mod::Apsk16 => [0, 0, 0, 2, 0, 3, 0, 1][k as usize], // the outer ring point at that angle
                }
            })
            .collect();
        Ok(Encoder {
            modulation: m,
            fec35: fec == "3/5",
            pilots,
            bits3: bits3 && m == Mod::Psk8,
            swap_iq,
            invert,
            kbch: fi.kbch,
            dfl: fi.dfl,
            nsym: fi.plframe,
            bch,
            ldpc,
            crc: crc8_table(),
            prbs: bb_prbs(fi.kbch),
            crc_prev: 0,
            pend: Vec::new(),
            stream: Default::default(),
            pos: 0,
            hdr,
            rot16: apsk16_map(|z| (-z.1, z.0)),
            conj16: apsk16_map(|z| (z.0, -z.1)),
            swap16: apsk16_map(|z| (z.1, z.0)),
        })
    }

    /// Symbols still waiting for a full output byte.
    pub fn pending(&self) -> usize {
        self.pend.len()
    }

    fn push(&mut self, ts: &[u8]) {
        for row in ts.chunks_exact(188) {
            self.stream.push_back(self.crc_prev);
            self.stream.extend(&row[1..]);
            self.crc_prev = crc8(&self.crc, &row[1..]);
        }
    }

    /// The next FECFRAME as bits: BBFRAME (scrambled) + BCH parity + LDPC parity.
    fn fecframe(&mut self) -> Vec<u8> {
        let nb = self.dfl / 8;
        let first = self.pos.div_ceil(188) * 188;
        let syncd = if first < self.pos + nb { (first - self.pos) * 8 } else { 65535 };
        let mut h = vec![
            0xF0,
            0x00,
            0x05,
            0xE0,
            (self.dfl >> 8) as u8,
            self.dfl as u8,
            0x47,
            (syncd >> 8) as u8,
            syncd as u8,
        ];
        h.push(crc8(&self.crc, &h));
        h.extend(self.stream.drain(..nb));
        self.pos += nb;
        let mut bits = Vec::with_capacity(self.ldpc.k + self.ldpc.nk);
        for (i, byte) in h.iter().enumerate() {
            for b in 0..8 {
                bits.push((byte >> (7 - b) & 1) ^ self.prbs[8 * i + b]);
            }
        }
        debug_assert_eq!(bits.len(), self.kbch);
        let par = self.bch.parity(&bits);
        bits.extend(par);
        let par = self.ldpc.parity(&bits);
        bits.extend(par);
        bits
    }

    /// Data symbols with the pilot blocks (36 symbols after every 16 slots, none after the last).
    fn with_pilots(&self, sym: Vec<u8>, pilot: u8) -> Vec<u8> {
        if !self.pilots {
            return sym;
        }
        let slots = sym.len() / 90;
        let mut out = Vec::with_capacity(sym.len() + 36 * (slots / 16));
        let mut s = 0;
        while s < slots {
            let e = (s + 16).min(slots);
            out.extend_from_slice(&sym[90 * s..90 * e]);
            if e < slots {
                out.extend(std::iter::repeat(pilot).take(36));
            }
            s = e;
        }
        out
    }

    fn frame_bytes(&mut self) -> Vec<u8> {
        let cw = self.fecframe();
        let r = rotation();
        let mut s = std::mem::take(&mut self.pend);
        s.extend_from_slice(&self.hdr);
        match self.modulation {
            Mod::Qpsk => {
                // levels: bit 0 = I > 0, bit 1 = Q > 0; code bit 0 -> +1
                let d: Vec<u8> = cw.chunks_exact(2).map(|c| (1 - c[0]) | (1 - c[1]) << 1).collect();
                let d = self.with_pilots(d, 3);
                for (i, &l) in d.iter().enumerate() {
                    let (si, sq) = (l & 1, l >> 1);
                    // multiply by 1, j, -1, -j
                    let (ci, cq) = match r[i] {
                        0 => (si, sq),
                        1 => (1 - sq, si),
                        2 => (1 - si, 1 - sq),
                        _ => (sq, 1 - si),
                    };
                    s.push(ci | cq << 1);
                }
                let start = s.len() - d.len() - 90;
                for l in &mut s[start..] {
                    let (mut li, mut lq) = (*l & 1, *l >> 1);
                    if self.invert {
                        lq = 1 - lq;
                    }
                    if self.swap_iq {
                        std::mem::swap(&mut li, &mut lq);
                    }
                    *l = li | lq << 1;
                }
                let nb = s.len() / 4;
                self.pend = s[4 * nb..].to_vec();
                s.chunks_exact(4).map(|q| q[0] | q[1] << 2 | q[2] << 4 | q[3] << 6).collect()
            }
            Mod::Psk8 => {
                // bit interleaver: written by columns, read by rows (columns reversed for 3/5)
                let n = cw.len() / 3;
                let (a, c) = if self.fec35 { (2, 0) } else { (0, 2) };
                let d: Vec<u8> = (0..n)
                    .map(|j| PSK8_K[(cw[a * n + j] << 2 | cw[n + j] << 1 | cw[c * n + j]) as usize])
                    .collect();
                let d = self.with_pilots(d, 1);
                s.extend(d.iter().enumerate().map(|(i, &k)| (k + 2 * r[i]) % 8));
                let start = s.len() - d.len() - 90;
                for k in &mut s[start..] {
                    if self.invert {
                        *k = (8 - *k) % 8;
                    }
                    if self.swap_iq {
                        *k = (10 - *k) % 8;
                    }
                }
                if self.bits3 {
                    let nr = s.len() / 8;
                    self.pend = s[8 * nr..].to_vec();
                    let mut out = Vec::with_capacity(3 * nr);
                    for q in s.chunks_exact(8) {
                        let v = q.iter().enumerate().fold(0u32, |v, (i, &k)| v | (k as u32) << (3 * i));
                        out.extend_from_slice(&[v as u8, (v >> 8) as u8, (v >> 16) as u8]);
                    }
                    return out;
                }
                let nb = s.len() / 2;
                self.pend = s[2 * nb..].to_vec();
                s.chunks_exact(2).map(|q| q[0] | q[1] << 4).collect()
            }
            Mod::Apsk16 => {
                let n = cw.len() / 4;
                let d: Vec<u8> =
                    (0..n).map(|j| cw[j] << 3 | cw[n + j] << 2 | cw[2 * n + j] << 1 | cw[3 * n + j]).collect();
                let d = self.with_pilots(d, 0);
                for (i, &v) in d.iter().enumerate() {
                    let mut v = v;
                    for _ in 0..r[i] {
                        v = self.rot16[v as usize];
                    }
                    s.push(v);
                }
                let start = s.len() - d.len() - 90;
                for v in &mut s[start..] {
                    if self.invert {
                        *v = self.conj16[*v as usize];
                    }
                    if self.swap_iq {
                        *v = self.swap16[*v as usize];
                    }
                }
                let nb = s.len() / 2;
                self.pend = s[2 * nb..].to_vec();
                s.chunks_exact(2).map(|q| q[0] | q[1] << 4).collect()
            }
        }
    }

    /// Push packets (a multiple of 188 bytes) and return the packed symbols of
    /// every PLFRAME that is complete now.
    pub fn encode(&mut self, ts: &[u8]) -> Vec<u8> {
        self.push(ts);
        let mut out = Vec::new();
        while self.stream.len() >= self.dfl / 8 {
            out.extend(self.frame_bytes());
        }
        out
    }
}

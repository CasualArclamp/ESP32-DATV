//! DVB-S encoder (ETSI EN 300 421): MPEG transport stream -> packed QPSK symbols.
//!
//! A Rust port of host/dvbs.py by SP8ESA (same chain, same output bytes): energy
//! dispersal (clause 4.4.1), RS(204,188) (4.4.2), Forney interleaver I=12, M=17
//! (4.4.3), K=7 convolutional code with puncturing (4.4.4, table 2), QPSK mapping.
//! Output: 4 symbols per byte, bit 0 = I level, bit 1 = Q level (1 = +1), the
//! first symbol in the lowest bits, which is what the firmware's QPSKT reads.

pub const NULL_PACKET: [u8; 188] = {
    let mut p = [0xFFu8; 188];
    p[0] = 0x47;
    p[1] = 0x1F;
    p[2] = 0xFF;
    p[3] = 0x10;
    p
};

pub const FECS: [&str; 5] = ["1/2", "2/3", "3/4", "5/6", "7/8"];

/// Puncturing patterns (X, Y) per period of input bits, EN 300 421 table 2.
fn punct(fec: &str) -> Option<(&'static [u8], &'static [u8])> {
    Some(match fec {
        "1/2" => (&[1], &[1]),
        "2/3" => (&[1, 0], &[1, 1]),
        "3/4" => (&[1, 0, 1], &[1, 1, 0]),
        "5/6" => (&[1, 0, 1, 0, 1], &[1, 1, 0, 1, 0]),
        "7/8" => (&[1, 0, 0, 0, 1, 0, 1], &[1, 1, 1, 1, 0, 1, 0]),
        _ => return None,
    })
}

pub fn fec_rate(fec: &str) -> Option<f64> {
    let (a, b) = fec.split_once('/')?;
    Some(a.parse::<f64>().ok()? / b.parse::<f64>().ok()?)
}

/// Useful transport stream bit rate [bit/s] at a symbol rate.
pub fn ts_rate(baud: f64, fec: &str) -> Option<f64> {
    punct(fec)?;
    Some(baud * 2.0 * fec_rate(fec)? * 188.0 / 204.0)
}

/// GF(256) with x^8 + x^4 + x^3 + x^2 + 1 and the RS generator
/// g(x) = (x + a^0)..(x + a^15), highest power first.
struct Rs {
    exp: [u8; 512],
    log: [u16; 256],
    gen: [u8; 16],
}

impl Rs {
    fn new() -> Self {
        let mut exp = [0u8; 512];
        let mut log = [0u16; 256];
        let mut x = 1u16;
        for i in 0..255 {
            exp[i] = x as u8;
            log[x as usize] = i as u16;
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= 0x11D;
            }
        }
        for i in 255..510 {
            exp[i] = exp[i - 255];
        }
        let mul = |a: u8, b: u8| -> u8 {
            if a == 0 || b == 0 {
                0
            } else {
                exp[(log[a as usize] + log[b as usize]) as usize]
            }
        };
        let mut g = vec![1u8];
        for i in 0..16 {
            let r = exp[i];
            let mut ng = vec![0u8; g.len() + 1];
            for (k, &c) in g.iter().enumerate() {
                ng[k] ^= c;
                ng[k + 1] ^= mul(c, r);
            }
            g = ng;
        }
        let mut gen = [0u8; 16];
        gen.copy_from_slice(&g[1..]);
        Rs { exp, log, gen }
    }

    fn parity(&self, data: &[u8]) -> [u8; 16] {
        let mut r = [0u8; 16];
        for &d in data {
            let fb = d ^ r[0];
            r.copy_within(1.., 0);
            r[15] = 0;
            if fb != 0 {
                let lf = self.log[fb as usize];
                for j in 0..16 {
                    if self.gen[j] != 0 {
                        r[j] ^= self.exp[(lf + self.log[self.gen[j] as usize]) as usize];
                    }
                }
            }
        }
        r
    }
}

/// 1503 PRBS bytes: 1 + x^14 + x^15, initial 100101010000000.
fn prbs_bytes() -> Vec<u8> {
    let mut reg = [1u8, 0, 0, 1, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0];
    let mut out = vec![0u8; 1503];
    for i in 0..1503 * 8 {
        let o = reg[13] ^ reg[14];
        reg.copy_within(0..14, 1);
        reg[0] = o;
        out[i / 8] |= o << (7 - i % 8);
    }
    out
}

pub struct Encoder {
    xp: &'static [u8],
    yp: &'static [u8],
    swap_iq: bool,
    invert: bool,
    rs: Rs,
    prbs: Vec<u8>,
    pkt: usize,
    hist: Vec<u8>,
    sr: u8,
    cpos: usize,
    pend_bit: Option<u8>,
    pend_sym: Vec<u8>,
}

impl Encoder {
    pub fn new(fec: &str, swap_iq: bool, invert: bool) -> Result<Self, String> {
        let (xp, yp) = punct(fec).ok_or_else(|| format!("DVB-S code rate: {}", FECS.join(", ")))?;
        Ok(Encoder {
            xp,
            yp,
            swap_iq,
            invert,
            rs: Rs::new(),
            prbs: prbs_bytes(),
            pkt: 0,
            hist: vec![0; 204 * 11],
            sr: 0,
            cpos: 0,
            pend_bit: None,
            pend_sym: Vec::new(),
        })
    }

    /// ts: a multiple of 188 bytes, every packet starting with 0x47.
    pub fn encode(&mut self, ts: &[u8]) -> Vec<u8> {
        let n = ts.len() / 188;
        let h = self.hist.len();
        let mut x = std::mem::take(&mut self.hist);
        x.reserve(n * 204);
        for k in 0..n {
            let mut p = [0u8; 188];
            p.copy_from_slice(&ts[188 * k..188 * k + 188]);
            let g = self.pkt;
            for i in 1..188 {
                p[i] ^= self.prbs[188 * g + i - 1];
            }
            if g == 0 {
                p[0] = 0xB8;
            }
            self.pkt = (self.pkt + 1) % 8;
            x.extend_from_slice(&p);
            x.extend_from_slice(&self.rs.parity(&p));
        }
        let len = n * 204;
        let mut out = Vec::with_capacity(len / 2 + 4);
        let p = self.xp.len();
        let emit = |bit: u8, s: &mut Self, out: &mut Vec<u8>| match s.pend_bit.take() {
            None => s.pend_bit = Some(bit),
            Some(xi) => {
                let (mut li, mut lq) = (1 - xi, 1 - bit);
                if s.invert {
                    lq = 1 - lq;
                }
                if s.swap_iq {
                    std::mem::swap(&mut li, &mut lq);
                }
                s.pend_sym.push(li | lq << 1);
                if s.pend_sym.len() == 4 {
                    let q = &s.pend_sym;
                    out.push(q[0] | q[1] << 2 | q[2] << 4 | q[3] << 6);
                    s.pend_sym.clear();
                }
            }
        };
        for t in 0..len {
            // Forney interleaver: out(t) = in(t - 204 * (t mod 12))
            let byte = x[h + t - 204 * (t % 12)];
            for b in (0..8).rev() {
                self.sr = ((self.sr << 1) | (byte >> b & 1)) & 0x7F;
                let xb = (self.sr & 0x4F).count_ones() as u8 & 1; // 171: delays 0 1 2 3 6
                let yb = (self.sr & 0x6D).count_ones() as u8 & 1; // 133: delays 0 2 3 5 6
                let (kx, ky) = (self.xp[self.cpos], self.yp[self.cpos]);
                self.cpos = (self.cpos + 1) % p;
                if kx == 1 {
                    emit(xb, self, &mut out);
                }
                if ky == 1 {
                    emit(yb, self, &mut out);
                }
            }
        }
        self.hist = x[x.len() - h..].to_vec();
        out
    }
}

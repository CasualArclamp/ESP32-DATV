//! Makes FFmpeg's AV1 transport stream follow the AOM draft "Carriage of AV1 in MPEG-2 TS"
//! (working group draft of 25 March 2026, https://aomediacodec.github.io/av1-mpeg2-ts/).
//!
//! FFmpeg's MPEG-TS muxer writes AV1 as stream_type 0x06 with no descriptors and raw
//! low-overhead OBUs in the PES packets, which no receiver can identify. This filter
//! rewrites the stream packet by packet:
//!  - PMT: a registration descriptor 'AV01' and the AV1 video descriptor (tag 0x80)
//!    are added to the video stream (draft sections 2.1 and 2.2);
//!  - video PES: stream_id 0xBD, data_alignment_indicator set, every OBU preceded by
//!    the start code 00 00 01 with emulation prevention bytes inside it, temporal
//!    delimiters removed (section 3.2); one PES packet per access unit;
//!  - key frames get random_access_indicator in the first packet (section 3.3).
//! PCRs of the video PID stay at their place in adaptation-only packets, so the clock
//! is not disturbed by the video packets moving (an access unit is sent when it is
//! complete, one frame later than FFmpeg wrote it).

pub type Packet = [u8; 188];

/// MPEG-2 CRC-32 (ISO/IEC 13818-1 annex A): polynomial 0x04C11DB7, initial value all ones.
pub fn crc32_mpeg(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c ^= (b as u32) << 24;
        for _ in 0..8 {
            c = if c & 0x8000_0000 != 0 { (c << 1) ^ 0x04C1_1DB7 } else { c << 1 };
        }
    }
    c
}

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn f(&mut self, n: usize) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.d.get(self.pos / 8)?;
            v = v << 1 | (byte >> (7 - self.pos % 8) & 1) as u32;
            self.pos += 1;
        }
        Some(v)
    }

    fn uvlc(&mut self) -> Option<()> {
        let mut zeros = 0;
        while self.f(1)? == 0 {
            zeros += 1;
            if zeros > 32 {
                return None;
            }
        }
        self.f(zeros.min(32))?;
        Some(())
    }
}

/// The four bytes of the AV1 video descriptor from a sequence header OBU payload
/// (AV1 specification 5.5: sequence_header_obu, color_config).
pub fn video_descriptor(seq: &[u8]) -> Option<[u8; 4]> {
    let mut b = Bits { d: seq, pos: 0 };
    let profile = b.f(3)?;
    b.f(1)?; // still_picture
    let reduced = b.f(1)? == 1;
    let (mut level, mut tier) = (0, 0);

    if reduced {
        level = b.f(5)?;
    } else {
        let mut decoder_model = false;
        let mut delay_len = 0;
        if b.f(1)? == 1 {
            b.f(32)?; // num_units_in_display_tick
            b.f(32)?; // time_scale
            if b.f(1)? == 1 {
                b.uvlc()?; // num_ticks_per_picture_minus_1
            }
            decoder_model = b.f(1)? == 1;
            if decoder_model {
                delay_len = b.f(5)? as usize + 1;
                b.f(32)?; // num_units_in_decoding_tick
                b.f(10)?; // buffer_removal_time_length_minus_1, frame_presentation_time_length_minus_1
            }
        }
        let delay_present = b.f(1)? == 1;
        let ops = b.f(5)? + 1;
        for i in 0..ops {
            b.f(12)?; // operating_point_idc
            let l = b.f(5)?;
            let t = if l > 7 { b.f(1)? } else { 0 };
            if i == 0 {
                (level, tier) = (l, t);
            }
            if decoder_model && b.f(1)? == 1 {
                b.f(delay_len)?;
                b.f(delay_len)?;
                b.f(1)?;
            }
            if delay_present && b.f(1)? == 1 {
                b.f(4)?;
            }
        }
    }
    let wbits = b.f(4)? as usize + 1;
    let hbits = b.f(4)? as usize + 1;
    b.f(wbits)?;
    b.f(hbits)?;
    if !reduced && b.f(1)? == 1 {
        b.f(7)?; // delta_frame_id_length_minus_2, additional_frame_id_length_minus_1
    }
    b.f(3)?; // use_128x128_superblock, enable_filter_intra, enable_intra_edge_filter
    if !reduced {
        b.f(4)?; // interintra, masked compound, warped motion, dual filter
        let order_hint = b.f(1)? == 1;
        if order_hint {
            b.f(2)?; // jnt_comp, ref_frame_mvs
        }
        let force_sct = if b.f(1)? == 1 { 2 } else { b.f(1)? };
        if force_sct > 0 && b.f(1)? == 0 {
            b.f(1)?; // seq_force_integer_mv
        }
        if order_hint {
            b.f(3)?;
        }
    }
    b.f(3)?; // superres, cdef, restoration
    let high = b.f(1)?;
    let twelve = if profile == 2 && high == 1 { b.f(1)? } else { 0 };
    let mono = if profile == 1 { 0 } else { b.f(1)? };
    let (mut cp, mut tc, mut mc) = (2, 2, 2);
    if b.f(1)? == 1 {
        cp = b.f(8)?;
        tc = b.f(8)?;
        mc = b.f(8)?;
    }
    let (ssx, ssy, mut csp) = if mono == 1 {
        (1, 1, 0)
    } else if cp == 1 && tc == 13 && mc == 0 {
        (0, 0, 0)
    } else {
        b.f(1)?; // color_range
        match profile {
            0 => (1, 1, 0),
            1 => (0, 0, 0),
            _ if twelve == 1 => {
                let x = b.f(1)?;
                let y = if x == 1 { b.f(1)? } else { 0 };
                (x, y, 0)
            }
            _ => (1, 0, 0),
        }
    };
    if mono == 0 && ssx == 1 && ssy == 1 && !(cp == 1 && tc == 13 && mc == 0) {
        csp = b.f(2)?;
    }
    Some([
        0x81, // marker, version 1
        (profile << 5 | level) as u8,
        (tier << 7 | high << 6 | twelve << 5 | mono << 4 | ssx << 3 | ssy << 2 | csp) as u8,
        0x00, // hdr_wcg_idc 0 (SDR), no initial presentation delay
    ])
}

/// Main profile, level 2.0, 8 bit 4:2:0: used only if the sequence header cannot be read.
const DEFAULT_DESCRIPTOR: [u8; 4] = [0x81, 0x00, 0x0C, 0x00];

fn leb128(d: &[u8]) -> Option<(usize, usize)> {
    let mut v = 0usize;
    for i in 0..8 {
        let b = *d.get(i)?;
        v |= ((b & 0x7F) as usize) << (7 * i);
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

/// Low-overhead OBUs -> start code format with emulation prevention; temporal delimiters
/// dropped. Returns the payload, whether a sequence header was in it, and its descriptor.
fn convert_obus(p: &[u8]) -> (Vec<u8>, bool, Option<[u8; 4]>) {
    let mut out = Vec::with_capacity(p.len() + p.len() / 64 + 16);
    let (mut key, mut desc) = (false, None);
    let mut i = 0;
    while i < p.len() {
        let hdr = p[i];
        let ty = hdr >> 3 & 0xF;
        let ext = (hdr >> 2 & 1) as usize;
        let (payload, total) = if hdr & 2 != 0 {
            match leb128(&p[(i + 1 + ext).min(p.len())..]) {
                Some((size, n)) if i + 1 + ext + n + size <= p.len() => (i + 1 + ext + n, 1 + ext + n + size),
                _ => (p.len(), p.len() - i), // damaged: keep the rest as it is
            }
        } else {
            ((i + 1 + ext).min(p.len()), p.len() - i)
        };
        let obu = &p[i..i + total];
        i += total;
        if ty == 2 {
            continue; // temporal delimiter
        }
        if ty == 1 {
            key = true;
            desc = video_descriptor(&obu[payload - (i - total)..]);
        }
        out.extend_from_slice(&[0, 0, 1]);
        let mut zeros = 0;
        for &b in obu {
            if zeros >= 2 && b <= 3 {
                out.push(3);
                zeros = 0;
            }
            out.push(b);
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
    }
    (out, key, desc)
}

#[derive(Default)]
pub struct Av1Fix {
    pmt_pid: Option<u16>,
    video_pid: Option<u16>,
    pes: Vec<u8>,
    cc: u8,
    desc: Option<[u8; 4]>,
    /// Packets waiting for the first sequence header (the PMT needs its values).
    held: Vec<Packet>,
    /// Packets sent beyond what came in.
    extra: i64,
}

fn pid_of(p: &Packet) -> u16 {
    ((p[1] & 0x1F) as u16) << 8 | p[2] as u16
}

/// Offset of the payload in a packet, None if it has none.
fn payload_at(p: &Packet) -> Option<usize> {
    match p[3] >> 4 & 3 {
        1 => Some(4),
        3 if (p[4] as usize) < 183 => Some(5 + p[4] as usize),
        _ => None,
    }
}

impl Av1Fix {
    pub fn new() -> Self {
        Self::default()
    }

    /// One packet in, zero or more packets out. The rewritten video takes a few packets
    /// more than FFmpeg wrote; as many of its null packets are left out, so the rate of the
    /// multiplex stays what FFmpeg was told.
    pub fn push(&mut self, p: &Packet, out: &mut Vec<Packet>) {
        let pid = pid_of(p);
        if pid == 0x1FFF {
            if self.extra > 0 {
                self.extra -= 1;
            } else {
                self.route(*p, out);
            }
            return;
        }
        let before = out.len() + self.held.len();
        self.push_inner(p, out);
        self.extra += (out.len() + self.held.len()) as i64 - before as i64 - 1;
    }

    fn push_inner(&mut self, p: &Packet, out: &mut Vec<Packet>) {
        let pid = pid_of(p);
        if pid == 0 && p[1] & 0x40 != 0 {
            if let Some(at) = payload_at(p) {
                let s = at + 1 + p[at] as usize; // after the pointer field
                if s + 12 <= 188 {
                    let end = (s + 3 + (((p[s + 1] & 0xF) as usize) << 8 | p[s + 2] as usize)).saturating_sub(4).min(188);
                    let mut i = s + 8;
                    while i + 4 <= end {
                        if p[i] != 0 || p[i + 1] != 0 {
                            self.pmt_pid = Some(((p[i + 2] & 0x1F) as u16) << 8 | p[i + 3] as u16);
                            break;
                        }
                        i += 4;
                    }
                }
            }
        }
        if self.video_pid.is_none() && Some(pid) == self.pmt_pid {
            self.find_video(p);
        }
        if Some(pid) != self.video_pid {
            self.route(*p, out);
            return;
        }
        // video: keep the PCR where it is, collect the PES packet
        let has_af = p[3] & 0x20 != 0;
        if has_af && p[4] >= 7 && p[5] & 0x10 != 0 {
            let mut q = [0xFFu8; 188];
            q[0] = 0x47;
            q[1] = p[1] & 0x1F;
            q[2] = p[2];
            q[3] = 0x20 | self.cc; // adaptation field only: the counter does not advance
            q[4] = 183;
            q[5] = 0x10;
            q[6..12].copy_from_slice(&p[6..12]);
            self.route(q, out);
        }
        if p[1] & 0x40 != 0 {
            self.flush(out);
        }
        if let Some(at) = payload_at(p) {
            if p[1] & 0x40 != 0 || !self.pes.is_empty() {
                self.pes.extend_from_slice(&p[at..]);
            }
        }
    }

    /// Sends a packet that is not video data, the PMT with its descriptors added.
    fn route(&mut self, p: Packet, out: &mut Vec<Packet>) {
        let Some(desc) = self.desc else {
            self.held.push(p);
            if self.held.len() > 4000 {
                out.append(&mut self.held); // no sequence header yet: do not hold the stream back for ever
            }
            return;
        };
        if Some(pid_of(&p)) == self.pmt_pid {
            out.push(self.rewrite_pmt(&p, &desc).unwrap_or(p));
        } else {
            out.push(p);
        }
    }

    /// The elementary streams of a PMT: (offset of the entry, stream_type, pid, ES_info_length).
    fn pmt_streams(p: &Packet) -> Option<(usize, usize, Vec<(usize, u8, u16, usize)>)> {
        if p[1] & 0x40 == 0 {
            return None;
        }
        let at = payload_at(p)?;
        let s = at + 1 + *p.get(at)? as usize;
        if *p.get(s)? != 0x02 || s + 12 > 188 {
            return None;
        }
        let end = s + 3 + (((p[s + 1] & 0xF) as usize) << 8 | p[s + 2] as usize) - 4; // start of the CRC
        if end + 4 > 188 {
            return None;
        }
        let mut i = s + 12 + (((p[s + 10] & 0xF) as usize) << 8 | p[s + 11] as usize);
        let mut v = Vec::new();
        while i + 5 <= end {
            let len = ((p[i + 3] & 0xF) as usize) << 8 | p[i + 4] as usize;
            v.push((i, p[i], ((p[i + 1] & 0x1F) as u16) << 8 | p[i + 2] as u16, len));
            i += 5 + len;
        }
        Some((s, end, v))
    }

    fn find_video(&mut self, p: &Packet) {
        if let Some((_, _, streams)) = Self::pmt_streams(p) {
            self.video_pid = streams.iter().find(|e| e.1 == 0x06).map(|e| e.2);
        }
    }

    fn rewrite_pmt(&self, p: &Packet, desc: &[u8; 4]) -> Option<Packet> {
        let (s, _, streams) = Self::pmt_streams(p)?;
        let first = streams.first()?.0;
        let mut sec = p[s..first].to_vec();
        for (at, ty, pid, len) in streams {
            if Some(pid) == self.video_pid {
                sec.extend_from_slice(&[ty, p[at + 1], p[at + 2], 0xF0, 12]);
                sec.extend_from_slice(&[0x05, 4, b'A', b'V', b'0', b'1', 0x80, 4]);
                sec.extend_from_slice(desc);
            } else {
                sec.extend_from_slice(&p[at..at + 5 + len]);
            }
        }
        let len = sec.len() - 3 + 4;
        sec[1] = (sec[1] & 0xF0) | (len >> 8) as u8;
        sec[2] = len as u8;
        let crc = crc32_mpeg(&sec);
        sec.extend_from_slice(&crc.to_be_bytes());
        if 5 + sec.len() > 188 {
            return None;
        }
        let mut q = [0xFFu8; 188];
        q[..4].copy_from_slice(&p[..4]);
        q[3] = 0x10 | (p[3] & 0x0F);
        q[4] = 0;
        q[5..5 + sec.len()].copy_from_slice(&sec);
        Some(q)
    }

    /// Converts the collected PES packet and sends it.
    fn flush(&mut self, out: &mut Vec<Packet>) {
        let pes = std::mem::take(&mut self.pes);
        if pes.len() < 9 || pes[..3] != [0, 0, 1] {
            return;
        }
        let hlen = 9 + pes[8] as usize;
        if hlen > pes.len() {
            return;
        }
        let (payload, key, desc) = convert_obus(&pes[hlen..]);
        if key && (desc.is_some() || self.desc.is_none()) {
            self.desc = Some(desc.unwrap_or(DEFAULT_DESCRIPTOR));
        }
        if self.desc.is_none() {
            return; // nothing decodable before the first sequence header
        }
        let held = std::mem::take(&mut self.held);
        for h in held {
            self.route(h, out);
        }
        let n = hlen - 6 + payload.len();
        let mut d = Vec::with_capacity(hlen + payload.len());
        d.extend_from_slice(&[0, 0, 1, 0xBD]);
        d.extend_from_slice(&if n <= 0xFFFF { (n as u16).to_be_bytes() } else { [0, 0] });
        d.push(pes[6] | 0x04); // data_alignment_indicator
        d.extend_from_slice(&pes[7..hlen]);
        d.extend_from_slice(&payload);
        let pid = self.video_pid.unwrap_or(0x100);
        let mut i = 0;
        while i < d.len() {
            let first = i == 0;
            let left = d.len() - i;
            let mut q = [0xFFu8; 188];
            q[0] = 0x47;
            q[1] = (pid >> 8) as u8 & 0x1F | if first { 0x40 } else { 0 };
            q[2] = pid as u8;
            self.cc = (self.cc + 1) & 0xF;
            let flags = if first && key { 0x60 } else { 0x00 }; // random access, ES priority
            let at = if flags != 0 || left < 184 {
                q[3] = 0x30 | self.cc;
                if flags == 0 && left == 183 {
                    q[4] = 0;
                    5
                } else {
                    let room = 182usize;
                    let take = left.min(room);
                    q[4] = (183 - take) as u8;
                    q[5] = flags;
                    188 - take
                }
            } else {
                q[3] = 0x10 | self.cc;
                4
            };
            let take = 188 - at;
            q[at..].copy_from_slice(&d[i..i + take]);
            i += take;
            out.push(q);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section_packet(pid: u16, cc: u8, sec: &[u8]) -> Packet {
        let mut q = [0xFFu8; 188];
        q[..5].copy_from_slice(&[0x47, 0x40 | (pid >> 8) as u8, pid as u8, 0x10 | cc, 0]);
        q[5..5 + sec.len()].copy_from_slice(sec);
        let crc = crc32_mpeg(sec);
        q[5 + sec.len()..9 + sec.len()].copy_from_slice(&crc.to_be_bytes());
        q
    }

    /// PES packet -> TS packets the way FFmpeg writes them (PCR in the first one).
    fn pes_packets(pid: u16, cc: &mut u8, pes: &[u8]) -> Vec<Packet> {
        let mut v = Vec::new();
        let mut i = 0;
        while i < pes.len() {
            let mut q = [0xFFu8; 188];
            q[0] = 0x47;
            q[1] = (pid >> 8) as u8 | if i == 0 { 0x40 } else { 0 };
            q[2] = pid as u8;
            let left = pes.len() - i;
            let at = if i == 0 {
                let take = left.min(176);
                q[3] = 0x30 | *cc;
                q[4] = (183 - take) as u8;
                q[5] = 0x10;
                q[6..12].copy_from_slice(&[1, 2, 3, 4, 0x7E, 5]);
                188 - take
            } else if left < 184 {
                q[3] = 0x30 | *cc;
                q[4] = (183 - left) as u8;
                if left < 183 {
                    q[5] = 0;
                }
                188 - left
            } else {
                q[3] = 0x10 | *cc;
                4
            };
            *cc = (*cc + 1) & 15;
            q[at..].copy_from_slice(&pes[i..i + 188 - at]);
            i += 188 - at;
            v.push(q);
        }
        v
    }

    fn obu(ty: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![ty << 3 | 2];
        let mut n = payload.len();
        loop {
            let b = (n & 0x7F) as u8;
            n >>= 7;
            v.push(if n > 0 { b | 0x80 } else { b });
            if n == 0 {
                break;
            }
        }
        v.extend_from_slice(payload);
        v
    }

    /// The sequence header of libsvtav1 for 640x360 (from an FFmpeg test stream).
    const SEQ: [u8; 12] = [0x02, 0x00, 0x00, 0x0D, 0x26, 0x27, 0xFB, 0x38, 0xD5, 0xF3, 0x00, 0x80];

    #[test]
    fn descriptor_from_sequence_header() {
        // main profile, 8 bit, 4:2:0
        let d = video_descriptor(&SEQ).unwrap();
        assert_eq!(d[0], 0x81);
        assert_eq!(d[1] >> 5, 0);
        assert_eq!(d[2] & 0x7C, 0x0C);
    }

    #[test]
    fn rewrites_stream() {
        let pat = section_packet(0, 0, &[0x00, 0xB0, 0x0D, 0, 1, 0xC1, 0, 0, 0, 1, 0xF0, 0x00]);
        let pmt = section_packet(
            0x1000,
            0,
            &[0x02, 0xB0, 0x17, 0, 1, 0xC1, 0, 0, 0xE1, 0x00, 0xF0, 0, 0x06, 0xE1, 0x00, 0xF0, 0, 0x03, 0xE1, 0x01, 0xF0, 0],
        );
        // frames with zero runs, long enough for several packets
        let frame: Vec<u8> = (0..700u32).map(|i| if i % 7 < 3 { 0 } else { (i % 4) as u8 }).collect();
        let hdr = [0, 0, 1, 0xE0, 0, 0, 0x80, 0x80, 5, 0x21, 0, 7, 0xDF, 0x6D];
        let mut au1 = hdr.to_vec();
        au1.extend(obu(2, &[]));
        au1.extend(obu(1, &SEQ));
        au1.extend(obu(6, &frame));
        let mut au2 = hdr.to_vec();
        au2.extend(obu(2, &[]));
        au2.extend(obu(6, &frame[..183]));
        let mut au3 = hdr.to_vec();
        au3.extend(obu(6, &[9]));

        let mut input = vec![pat, pmt];
        let mut cc = 0;
        for au in [&au1, &au2, &au3] {
            input.extend(pes_packets(0x100, &mut cc, au));
        }
        let mut fix = Av1Fix::new();
        let mut out = Vec::new();
        for p in &input {
            fix.push(p, &mut out);
        }

        // PMT: descriptors and a valid CRC
        let pmt = out.iter().find(|p| pid_of(p) == 0x1000).unwrap();
        let len = ((pmt[6] & 0xF) as usize) << 8 | pmt[7] as usize;
        assert_eq!(crc32_mpeg(&pmt[5..5 + 3 + len]), 0);
        assert_eq!(&pmt[17..22], &[0x06, 0xE1, 0x00, 0xF0, 12]);
        assert_eq!(&pmt[22..30], &[0x05, 4, b'A', b'V', b'0', b'1', 0x80, 4]);
        assert_eq!(&pmt[34..39], &[0x03, 0xE1, 0x01, 0xF0, 0]);

        // video: two access units out (the third waits for the next one), counters in order
        let mut pes: Vec<Vec<u8>> = Vec::new();
        let mut last_cc = None;
        let mut pcrs = 0;
        for p in out.iter().filter(|p| pid_of(p) == 0x100) {
            let Some(at) = payload_at(p) else {
                assert_eq!(&p[6..12], &[1, 2, 3, 4, 0x7E, 5]);
                pcrs += 1;
                continue;
            };
            if let Some(l) = last_cc {
                assert_eq!(p[3] & 15, (l + 1) & 15);
            }
            last_cc = Some(p[3] & 15);
            if p[1] & 0x40 != 0 {
                pes.push(Vec::new());
            }
            pes.last_mut().unwrap().extend_from_slice(&p[at..]);
        }
        assert_eq!(pcrs, 3);
        assert_eq!(pes.len(), 2);
        let first = out.iter().find(|p| pid_of(p) == 0x100 && p[1] & 0x40 != 0).unwrap();
        assert_eq!(first[5] & 0x40, 0x40, "random access indicator on the key frame");

        for (got, src) in pes.iter().zip([&au1, &au2]) {
            assert_eq!(&got[..4], &[0, 0, 1, 0xBD]);
            assert_eq!(u16::from_be_bytes([got[4], got[5]]) as usize, got.len() - 6);
            assert_eq!(got[6] & 0x04, 0x04);
            assert_eq!(&got[7..14], &src[7..14]);
            // no start code emulation inside the OBUs: split, unescape, compare
            let body = &got[14..];
            let mut obus = Vec::new();
            let mut i = 0;
            let mut cur: Option<Vec<u8>> = None;
            while i < body.len() {
                if body[i..].starts_with(&[0, 0, 1]) {
                    obus.extend(cur.take());
                    cur = Some(Vec::new());
                    i += 3;
                } else if body[i..].starts_with(&[0, 0, 3]) {
                    cur.as_mut().unwrap().extend_from_slice(&[0, 0]);
                    i += 3;
                } else {
                    assert!(!body[i..].starts_with(&[0, 0, 0]) && !body[i..].starts_with(&[0, 0, 2]));
                    cur.as_mut().unwrap().push(body[i]);
                    i += 1;
                }
            }
            obus.extend(cur);
            let want: Vec<u8> = src[14 + 2..].to_vec(); // without the temporal delimiter
            assert_eq!(obus.concat(), want);
        }
    }
}

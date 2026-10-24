//! Minimal WebM (Matroska) demuxer for a single Opus audio track.
//!
//! Written for exactly what gapless Opus needs and symphonia's mkv reader doesn't give:
//! `CodecPrivate` (OpusHead, for pre-skip and gain), `DiscardPadding` on the final block (the end
//! trim), and `Cues` for fast seeks.

use crate::{Error, Result};
use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};

mod id {
    pub const EBML: u32 = 0x1A45DFA3;
    pub const SEGMENT: u32 = 0x18538067;
    pub const SEEK_HEAD: u32 = 0x114D9B74;
    pub const SEEK: u32 = 0x4DBB;
    pub const SEEK_ID: u32 = 0x53AB;
    pub const SEEK_POSITION: u32 = 0x53AC;
    pub const INFO: u32 = 0x1549A966;
    pub const TIMECODE_SCALE: u32 = 0x2AD7B1;
    pub const DURATION: u32 = 0x4489;
    pub const TRACKS: u32 = 0x1654AE6B;
    pub const TRACK_ENTRY: u32 = 0xAE;
    pub const TRACK_NUMBER: u32 = 0xD7;
    pub const TRACK_TYPE: u32 = 0x83;
    pub const CODEC_ID: u32 = 0x86;
    pub const CODEC_PRIVATE: u32 = 0x63A2;
    pub const AUDIO: u32 = 0xE1;
    pub const SAMPLING_FREQUENCY: u32 = 0xB5;
    pub const CHANNELS: u32 = 0x9F;
    pub const CUES: u32 = 0x1C53BB6B;
    pub const CUE_POINT: u32 = 0xBB;
    pub const CUE_TIME: u32 = 0xB3;
    pub const CUE_TRACK_POSITIONS: u32 = 0xB7;
    pub const CUE_CLUSTER_POSITION: u32 = 0xF1;
    pub const CLUSTER: u32 = 0x1F43B675;
    pub const TIMECODE: u32 = 0xE7;
    pub const SIMPLE_BLOCK: u32 = 0xA3;
    pub const BLOCK_GROUP: u32 = 0xA0;
    pub const BLOCK: u32 = 0xA1;
    pub const DISCARD_PADDING: u32 = 0x75A2;
}

const UNKNOWN_SIZE: u64 = u64::MAX;
/// Refuse absurd element sizes instead of allocating them.
const MAX_ELEMENT: u64 = 16 << 20;

#[derive(Debug, Clone)]
pub struct OpusTrack {
    pub channels: usize,
    /// Samples (at 48 kHz) to drop at the very start.
    pub pre_skip: u64,
    /// OpusHead output gain, linear.
    pub gain: f32,
    pub sample_rate: f64,
}

#[derive(Debug)]
pub struct Packet {
    pub data: Vec<u8>,
    /// Absolute timestamp in nanoseconds.
    pub ts_ns: u64,
    /// Nanoseconds of padding to drop from the end of this packet's decoded audio.
    pub discard_padding_ns: u64,
}

pub struct WebmDemuxer<R> {
    r: R,
    pub track: OpusTrack,
    /// Segment duration in nanoseconds, if the file says.
    pub duration_ns: Option<u64>,
    track_number: u64,
    timecode_scale: u64,
    segment_start: u64,
    segment_end: u64,
    first_cluster: u64,
    /// (time in ns, absolute byte position of the cluster)
    cues: Vec<(u64, u64)>,
    cluster_tc: u64,
    queue: VecDeque<Packet>,
}

impl<R: Read + Seek> WebmDemuxer<R> {
    pub fn open(mut r: R) -> Result<Self> {
        let (eid, size) = read_header(&mut r)?;
        if eid != id::EBML {
            return Err(Error::Media("not a WebM file".into()));
        }
        skip(&mut r, size)?;
        let (eid, size) = read_header(&mut r)?;
        if eid != id::SEGMENT {
            return Err(Error::Media("no Segment".into()));
        }
        let segment_start = pos(&mut r)?;
        let segment_end = if size == UNKNOWN_SIZE {
            u64::MAX
        } else {
            segment_start + size
        };

        let mut d = WebmDemuxer {
            r,
            track: OpusTrack {
                channels: 2,
                pre_skip: 0,
                gain: 1.0,
                sample_rate: 48000.0,
            },
            duration_ns: None,
            track_number: 0,
            timecode_scale: 1_000_000,
            segment_start,
            segment_end,
            first_cluster: 0,
            cues: Vec::new(),
            cluster_tc: 0,
            queue: VecDeque::new(),
        };
        let mut cues_at = None;
        let mut duration = None;
        let mut have_track = false;
        // Walk the top level until the first cluster.
        loop {
            let at = pos(&mut d.r)?;
            if at >= d.segment_end {
                return Err(Error::Media("no clusters".into()));
            }
            let (eid, size) = read_header(&mut d.r)?;
            match eid {
                id::CLUSTER => {
                    d.first_cluster = at;
                    break;
                }
                id::SEEK_HEAD => {
                    let body = read_body(&mut d.r, size)?;
                    for (sid, seek) in children(&body)? {
                        if sid != id::SEEK {
                            continue;
                        }
                        let mut target = 0;
                        let mut p = None;
                        for (k, v) in children(seek)? {
                            match k {
                                id::SEEK_ID => target = uint(v) as u32,
                                id::SEEK_POSITION => p = Some(uint(v)),
                                _ => {}
                            }
                        }
                        if target == id::CUES {
                            cues_at = p.map(|p| segment_start + p);
                        }
                    }
                }
                id::INFO => {
                    let body = read_body(&mut d.r, size)?;
                    for (k, v) in children(&body)? {
                        match k {
                            id::TIMECODE_SCALE => d.timecode_scale = uint(v).max(1),
                            id::DURATION => duration = Some(float(v)),
                            _ => {}
                        }
                    }
                }
                id::TRACKS => {
                    let body = read_body(&mut d.r, size)?;
                    have_track = d.parse_tracks(&body)?;
                }
                id::CUES => {
                    let body = read_body(&mut d.r, size)?;
                    d.parse_cues(&body)?;
                }
                _ => skip(&mut d.r, size)?,
            }
        }
        if !have_track {
            return Err(Error::Media("no Opus track".into()));
        }
        d.duration_ns = duration.map(|t| (t * d.timecode_scale as f64) as u64);
        // Cues after the clusters (non-DASH files): read them via the SeekHead pointer.
        if d.cues.is_empty()
            && let Some(p) = cues_at
        {
            d.r.seek(SeekFrom::Start(p)).map_err(io)?;
            let (eid, size) = read_header(&mut d.r)?;
            if eid == id::CUES {
                let body = read_body(&mut d.r, size)?;
                d.parse_cues(&body)?;
            }
        }
        d.r.seek(SeekFrom::Start(d.first_cluster)).map_err(io)?;
        Ok(d)
    }

    fn parse_tracks(&mut self, body: &[u8]) -> Result<bool> {
        for (k, entry) in children(body)? {
            if k != id::TRACK_ENTRY {
                continue;
            }
            let (mut num, mut kind, mut codec, mut private) = (0, 0, String::new(), None);
            let (mut rate, mut ch) = (48000.0, 0usize);
            for (k, v) in children(entry)? {
                match k {
                    id::TRACK_NUMBER => num = uint(v),
                    id::TRACK_TYPE => kind = uint(v),
                    id::CODEC_ID => {
                        codec = String::from_utf8_lossy(v).trim_end_matches('\0').into()
                    }
                    id::CODEC_PRIVATE => private = Some(v.to_vec()),
                    id::AUDIO => {
                        for (k, v) in children(v)? {
                            match k {
                                id::SAMPLING_FREQUENCY => rate = float(v),
                                id::CHANNELS => ch = uint(v) as usize,
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            if kind != 2 || codec != "A_OPUS" {
                continue;
            }
            let head = private.ok_or_else(|| Error::Media("Opus track without OpusHead".into()))?;
            if head.len() < 19 || &head[..8] != b"OpusHead" {
                return Err(Error::Media("bad OpusHead".into()));
            }
            let channels = head[9] as usize;
            if head[18] != 0 || channels == 0 || channels > 2 {
                return Err(Error::Media(format!(
                    "unsupported Opus layout ({channels} ch)"
                )));
            }
            let _ = ch;
            let gain_q8 = i16::from_le_bytes([head[16], head[17]]);
            self.track = OpusTrack {
                channels,
                pre_skip: u16::from_le_bytes([head[10], head[11]]) as u64,
                gain: 10f32.powf(gain_q8 as f32 / (20.0 * 256.0)),
                sample_rate: rate,
            };
            self.track_number = num;
            return Ok(true);
        }
        Ok(false)
    }

    fn parse_cues(&mut self, body: &[u8]) -> Result<()> {
        for (k, point) in children(body)? {
            if k != id::CUE_POINT {
                continue;
            }
            let (mut t, mut p) = (None, None);
            for (k, v) in children(point)? {
                match k {
                    id::CUE_TIME => t = Some(uint(v)),
                    id::CUE_TRACK_POSITIONS => {
                        for (k, v) in children(v)? {
                            if k == id::CUE_CLUSTER_POSITION {
                                p = Some(uint(v));
                            }
                        }
                    }
                    _ => {}
                }
            }
            if let (Some(t), Some(p)) = (t, p) {
                self.cues
                    .push((t * self.timecode_scale, self.segment_start + p));
            }
        }
        self.cues.sort_unstable();
        Ok(())
    }

    /// Next audio packet, or `None` at the end of the file.
    pub fn next_packet(&mut self) -> Result<Option<Packet>> {
        loop {
            if let Some(p) = self.queue.pop_front() {
                return Ok(Some(p));
            }
            let at = pos(&mut self.r)?;
            if at >= self.segment_end {
                return Ok(None);
            }
            match self.next_element() {
                Ok(true) => {}
                Ok(false) => return Ok(None),
                // Not downloaded yet: back to the start of the element, to read it whole later.
                Err(Error::WouldBlock) => {
                    self.r.seek(SeekFrom::Start(at)).map_err(io)?;
                    return Err(Error::WouldBlock);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Reads one element, queueing any packets in it; `false` at the end of the file.
    fn next_element(&mut self) -> Result<bool> {
        let (eid, size) = match read_header(&mut self.r) {
            Ok(h) => h,
            // A clean end of file (or a truncated final element) ends the stream; the
            // download giving up doesn't (that comes back as the stream's own error).
            Err(Error::Media(_)) => return Ok(false),
            Err(e) => return Err(e),
        };
        // Cluster children are read flat, so clusters of unknown size work too.
        match eid {
            id::CLUSTER => {}
            id::TIMECODE => self.cluster_tc = uint(&read_body(&mut self.r, size)?),
            id::SIMPLE_BLOCK => {
                let b = read_body(&mut self.r, size)?;
                self.parse_block(&b, 0)?;
            }
            id::BLOCK_GROUP => {
                let body = read_body(&mut self.r, size)?;
                let mut block = None;
                let mut padding = 0i64;
                for (k, v) in children(&body)? {
                    match k {
                        id::BLOCK => block = Some(v),
                        id::DISCARD_PADDING => padding = sint(v),
                        _ => {}
                    }
                }
                if let Some(b) = block {
                    self.parse_block(b, padding.max(0) as u64)?;
                }
            }
            _ if size == UNKNOWN_SIZE => {
                return Err(Error::Media(format!("unknown-size element {eid:#x}")));
            }
            _ => skip(&mut self.r, size)?,
        }
        Ok(true)
    }

    pub fn get_ref(&self) -> &R {
        &self.r
    }

    pub fn get_mut(&mut self) -> &mut R {
        &mut self.r
    }

    fn parse_block(&mut self, b: &[u8], discard_padding_ns: u64) -> Result<()> {
        let mut c = Cursor { b, p: 0 };
        let track = c.vint()?;
        if track != self.track_number {
            return Ok(());
        }
        let rel = i16::from_be_bytes([c.byte()?, c.byte()?]) as i64;
        let flags = c.byte()?;
        let ts = (self.cluster_tc as i64 + rel).max(0) as u64 * self.timecode_scale;
        let frames: Vec<&[u8]> = match (flags >> 1) & 3 {
            0 => vec![c.rest()],
            lacing => {
                let count = c.byte()? as usize + 1;
                let mut sizes = Vec::with_capacity(count);
                match lacing {
                    1 => {
                        // Xiph lacing.
                        for _ in 0..count - 1 {
                            let mut s = 0usize;
                            loop {
                                let x = c.byte()?;
                                s += x as usize;
                                if x != 255 {
                                    break;
                                }
                            }
                            sizes.push(s);
                        }
                    }
                    3 => {
                        // EBML lacing: first size, then signed differences.
                        let mut s = c.vint()? as i64;
                        sizes.push(s as usize);
                        for _ in 1..count - 1 {
                            let (raw, len) = c.vint_len()?;
                            s += raw as i64 - ((1i64 << (7 * len - 1)) - 1);
                            sizes.push(s.max(0) as usize);
                        }
                    }
                    _ => {
                        // Fixed lacing.
                        let each = c.rest_len() / count;
                        sizes.extend(std::iter::repeat_n(each, count - 1));
                    }
                }
                let used: usize = sizes.iter().sum();
                sizes.push(
                    c.rest_len()
                        .checked_sub(used)
                        .ok_or_else(|| bad("lace sizes"))?,
                );
                let mut out = Vec::with_capacity(count);
                for s in sizes {
                    out.push(c.take(s)?);
                }
                out
            }
        };
        let n = frames.len();
        for (i, f) in frames.into_iter().enumerate() {
            // Laced frames share a timestamp; the decoder works out their real positions from
            // packet lengths, so only the first carries it meaningfully.
            self.queue.push_back(Packet {
                data: f.to_vec(),
                ts_ns: ts,
                discard_padding_ns: if i + 1 == n { discard_padding_ns } else { 0 },
            });
        }
        Ok(())
    }

    /// Positions the demuxer at the last cluster starting at or before `ts_ns`.
    /// Returns the timestamp of that cluster (packets from it onwards follow).
    pub fn seek(&mut self, ts_ns: u64) -> Result<u64> {
        self.queue.clear();
        let (t, p) = match self.cues.iter().rev().find(|(t, _)| *t <= ts_ns) {
            Some(&c) => c,
            None => (0, self.first_cluster),
        };
        self.r.seek(SeekFrom::Start(p)).map_err(io)?;
        self.cluster_tc = t / self.timecode_scale;
        Ok(t)
    }
}

// ---- EBML primitives -------------------------------------------------------------------------

fn io(e: std::io::Error) -> Error {
    match e.get_ref().and_then(|i| i.downcast_ref::<Error>()) {
        Some(inner) => inner.clone(),
        None => Error::Media(e.to_string()),
    }
}

fn bad(what: &str) -> Error {
    Error::Media(format!("corrupt WebM ({what})"))
}

fn pos<R: Seek>(r: &mut R) -> Result<u64> {
    r.stream_position().map_err(io)
}

fn skip<R: Seek>(r: &mut R, size: u64) -> Result<()> {
    if size == UNKNOWN_SIZE {
        return Err(bad("cannot skip unknown-size element"));
    }
    r.seek(SeekFrom::Current(size as i64))
        .map_err(io)
        .map(|_| ())
}

fn read_body<R: Read>(r: &mut R, size: u64) -> Result<Vec<u8>> {
    if size > MAX_ELEMENT {
        return Err(bad("element too large"));
    }
    let mut v = vec![0; size as usize];
    r.read_exact(&mut v).map_err(io)?;
    Ok(v)
}

/// Reads an element ID (marker bits kept) and size (marker removed; all ones = unknown).
fn read_header<R: Read>(r: &mut R) -> Result<(u32, u64)> {
    let (eid, _) = read_vint(r, true)?;
    let (size, len) = read_vint(r, false)?;
    let size = if size == (1u64 << (7 * len)) - 1 {
        UNKNOWN_SIZE
    } else {
        size
    };
    Ok((eid as u32, size))
}

fn read_vint<R: Read>(r: &mut R, keep_marker: bool) -> Result<(u64, u32)> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b).map_err(io)?;
    let len = b[0].leading_zeros() + 1;
    if len > 8 {
        return Err(bad("vint"));
    }
    let mut v = if keep_marker {
        b[0] as u64
    } else {
        (b[0] as u64) & (0xFF >> len)
    };
    let mut rest = [0u8; 7];
    r.read_exact(&mut rest[..len as usize - 1]).map_err(io)?;
    for x in &rest[..len as usize - 1] {
        v = (v << 8) | *x as u64;
    }
    Ok((v, len))
}

/// Direct children of an element body: (id, body).
fn children(body: &[u8]) -> Result<Vec<(u32, &[u8])>> {
    let mut out = Vec::new();
    let mut c = std::io::Cursor::new(body);
    while (c.position() as usize) < body.len() {
        let (eid, size) = read_header(&mut c)?;
        let start = c.position() as usize;
        let end = start
            .checked_add(size as usize)
            .filter(|e| *e <= body.len())
            .ok_or_else(|| bad("child size"))?;
        out.push((eid, &body[start..end]));
        c.set_position(end as u64);
    }
    Ok(out)
}

fn uint(v: &[u8]) -> u64 {
    v.iter().take(8).fold(0, |a, b| (a << 8) | *b as u64)
}

fn sint(v: &[u8]) -> i64 {
    if v.is_empty() {
        return 0;
    }
    let u = uint(v);
    let bits = 8 * v.len().min(8) as u32;
    if bits < 64 && u & (1 << (bits - 1)) != 0 {
        (u as i64) - (1i64 << bits)
    } else {
        u as i64
    }
}

fn float(v: &[u8]) -> f64 {
    match v.len() {
        4 => f32::from_be_bytes(v.try_into().unwrap()) as f64,
        8 => f64::from_be_bytes(v.try_into().unwrap()),
        _ => 0.0,
    }
}

struct Cursor<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cursor<'a> {
    fn byte(&mut self) -> Result<u8> {
        let x = *self.b.get(self.p).ok_or_else(|| bad("short block"))?;
        self.p += 1;
        Ok(x)
    }
    fn vint_len(&mut self) -> Result<(u64, u32)> {
        let mut c = std::io::Cursor::new(&self.b[self.p..]);
        let r = read_vint(&mut c, false)?;
        self.p += c.position() as usize;
        Ok(r)
    }
    fn vint(&mut self) -> Result<u64> {
        Ok(self.vint_len()?.0)
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let s = self
            .b
            .get(self.p..self.p + n)
            .ok_or_else(|| bad("short frame"))?;
        self.p += n;
        Ok(s)
    }
    fn rest(&mut self) -> &'a [u8] {
        let s = &self.b[self.p..];
        self.p = self.b.len();
        s
    }
    fn rest_len(&self) -> usize {
        self.b.len() - self.p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_ints() {
        assert_eq!(sint(&[0xFF]), -1);
        assert_eq!(sint(&[0x7F]), 127);
        assert_eq!(sint(&[0x01, 0x00]), 256);
        assert_eq!(sint(&[0xFE, 0x00]), -512);
    }

    #[test]
    fn vints() {
        let mut c = std::io::Cursor::new([0x81u8]);
        assert_eq!(read_vint(&mut c, false).unwrap(), (1, 1));
        let mut c = std::io::Cursor::new([0x40u8, 0x02]);
        assert_eq!(read_vint(&mut c, false).unwrap(), (2, 2));
        let mut c = std::io::Cursor::new([0x1Au8, 0x45, 0xDF, 0xA3]);
        assert_eq!(read_vint(&mut c, true).unwrap(), (0x1A45DFA3, 4));
    }

    /// An element: its id, an 8-byte size (`None`: unknown) and the body.
    fn el(eid: u32, size: Option<usize>, body: &[u8]) -> Vec<u8> {
        let mut v: Vec<u8> = eid
            .to_be_bytes()
            .into_iter()
            .skip_while(|b| *b == 0)
            .collect();
        let size = size.map_or(0x00FF_FFFF_FFFF_FFFF, |s| s as u64);
        v.push(0x01);
        v.extend(&size.to_be_bytes()[1..]);
        v.extend(body);
        v
    }

    fn known(eid: u32, body: &[u8]) -> Vec<u8> {
        el(eid, Some(body.len()), body)
    }

    /// One Opus track and a cluster of two 100-byte blocks, 20 ms apart; and where the first
    /// block starts.
    fn webm() -> (Vec<u8>, usize) {
        let mut head = b"OpusHead".to_vec();
        head.extend([1, 2, 0x38, 0x01, 0x80, 0xBB, 0, 0, 0, 0, 0]);
        let entry = [
            known(id::TRACK_NUMBER, &[1]),
            known(id::TRACK_TYPE, &[2]),
            known(id::CODEC_ID, b"A_OPUS"),
            known(id::CODEC_PRIVATE, &head),
        ]
        .concat();
        let block = |rel: u8| {
            known(
                id::SIMPLE_BLOCK,
                &[[0x81, 0, rel, 0x80].as_slice(), &[7; 100]].concat(),
            )
        };
        let mut f = known(id::EBML, &[]);
        f.extend(el(id::SEGMENT, None, &[]));
        f.extend(known(id::TRACKS, &known(id::TRACK_ENTRY, &entry)));
        f.extend(el(id::CLUSTER, None, &[]));
        f.extend(known(id::TIMECODE, &[0]));
        let first = f.len();
        f.extend(block(0));
        f.extend(block(20));
        (f, first)
    }

    /// Bytes past `limit` aren't there yet: reads fail with `WouldBlock`, as the stream's do.
    struct Partial {
        data: std::io::Cursor<Vec<u8>>,
        limit: u64,
    }

    impl Read for Partial {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let room = self.limit.saturating_sub(self.data.position()) as usize;
            if room == 0 && (self.data.position() as usize) < self.data.get_ref().len() {
                return Err(Error::WouldBlock.into());
            }
            let n = out.len().min(room);
            self.data.read(&mut out[..n])
        }
    }

    impl Seek for Partial {
        fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
            self.data.seek(to)
        }
    }

    #[test]
    fn a_block_not_downloaded_yet_is_read_whole_later() {
        let (f, first) = webm();
        let len = f.len() as u64;
        let r = Partial {
            data: std::io::Cursor::new(f),
            limit: len,
        };
        let mut d = WebmDemuxer::open(r).unwrap();
        assert_eq!(d.track.pre_skip, 312);
        // Cut off in the middle of the first block.
        d.get_mut().limit = first as u64 + 50;
        assert!(matches!(d.next_packet(), Err(Error::WouldBlock)));
        assert_eq!(d.get_mut().data.position(), first as u64);
        d.get_mut().limit = len;
        let p = d.next_packet().unwrap().unwrap();
        assert_eq!((p.ts_ns, p.data.len()), (0, 100));
        let p = d.next_packet().unwrap().unwrap();
        assert_eq!(p.ts_ns, 20_000_000);
        assert!(d.next_packet().unwrap().is_none());
    }
}

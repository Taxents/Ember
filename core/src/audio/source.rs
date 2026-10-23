//! Decoded, sample-exact audio sources.
//!
//! A source yields interleaved stereo f32 frames at its native rate, with codec delay and end
//! padding already removed, so consecutive tracks butt together without a gap. Frame 0 is the
//! first real sample of the song; seeks land on the exact frame asked for.

use super::stream::HttpStream;
use super::webm::WebmDemuxer;
use crate::yt::StreamInfo;
use crate::{Error, Result};
use std::io::Read;

/// Opus seeks start decoding this early so the decoder has converged (RFC 7845 §4.6).
const OPUS_PREROLL: u64 = 3840;
const OPUS_RATE: u64 = 48000;
/// AAC seeks start two packets early: the first packet after a reset has no MDCT overlap.
const AAC_PREROLL: i64 = 2048;

pub trait Source: Send {
    /// Native sample rate.
    fn rate(&self) -> u32;
    /// Length in frames after trimming, when known.
    fn total_frames(&self) -> Option<u64>;
    /// Decodes the next chunk, appending interleaved stereo frames to `out`.
    /// Returns `false` at the end of the stream.
    fn read(&mut self, out: &mut Vec<f32>) -> Result<bool>;
    /// Repositions so the next frame read is exactly `frame`.
    fn seek(&mut self, frame: u64) -> Result<()>;
}

pub fn open(info: &StreamInfo, stream: HttpStream) -> Result<Box<dyn Source>> {
    if info.is_webm() {
        Ok(Box::new(OpusSource::new(stream)?))
    } else {
        Ok(Box::new(AacSource::new(stream)?))
    }
}

/// Appends `frames` frames of `ch`-channel audio as stereo, scaled by `gain`.
fn push_stereo(out: &mut Vec<f32>, src: &[f32], ch: usize, gain: f32) {
    if ch == 2 {
        if gain == 1.0 {
            out.extend_from_slice(src);
        } else {
            out.extend(src.iter().map(|s| s * gain));
        }
    } else {
        for f in src.chunks_exact(ch) {
            let s = f[0] * gain;
            out.push(s);
            out.push(s);
        }
    }
}

// ---- Opus in WebM ----------------------------------------------------------------------------

pub struct OpusSource {
    demux: WebmDemuxer<HttpStream>,
    dec: rusty_opus::OpusDecoder,
    buf: Vec<f32>,
    ch: usize,
    /// Output-frame index of the next decoded sample (negative inside the pre-skip).
    /// `None` until the first packet after opening or seeking anchors it.
    next: Option<i64>,
    /// Frames before this index are decoded but dropped (pre-skip, seek pre-roll).
    skip_until: i64,
}

impl OpusSource {
    pub fn new(stream: HttpStream) -> Result<Self> {
        let demux = WebmDemuxer::open(stream)?;
        let ch = demux.track.channels;
        Ok(OpusSource {
            dec: new_opus(ch)?,
            buf: vec![0.0; 5760 * ch],
            ch,
            demux,
            next: None,
            skip_until: 0,
        })
    }
}

/// The decode position of a packet from its timestamp. WebM stores whole milliseconds, so a
/// timestamp can be ±1 ms (48 samples) off; every Opus frame is a multiple of 2.5 ms (120
/// samples) from the start, so snapping to that grid recovers the exact position.
fn opus_sample(ts_ns: u64) -> i64 {
    let raw = (ts_ns * OPUS_RATE + 500_000_000) / 1_000_000_000;
    ((raw + 60) / 120 * 120) as i64
}

fn new_opus(ch: usize) -> Result<rusty_opus::OpusDecoder> {
    rusty_opus::OpusDecoder::new(OPUS_RATE as i32, ch)
        .map_err(|e| Error::Media(format!("opus init: {e:?}")))
}

impl Source for OpusSource {
    fn rate(&self) -> u32 {
        OPUS_RATE as u32
    }

    fn total_frames(&self) -> Option<u64> {
        let d = self.demux.duration_ns?;
        Some((d * OPUS_RATE / 1_000_000_000).saturating_sub(self.demux.track.pre_skip))
    }

    fn read(&mut self, out: &mut Vec<f32>) -> Result<bool> {
        let Some(p) = self.demux.next_packet()? else {
            return Ok(false);
        };
        let pre_skip = self.demux.track.pre_skip as i64;
        let first = *self
            .next
            .get_or_insert_with(|| opus_sample(p.ts_ns) - pre_skip);
        let n = match self.dec.decode(&p.data, 5760, &mut self.buf) {
            Ok(n) => n,
            Err(e) => {
                // A damaged packet: conceal 20 ms rather than stop the song.
                log::warn!("opus decode error {e:?}, concealing");
                self.dec.decode(&[], 960, &mut self.buf).unwrap_or(0)
            }
        };
        self.next = Some(first + n as i64);
        let pad = (p.discard_padding_ns * OPUS_RATE).div_ceil(1_000_000_000) as usize;
        let keep = n.saturating_sub(pad);
        let start = (self.skip_until - first).clamp(0, keep as i64) as usize;
        let gain = self.demux.track.gain;
        push_stereo(
            out,
            &self.buf[start * self.ch..keep * self.ch],
            self.ch,
            gain,
        );
        Ok(true)
    }

    fn seek(&mut self, frame: u64) -> Result<()> {
        let pre_skip = self.demux.track.pre_skip;
        let target = (frame + pre_skip).saturating_sub(OPUS_PREROLL);
        self.demux.seek(target * 1_000_000_000 / OPUS_RATE)?;
        self.dec = new_opus(self.ch)?;
        self.next = None;
        self.skip_until = frame as i64;
        Ok(())
    }
}

// ---- AAC in MP4 (fallback) -------------------------------------------------------------------

use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;

pub struct AacSource {
    fmt: Box<dyn FormatReader>,
    dec: Box<dyn AudioDecoder>,
    track_id: u32,
    rate: u32,
    /// Encoder delay to drop, from the MP4 edit list.
    priming: i64,
    total: Option<u64>,
    next: Option<i64>,
    skip_until: i64,
    tmp: Vec<f32>,
}

impl AacSource {
    pub fn new(stream: HttpStream) -> Result<Self> {
        let edit = read_edit_list(&mut stream.reader());
        let mss = MediaSourceStream::new(Box::new(stream), MediaSourceStreamOptions::default());
        let fmt = symphonia::default::get_probe()
            .probe(
                &Hint::new(),
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(|e| Error::Media(format!("mp4: {e}")))?;
        let track = fmt
            .default_track(TrackType::Audio)
            .ok_or_else(|| Error::Media("mp4: no audio track".into()))?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or_else(|| Error::Media("mp4: no audio parameters".into()))?
            .clone();
        let track_id = track.id;
        let rate = params
            .sample_rate
            .ok_or_else(|| Error::Media("mp4: no sample rate".into()))?;
        let dec = symphonia::default::get_codecs()
            .make_audio_decoder(&params, &AudioDecoderOptions::default())
            .map_err(|e| Error::Media(format!("aac: {e}")))?;
        let (priming, total) = match edit {
            Some(e) => (e.priming(rate), e.total(rate)),
            // No edit list (some YouTube files): the usual 1024 + 576 priming, verified against
            // the Opus stream of the same songs. The end padding (< 1024 frames) stays: the
            // player's reported duration is longer than the audio, so it can't bound it.
            None => (1600, None),
        };
        Ok(AacSource {
            fmt,
            dec,
            track_id,
            rate,
            priming: priming as i64,
            total,
            next: None,
            skip_until: 0,
            tmp: Vec::new(),
        })
    }
}

impl Source for AacSource {
    fn rate(&self) -> u32 {
        self.rate
    }

    fn total_frames(&self) -> Option<u64> {
        self.total
    }

    fn read(&mut self, out: &mut Vec<f32>) -> Result<bool> {
        loop {
            let p = match self.fmt.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => return Ok(false),
                Err(e) => return Err(Error::Media(format!("mp4: {e}"))),
            };
            if p.track_id != self.track_id {
                continue;
            }
            let first = *self.next.get_or_insert(p.pts.get() - self.priming);
            let ab = match self.dec.decode(&p) {
                Ok(ab) => ab,
                Err(e) => {
                    log::warn!("aac decode error: {e}");
                    continue;
                }
            };
            let ch = ab.spec().channels().count().max(1);
            self.tmp.resize(ab.samples_interleaved(), 0.0);
            ab.copy_to_slice_interleaved(&mut self.tmp);
            let n = (self.tmp.len() / ch) as i64;
            self.next = Some(first + n);
            let end = match self.total {
                Some(t) => (t as i64 - first).clamp(0, n),
                None => n,
            };
            let start = (self.skip_until - first).clamp(0, end);
            push_stereo(
                out,
                &self.tmp[start as usize * ch..end as usize * ch],
                ch,
                1.0,
            );
            if self.total.is_some_and(|t| first + n >= t as i64) {
                // Past the edit list's end: the rest is encoder padding.
                return Ok(end > start);
            }
            return Ok(true);
        }
    }

    fn seek(&mut self, frame: u64) -> Result<()> {
        let ts = (frame as i64 + self.priming - AAC_PREROLL).max(0);
        self.fmt
            .seek(
                SeekMode::Accurate,
                SeekTo::Timestamp {
                    ts: ts.into(),
                    track_id: self.track_id,
                },
            )
            .map_err(|e| Error::Media(format!("mp4 seek: {e}")))?;
        self.dec.reset();
        self.next = None;
        self.skip_until = frame as i64;
        Ok(())
    }
}

/// The first audio edit-list entry: where real audio starts and how long it lasts.
struct EditList {
    media_time: u64,
    media_timescale: u64,
    segment_duration: u64,
    movie_timescale: u64,
}

impl EditList {
    fn priming(&self, rate: u32) -> u64 {
        self.media_time * rate as u64 / self.media_timescale.max(1)
    }
    fn total(&self, rate: u32) -> Option<u64> {
        (self.segment_duration > 0)
            .then(|| self.segment_duration * rate as u64 / self.movie_timescale.max(1))
    }
}

/// Reads `moov` from the start of the file and pulls out the edit list (`elst`) and timescales.
fn read_edit_list(r: &mut HttpStream) -> Option<EditList> {
    let mut head = vec![0u8; (r.len() as usize).min(256 * 1024)];
    r.read_exact(&mut head).ok()?;
    let moov = find_box(&head, b"moov")?;
    let mvhd = find_box(moov, b"mvhd")?;
    let movie_timescale = timescale(mvhd)?;
    let trak = find_box(moov, b"trak")?;
    let mdhd = find_box(find_box(trak, b"mdia")?, b"mdhd")?;
    let media_timescale = timescale(mdhd)?;
    let elst = find_box(find_box(trak, b"edts")?, b"elst")?;
    let version = *elst.first()?;
    let count = be(elst.get(4..8)?);
    if count == 0 {
        return None;
    }
    let e = elst.get(8..)?;
    let (segment_duration, media_time) = if version == 1 {
        (be(e.get(0..8)?), be(e.get(8..16)?) as i64)
    } else {
        (be(e.get(0..4)?), be(e.get(4..8)?) as u32 as i32 as i64)
    };
    Some(EditList {
        media_time: media_time.max(0) as u64,
        media_timescale,
        segment_duration,
        movie_timescale,
    })
}

/// The body of the first child box called `name` (searches only this level).
fn find_box<'a>(mut data: &'a [u8], name: &[u8; 4]) -> Option<&'a [u8]> {
    while data.len() >= 8 {
        let mut size = be(&data[0..4]) as usize;
        let mut header = 8;
        if size == 1 {
            size = be(data.get(8..16)?) as usize;
            header = 16;
        } else if size == 0 {
            size = data.len();
        }
        if size < header || size > data.len() {
            return None;
        }
        if &data[4..8] == name {
            return Some(&data[header..size]);
        }
        data = &data[size..];
    }
    None
}

/// Timescale field of an `mvhd`/`mdhd` body.
fn timescale(b: &[u8]) -> Option<u64> {
    let at = if *b.first()? == 1 { 20 } else { 12 };
    Some(be(b.get(at..at + 4)?))
}

fn be(b: &[u8]) -> u64 {
    b.iter().fold(0, |a, x| (a << 8) | *x as u64)
}

#[cfg(test)]
mod tests {
    #[test]
    fn opus_timestamps_snap_to_frames() {
        assert_eq!(super::opus_sample(0), 0);
        // 20001 ms is packet 1000 (960 000 samples) written with a 1 ms rounding error.
        assert_eq!(super::opus_sample(20_001_000_000), 960_000);
        assert_eq!(super::opus_sample(19_999_000_000), 960_000);
        assert_eq!(super::opus_sample(2_500_000), 120);
    }
}

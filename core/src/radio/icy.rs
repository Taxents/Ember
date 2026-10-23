//! ICY (SHOUTcast / Icecast) in-band metadata.
//!
//! Asked for with `Icy-MetaData: 1`, a server answers with `icy-metaint: N` and then sends N
//! audio bytes, one length byte L, L × 16 metadata bytes (NUL-padded text such as
//! `StreamTitle='Artist - Title';StreamUrl='';`), N audio bytes, and so on. [`IcyDemux`]
//! removes those blocks in place, whatever the network read sizes, so the decoder only ever
//! sees audio.

/// Removes metadata blocks from an ICY stream, in place, across arbitrary chunk boundaries.
#[derive(Debug)]
pub struct IcyDemux {
    /// Audio bytes between metadata blocks (0: the stream carries no metadata).
    metaint: usize,
    state: State,
    /// The metadata block being collected (blocks may span reads).
    meta: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    /// This many audio bytes until the next length byte.
    Audio(usize),
    /// The next byte is a metadata length (× 16).
    Len,
    /// This many metadata bytes still to collect.
    Meta(usize),
}

impl IcyDemux {
    pub fn new(metaint: usize) -> Self {
        IcyDemux {
            metaint,
            state: State::Audio(metaint),
            meta: Vec::new(),
        }
    }

    /// Strips metadata from `buf` in place. Audio bytes are compacted to the front; returns how
    /// many there are. `on_meta` gets every completed, non-empty metadata block (NUL padding
    /// removed).
    pub fn feed(&mut self, buf: &mut [u8], mut on_meta: impl FnMut(&[u8])) -> usize {
        if self.metaint == 0 {
            return buf.len();
        }
        let (mut r, mut w) = (0, 0);
        while r < buf.len() {
            match self.state {
                State::Audio(left) => {
                    let n = left.min(buf.len() - r);
                    buf.copy_within(r..r + n, w);
                    (r, w) = (r + n, w + n);
                    self.state = if n == left {
                        State::Len
                    } else {
                        State::Audio(left - n)
                    };
                }
                State::Len => {
                    let len = buf[r] as usize * 16;
                    r += 1;
                    self.meta.clear();
                    self.state = if len == 0 {
                        State::Audio(self.metaint)
                    } else {
                        State::Meta(len)
                    };
                }
                State::Meta(left) => {
                    let n = left.min(buf.len() - r);
                    self.meta.extend_from_slice(&buf[r..r + n]);
                    r += n;
                    if n == left {
                        let end = self.meta.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1);
                        if end > 0 {
                            on_meta(&self.meta[..end]);
                        }
                        self.state = State::Audio(self.metaint);
                    } else {
                        self.state = State::Meta(left - n);
                    }
                }
            }
        }
        w
    }
}

/// The `StreamTitle` of a metadata block: `None` if the block has none, else the trimmed,
/// possibly empty, title. Titles may contain quotes and semicolons, so the value ends at the
/// `';` that is followed by the end of the block or by the next `Key='`.
pub fn stream_title(meta: &[u8]) -> Option<String> {
    const KEY: &[u8] = b"StreamTitle='";
    let start = meta.windows(KEY.len()).position(|w| w == KEY)? + KEY.len();
    let rest = &meta[start..];
    let at_end = |i: usize| {
        let after = &rest[i + 2..];
        let skip = after
            .iter()
            .position(|b| !b.is_ascii_whitespace() && *b != 0)
            .unwrap_or(after.len());
        let after = &after[skip..];
        after.is_empty() || next_key(after)
    };
    let end = (0..rest.len().saturating_sub(1))
        .filter(|&i| rest[i] == b'\'' && rest[i + 1] == b';')
        .find(|&i| at_end(i))
        .or_else(|| rest.iter().rposition(|b| *b == b'\''))
        .unwrap_or(rest.len());
    Some(decode(&rest[..end]).trim().to_string())
}

/// Does `s` start with `Identifier='`?
fn next_key(s: &[u8]) -> bool {
    let n = s
        .iter()
        .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_')
        .count();
    n > 0 && s[n..].starts_with(b"='")
}

/// UTF-8 when valid, else Latin-1 (older SHOUTcast servers).
fn decode(b: &[u8]) -> String {
    match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        Err(_) => b.iter().map(|&c| c as char).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An ICY stream: `audio` cut into `metaint` blocks, metadata block `metas[i]` after block i.
    fn icy(audio: &[u8], metaint: usize, metas: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, chunk) in audio.chunks(metaint).enumerate() {
            out.extend_from_slice(chunk);
            if chunk.len() < metaint {
                break;
            }
            let m = metas.get(i).copied().unwrap_or("");
            let blocks = m.len().div_ceil(16);
            out.push(blocks as u8);
            out.extend_from_slice(m.as_bytes());
            out.resize(out.len() + blocks * 16 - m.len(), 0);
        }
        out
    }

    #[test]
    fn demux_across_arbitrary_chunk_boundaries() {
        let audio: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let metas = [
            "StreamTitle='A - One';",
            "",
            "StreamTitle='B - Two';StreamUrl='';",
            "",
            "StreamTitle='';",
        ];
        let stream = icy(&audio, 1000, &metas);
        for chunk in [1, 2, 3, 7, 16, 17, 999, 1000, 1001, 4096, stream.len()] {
            let mut d = IcyDemux::new(1000);
            let (mut out, mut titles) = (Vec::new(), Vec::new());
            for c in stream.chunks(chunk) {
                let mut buf = c.to_vec();
                let n = d.feed(&mut buf, |m| titles.push(stream_title(m)));
                out.extend_from_slice(&buf[..n]);
            }
            assert_eq!(out, audio, "chunk size {chunk}");
            assert_eq!(
                titles,
                vec![
                    Some("A - One".into()),
                    Some("B - Two".into()),
                    Some(String::new())
                ],
                "chunk size {chunk}"
            );
        }
    }

    #[test]
    fn no_metaint_passes_everything() {
        let mut d = IcyDemux::new(0);
        let mut buf = b"\x05abc".to_vec();
        assert_eq!(d.feed(&mut buf, |_| panic!("no metadata")), 4);
    }

    #[test]
    fn stream_title_parsing() {
        let t = |s: &str| stream_title(s.as_bytes());
        assert_eq!(
            t("StreamTitle='Daft Punk - One More Time';StreamUrl='';").as_deref(),
            Some("Daft Punk - One More Time")
        );
        assert_eq!(t("StreamTitle='';").as_deref(), Some(""));
        assert_eq!(t("StreamUrl='x';").as_deref(), None);
        // Quotes and semicolons inside the title.
        assert_eq!(
            t("StreamTitle='Guns N' Roses - Sweet Child O' Mine';").as_deref(),
            Some("Guns N' Roses - Sweet Child O' Mine")
        );
        assert_eq!(
            t("StreamTitle='A;B - C';D';StreamUrl='http://x';").as_deref(),
            Some("A;B - C';D")
        );
        // Truncated block: up to the last quote, or everything.
        assert_eq!(
            t("StreamTitle='Half - Done'").as_deref(),
            Some("Half - Done")
        );
        assert_eq!(t("StreamTitle='Cut off").as_deref(), Some("Cut off"));
        // Latin-1 fallback.
        assert_eq!(
            stream_title(b"StreamTitle='Beyonc\xe9 - Halo';").as_deref(),
            Some("Beyonc\u{e9} - Halo")
        );
    }
}

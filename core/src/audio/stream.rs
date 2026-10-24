//! `HttpStream`: a whole audio file downloaded into memory in range requests, readable
//! (blocking) while it downloads.
//!
//! A song is 3–5 MB, so keeping it whole costs little and means a network hiccup never
//! interrupts playback and seeks are instant once downloaded. The first request is small so
//! audio can start quickly; later ones are 1 MiB. A read beyond the downloaded part moves the
//! download there.

use crate::{Error, Result, net};
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Download bookkeeping granularity.
const UNIT: u64 = 256 * 1024;
/// Units per request after the first (1 MiB).
const UNITS_PER_REQUEST: u64 = 4;
const READ_PIECE: usize = 64 * 1024;

/// Gets a fresh URL when the current one is refused (403/410 after expiry).
pub type Refresh = Box<dyn FnMut() -> Result<String> + Send>;

pub struct HttpStream {
    shared: Arc<Shared>,
    len: u64,
    pos: u64,
}

struct Shared {
    st: Mutex<State>,
    cv: Condvar,
    cancel: AtomicBool,
}

struct State {
    buf: Vec<u8>,
    have: Vec<bool>,
    /// Unit a reader is waiting for; the fetcher goes there next.
    want: u64,
    error: Option<Error>,
}

impl HttpStream {
    /// Starts downloading. Blocks until the first bytes (and so the file length) are known.
    pub fn open(url: String, len_hint: Option<u64>, refresh: Refresh) -> Result<Self> {
        let shared = Arc::new(Shared {
            st: Mutex::new(State {
                buf: Vec::new(),
                have: Vec::new(),
                want: 0,
                error: None,
            }),
            cv: Condvar::new(),
            cancel: AtomicBool::new(false),
        });
        let mut fetcher = Fetcher {
            shared: shared.clone(),
            url,
            refresh,
            len: len_hint,
            published: 0,
        };
        // The first request runs here, so `open` returns with the length known and the first
        // unit available.
        fetcher.first()?;
        let len = fetcher.len.unwrap_or(0);
        std::thread::Builder::new()
            .name("stream-fetch".into())
            .spawn(move || fetcher.run())
            .map_err(|e| Error::Net(e.to_string()))?;
        Ok(HttpStream {
            shared,
            len,
            pos: 0,
        })
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bytes downloaded so far.
    pub fn downloaded(&self) -> u64 {
        let st = self.shared.st.lock().unwrap();
        st.have.iter().filter(|h| **h).count() as u64 * UNIT
    }

    /// A second reader over the same download (e.g. for parsing headers separately).
    pub fn reader(&self) -> HttpStream {
        HttpStream {
            shared: self.shared.clone(),
            len: self.len,
            pos: 0,
        }
    }
}

impl Drop for HttpStream {
    fn drop(&mut self) {
        // The fetcher holds one reference; when only it remains nobody can read any more.
        if Arc::strong_count(&self.shared) <= 2 {
            self.shared.cancel.store(true, Ordering::Relaxed);
            self.shared.cv.notify_all();
        }
    }
}

impl Read for HttpStream {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.len || out.is_empty() {
            return Ok(0);
        }
        let unit = self.pos / UNIT;
        let mut st = self.shared.st.lock().unwrap();
        while !st.have[unit as usize] {
            if let Some(e) = &st.error {
                return Err(e.clone().into());
            }
            if self.shared.cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled.into());
            }
            st.want = unit;
            st = self.shared.cv.wait(st).unwrap();
        }
        // Copy up to the end of the contiguous downloaded run.
        let mut end_unit = unit;
        while (end_unit as usize) < st.have.len() && st.have[end_unit as usize] {
            end_unit += 1;
        }
        let end = (end_unit * UNIT).min(self.len);
        let n = out.len().min((end - self.pos) as usize);
        let p = self.pos as usize;
        out[..n].copy_from_slice(&st.buf[p..p + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for HttpStream {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        let p = match to {
            SeekFrom::Start(x) => x as i64,
            SeekFrom::End(x) => self.len as i64 + x,
            SeekFrom::Current(x) => self.pos as i64 + x,
        };
        if p < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek before start",
            ));
        }
        self.pos = p as u64;
        Ok(self.pos)
    }
}

impl symphonia::core::io::MediaSource for HttpStream {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}

struct Fetcher {
    shared: Arc<Shared>,
    url: String,
    refresh: Refresh,
    len: Option<u64>,
    /// Units published so far: a request that brought any is progress, whatever ended it.
    published: u64,
}

enum Fetch {
    Done,
    /// The URL was refused: get a new one.
    Refused(u16),
    /// A reader wants a different place; start a new request.
    Redirected,
    Failed(String),
}

impl Fetcher {
    fn first(&mut self) -> Result<()> {
        let mut refreshed = false;
        loop {
            match self.request(0, 1) {
                Fetch::Done | Fetch::Redirected => return Ok(()),
                Fetch::Refused(code) if !refreshed => {
                    log::warn!("stream refused (HTTP {code}) at start, refreshing URL");
                    self.url = (self.refresh)()?;
                    refreshed = true;
                }
                Fetch::Refused(code) => return Err(Error::Unavailable(format!("HTTP {code}"))),
                Fetch::Failed(e) => return Err(Error::Net(e)),
            }
        }
    }

    fn run(mut self) {
        let mut failures = 0u32;
        let mut refreshed = false;
        while !self.shared.cancel.load(Ordering::Relaxed) {
            let Some((start, n)) = self.next_range() else {
                return;
            };
            let published = self.published;
            let result = self.request(start, n);
            if self.published > published {
                failures = 0;
            }
            match result {
                Fetch::Done | Fetch::Redirected => {
                    failures = 0;
                    refreshed = false;
                }
                Fetch::Refused(code) if !refreshed => {
                    log::warn!("stream refused (HTTP {code}) at unit {start}, refreshing URL");
                    refreshed = true;
                    match (self.refresh)() {
                        Ok(u) => self.url = u,
                        Err(e) => return self.fail(e),
                    }
                }
                Fetch::Refused(code) => {
                    return self.fail(Error::Unavailable(format!("stream refused (HTTP {code})")));
                }
                Fetch::Failed(e) => {
                    failures += 1;
                    if failures > 4 {
                        return self.fail(Error::Net(e));
                    }
                    log::warn!("stream fetch failed ({e}), retry {failures}");
                    std::thread::sleep(Duration::from_millis(250 << failures));
                }
            }
        }
    }

    fn fail(&self, e: Error) {
        log::error!("stream failed: {e}");
        self.shared.st.lock().unwrap().error = Some(e);
        self.shared.cv.notify_all();
    }

    /// First missing unit at or after the wanted one (then from the start), and how many
    /// missing units follow it, up to one request's worth.
    fn next_range(&self) -> Option<(u64, u64)> {
        let st = self.shared.st.lock().unwrap();
        let n = st.have.len() as u64;
        let from_want = (st.want..n).find(|&u| !st.have[u as usize]);
        let start = from_want.or_else(|| (0..n).find(|&u| !st.have[u as usize]))?;
        let mut count = 1;
        while count < UNITS_PER_REQUEST && start + count < n && !st.have[(start + count) as usize] {
            count += 1;
        }
        Some((start, count))
    }

    /// Downloads `count` units starting at `unit`, publishing each unit as it completes.
    fn request(&mut self, unit: u64, count: u64) -> Fetch {
        let start = unit * UNIT;
        let mut end = start + count * UNIT - 1;
        if let Some(len) = self.len {
            end = end.min(len - 1);
        }
        let resp = net::agent()
            .get(&self.url)
            .header("User-Agent", net::BROWSER_UA)
            .header("Range", &format!("bytes={start}-{end}"))
            .call();
        let mut resp = match resp {
            Ok(r) => r,
            Err(e) => return Fetch::Failed(e.to_string()),
        };
        let status = resp.status().as_u16();
        if matches!(status, 401 | 403 | 404 | 410) {
            return Fetch::Refused(status);
        }
        if status != 206 {
            return Fetch::Failed(format!("HTTP {status}"));
        }
        if self.len.is_none() {
            let total = resp
                .headers()
                .get("content-range")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.rsplit('/').next())
                .and_then(|v| v.parse::<u64>().ok());
            match total {
                Some(t) if t > 0 => self.len = Some(t),
                _ => return Fetch::Failed("no Content-Range length".into()),
            }
        }
        let len = self.len.unwrap();
        {
            let mut st = self.shared.st.lock().unwrap();
            if st.buf.is_empty() {
                st.buf = vec![0; len as usize];
                st.have = vec![false; len.div_ceil(UNIT) as usize];
            }
        }
        end = end.min(len - 1);

        let mut body = resp.body_mut().with_config().limit(u64::MAX).reader();
        let mut piece = vec![0u8; READ_PIECE];
        let mut pos = start;
        let mut unit_start = start;
        while pos <= end {
            if self.shared.cancel.load(Ordering::Relaxed) {
                return Fetch::Done;
            }
            let want_n = ((end + 1 - pos) as usize).min(READ_PIECE);
            let n = match body.read(&mut piece[..want_n]) {
                Ok(0) => return Fetch::Failed("connection closed early".into()),
                Ok(n) => n,
                Err(e) => return Fetch::Failed(e.to_string()),
            };
            let mut st = self.shared.st.lock().unwrap();
            st.buf[pos as usize..pos as usize + n].copy_from_slice(&piece[..n]);
            pos += n as u64;
            // Publish every completed unit straight away.
            let unit_end = ((unit_start / UNIT) + 1) * UNIT;
            if pos >= unit_end.min(len) {
                let u = (unit_start / UNIT) as usize;
                st.have[u] = true;
                self.published += 1;
                unit_start = unit_end;
                self.shared.cv.notify_all();
                // A reader is blocked somewhere this request won't reach soon: go there.
                let w = st.want;
                if !st.have[w as usize] && (w < unit_start / UNIT || w > end / UNIT) {
                    return Fetch::Redirected;
                }
            }
        }
        Fetch::Done
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::testserver::{self, Behaviour};

    fn open(s: &testserver::Server) -> Result<HttpStream> {
        HttpStream::open(
            s.url.clone(),
            None,
            Box::new(|| Err(Error::Net("no new URL".into()))),
        )
    }

    #[test]
    fn reads_the_whole_file_while_it_downloads() {
        let s = testserver::serve(testserver::file(3 << 20), Behaviour::default());
        let mut st = open(&s).unwrap();
        assert_eq!(st.len(), 3 << 20);
        let mut got = Vec::new();
        st.read_to_end(&mut got).unwrap();
        assert!(got == *s.data);
    }

    #[test]
    fn a_read_far_ahead_moves_the_download_there() {
        let s = testserver::serve(
            testserver::file(8 << 20),
            Behaviour {
                rate: Some(4 << 20),
                ..Default::default()
            },
        );
        let mut st = open(&s).unwrap();
        st.seek(SeekFrom::End(-100)).unwrap();
        let mut got = vec![0; 100];
        st.read_exact(&mut got).unwrap();
        assert_eq!(got, s.data[s.data.len() - 100..]);
        // Far less than the whole file came down first.
        assert!(s.sent.load(Ordering::Relaxed) < 4 << 20);
    }

    #[test]
    fn a_refused_first_request_fails_the_open() {
        let s = testserver::serve(
            testserver::file(1 << 20),
            Behaviour {
                drop_after: Some(0),
                ..Default::default()
            },
        );
        assert!(matches!(open(&s), Err(Error::Net(_))));
    }

    #[test]
    fn connections_that_drop_but_make_progress_never_give_up() {
        // Every request after the first ends early, but each one brings two more units.
        let s = testserver::serve(
            testserver::file(3 << 20),
            Behaviour {
                drop_after: Some(600_000),
                ..Default::default()
            },
        );
        let mut st = open(&s).unwrap();
        let mut got = Vec::new();
        st.read_to_end(&mut got).unwrap();
        assert!(got == *s.data);
    }
}

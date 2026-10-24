//! A local HTTP server for stream tests: serves one file in byte ranges, slowly, stalling or
//! dropping connections as told.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

#[derive(Default)]
pub struct Behaviour {
    /// Body bytes per second (`None`: as fast as possible).
    pub rate: Option<u64>,
    /// After this many body bytes in all, send nothing more (the connection stays open).
    pub stall_after: Option<u64>,
    /// Close each response's connection after this many of its body bytes.
    pub drop_after: Option<u64>,
    /// Answer 403 (an expired URL) to every request after this many.
    pub refuse_after: Option<u64>,
}

pub struct Server {
    pub url: String,
    pub data: Arc<Vec<u8>>,
    stop: Arc<AtomicBool>,
    /// Body bytes sent so far.
    pub sent: Arc<AtomicU64>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// A file of `len` bytes whose content says where each byte is.
pub fn file(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

pub fn serve(data: Vec<u8>, b: Behaviour) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/audio", listener.local_addr().unwrap());
    let data = Arc::new(data);
    let stop = Arc::new(AtomicBool::new(false));
    let sent = Arc::new(AtomicU64::new(0));
    let requests = Arc::new(AtomicU64::new(0));
    let b = Arc::new(b);
    listener.set_nonblocking(true).unwrap();
    {
        let (data, stop, sent) = (data.clone(), stop.clone(), sent.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((s, _)) => {
                        let (data, stop, sent, b, requests) = (
                            data.clone(),
                            stop.clone(),
                            sent.clone(),
                            b.clone(),
                            requests.clone(),
                        );
                        std::thread::spawn(move || {
                            let _ = connection(s, &data, &b, &stop, &sent, &requests);
                        });
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
    }
    Server {
        url,
        data,
        stop,
        sent,
    }
}

fn connection(
    s: TcpStream,
    data: &[u8],
    b: &Behaviour,
    stop: &AtomicBool,
    sent: &AtomicU64,
    requests: &AtomicU64,
) -> std::io::Result<()> {
    s.set_nonblocking(false)?;
    let mut r = BufReader::new(s.try_clone()?);
    let mut w = s;
    loop {
        // Request line and headers; only Range matters.
        let mut range = None;
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Ok(());
        }
        loop {
            line.clear();
            r.read_line(&mut line)?;
            let l = line.trim();
            if l.is_empty() {
                break;
            }
            if let Some(v) = l.to_ascii_lowercase().strip_prefix("range: bytes=") {
                let (a, z) = v.split_once('-').unwrap();
                let a: usize = a.parse().unwrap();
                let z: usize = z.parse().unwrap_or(data.len() - 1);
                range = Some((a, z.min(data.len() - 1)));
            }
        }
        if b.refuse_after
            .is_some_and(|n| requests.fetch_add(1, Ordering::Relaxed) >= n)
        {
            write!(w, "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")?;
            continue;
        }
        let (a, z) = range.unwrap_or((0, data.len() - 1));
        write!(
            w,
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {a}-{z}/{}\r\n\
             Content-Length: {}\r\n\r\n",
            data.len(),
            z + 1 - a
        )?;
        let mut pos = a;
        let piece = 4096;
        while pos <= z {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            if b.stall_after
                .is_some_and(|n| sent.load(Ordering::Relaxed) >= n)
            {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            if b.drop_after.is_some_and(|n| (pos - a) as u64 >= n) {
                let _ = w.shutdown(std::net::Shutdown::Both);
                return Ok(());
            }
            let end = (pos + piece).min(z + 1);
            w.write_all(&data[pos..end])?;
            sent.fetch_add((end - pos) as u64, Ordering::Relaxed);
            if let Some(rate) = b.rate {
                std::thread::sleep(Duration::from_secs_f64((end - pos) as f64 / rate as f64));
            }
            pos = end;
        }
        w.flush()?;
    }
}

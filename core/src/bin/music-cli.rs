//! Test harness for the audio core.
//!
//!   music-cli play <videoId>... [--eq bass|vocal|treble|flat] [--vol 0.8]
//!       Plays the list gaplessly. Commands on stdin: p (pause/resume), n, b, s <sec>,
//!       v <0-100>, eq <preset|off>, q.
//!   music-cli decode <out-dir> <videoId>... [--rate 48000] [--itag 140]
//!       Saves each original file and its decoded PCM (f32 stereo) plus the gapless
//!       concatenation, for comparison with ffmpeg.
//!   music-cli seekcheck <videoId> [--itag 140]
//!       Seeks to several positions and compares against a straight decode.
//!   music-cli resolve <videoId>
//!       Prints the chosen stream and timings.

use music_core::audio::resample::Resampler;
use music_core::audio::source;
use music_core::audio::stream::HttpStream;
use music_core::audio::{Engine, EqSettings, Event};
use music_core::player::Player;
use music_core::yt::Resolver;
use std::io::{BufRead, Read, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Logger;
impl log::Log for Logger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, r: &log::Record) {
        if r.target().starts_with("music_core") || r.level() <= log::Level::Warn {
            eprintln!("[{:5}] {}", r.level(), r.args());
        }
    }
    fn flush(&self) {}
}

fn main() {
    let level = match std::env::var("MUSIC_LOG").as_deref() {
        Ok("debug") => log::LevelFilter::Debug,
        Ok("warn") => log::LevelFilter::Warn,
        _ => log::LevelFilter::Info,
    };
    log::set_logger(&Logger).ok();
    log::set_max_level(level);

    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let positional: Vec<String> = {
        let mut v = Vec::new();
        let mut skip = false;
        for a in args.iter().skip(1) {
            if skip {
                skip = false;
            } else if a.starts_with("--") {
                skip = true;
            } else {
                v.push(a.clone());
            }
        }
        v
    };
    let r = match args.first().map(String::as_str) {
        Some("play") if !positional.is_empty() => play(positional, flag("--eq"), flag("--vol")),
        Some("decode") if positional.len() >= 2 => decode(
            &positional[0],
            &positional[1..],
            flag("--rate"),
            flag("--itag"),
        ),
        Some("resolve") if !positional.is_empty() => resolve(&positional[0]),
        Some("seekcheck") if !positional.is_empty() => seekcheck(&positional[0], flag("--itag")),
        _ => {
            eprintln!("usage: music-cli play <id>... | decode <dir> <id>... | resolve <id>");
            std::process::exit(2);
        }
    };
    if let Err(e) = r {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn eq_from(name: &str) -> Option<EqSettings> {
    if name == "off" {
        return Some(EqSettings::default());
    }
    EqSettings::preset(name).map(|g| EqSettings {
        enabled: true,
        gains: g,
        preamp: None,
    })
}

fn play(ids: Vec<String>, eq: Option<String>, vol: Option<String>) -> music_core::Result<()> {
    let resolver = Arc::new(Resolver::new());
    let t0 = Instant::now();
    let engine = Engine::new(resolver)?;
    if let Some(e) = eq.as_deref().and_then(eq_from) {
        engine.set_eq(&e);
    }
    if let Some(v) = vol.and_then(|v| v.parse::<f32>().ok()) {
        engine.set_volume(v);
    }
    let mut player = Player::new(engine);
    player.play_queue(ids, 0);

    let (tx, rx) = crossbeam_channel::unbounded::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut first_audio = true;
    let mut last_status = Instant::now();
    loop {
        crossbeam_channel::select! {
            recv(player.engine.events()) -> e => {
                let Ok(e) = e else { break };
                match &e {
                    Event::Started { id, duration_ms } => {
                        if first_audio {
                            println!("first audio after {} ms", t0.elapsed().as_millis());
                            first_audio = false;
                        }
                        println!("▶ {id} ({})", fmt_ms(duration_ms.unwrap_or(0)));
                    }
                    Event::Finished => {
                        println!("■ finished (underruns: {})", player.engine.underruns());
                        player.on_event(&e);
                        return Ok(());
                    }
                    Event::Failed { id, error } => println!("✗ {id}: {error}"),
                    Event::Device(d) => println!("device: {d}"),
                }
                player.on_event(&e);
            }
            recv(rx) -> line => {
                let Ok(line) = line else { continue };
                let mut it = line.split_whitespace();
                match (it.next(), it.next()) {
                    (Some("p"), _) => {
                        if player.engine.position().is_some_and(|p| p.paused) {
                            player.engine.resume();
                        } else {
                            player.engine.pause();
                        }
                    }
                    (Some("n"), _) => player.next(),
                    (Some("b"), _) => player.previous(),
                    (Some("s"), Some(s)) => {
                        if let Ok(s) = s.parse::<f64>() {
                            player.engine.seek((s * 1000.0) as u64);
                        }
                    }
                    (Some("v"), Some(v)) => {
                        if let Ok(v) = v.parse::<f32>() {
                            player.engine.set_volume(v / 100.0);
                        }
                    }
                    (Some("eq"), Some(n)) => match eq_from(n) {
                        Some(e) => player.engine.set_eq(&e),
                        None => println!("presets: flat bass vocal treble off"),
                    },
                    (Some("q"), _) => {
                        println!("quit (underruns: {})", player.engine.underruns());
                        return Ok(());
                    }
                    _ => println!("p | n | b | s <sec> | v <0-100> | eq <preset|off> | q"),
                }
            }
            default(Duration::from_secs(1)) => {}
        }
        if last_status.elapsed() >= Duration::from_secs(5) {
            last_status = Instant::now();
            if let Some(p) = player.engine.position() {
                println!(
                    "  {} {} / {}{}",
                    p.id,
                    fmt_ms(p.ms),
                    fmt_ms(p.duration_ms.unwrap_or(0)),
                    if p.paused { " (paused)" } else { "" }
                );
            }
        }
    }
    Ok(())
}

fn fmt_ms(ms: u64) -> String {
    format!("{}:{:02}", ms / 60000, ms / 1000 % 60)
}

fn decode(
    dir: &str,
    ids: &[String],
    rate: Option<String>,
    itag: Option<String>,
) -> music_core::Result<()> {
    let dir = std::path::Path::new(dir);
    std::fs::create_dir_all(dir).ok();
    let mut resolver = Resolver::new();
    if let Some(i) = itag.and_then(|i| i.parse::<u32>().ok()) {
        resolver = resolver.with_itags(&[i]);
    }
    let out_rate: Option<u32> = rate.and_then(|r| r.parse().ok());
    let mut all = std::fs::File::create(dir.join("gapless.f32")).expect("create");
    for id in ids {
        let t0 = Instant::now();
        let info = resolver.resolve(id)?;
        let stream = HttpStream::open(
            info.url.clone(),
            info.content_length,
            Box::new(|| {
                Err(music_core::Error::Unavailable(
                    "no refresh in decode".into(),
                ))
            }),
        )?;
        // Keep the original file for ffmpeg.
        let mut raw = Vec::new();
        stream.reader().read_to_end(&mut raw).expect("download");
        let ext = if info.is_webm() { "webm" } else { "m4a" };
        std::fs::write(dir.join(format!("{id}.{ext}")), &raw).expect("write");
        let t_dl = t0.elapsed();

        let mut src = source::open(&info, stream)?;
        let mut rs = out_rate
            .filter(|r| *r != src.rate())
            .map(|r| Resampler::new(src.rate(), r));
        let t1 = Instant::now();
        let mut pcm = Vec::new();
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let more = src.read(&mut buf)?;
            match &mut rs {
                Some(r) => {
                    r.process(&buf, &mut pcm);
                    if !more {
                        r.finish(&mut pcm);
                    }
                }
                None => pcm.extend_from_slice(&buf),
            }
            if !more {
                break;
            }
        }
        let dec = t1.elapsed();
        let frames = pcm.len() / 2;
        let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
        std::fs::write(dir.join(format!("{id}.f32")), &bytes).expect("write");
        all.write_all(&bytes).expect("write");
        let r = out_rate.unwrap_or(src.rate());
        let peak = pcm.iter().fold(0f32, |a, s| a.max(s.abs()));
        println!(
            "{id}: itag {} {} frames @ {r} Hz ({:.3} s), header said {:?}; download {} ms, decode {} ms ({:.0}x realtime), peak {peak:.3}",
            info.itag,
            frames,
            frames as f64 / r as f64,
            src.total_frames(),
            t_dl.as_millis(),
            dec.as_millis(),
            frames as f64 / r as f64 / dec.as_secs_f64().max(1e-9),
        );
    }
    Ok(())
}

fn resolve(id: &str) -> music_core::Result<()> {
    let r = Resolver::new();
    let t = Instant::now();
    let info = r.resolve(id)?;
    let t_res = t.elapsed();
    let t = Instant::now();
    let s = HttpStream::open(
        info.url.clone(),
        info.content_length,
        Box::new(|| Err(music_core::Error::Unavailable("no refresh".into()))),
    )?;
    println!(
        "{id}: itag {} {} {:?} bytes, {:?} ms; resolve {} ms, first bytes {} ms, expires in {} min",
        info.itag,
        info.mime,
        info.content_length,
        info.duration_ms,
        t_res.as_millis(),
        t.elapsed().as_millis(),
        (info
            .expires_at
            .saturating_sub(music_core::paths::unix_now()))
            / 60
    );
    drop(s);
    Ok(())
}

fn seekcheck(id: &str, itag: Option<String>) -> music_core::Result<()> {
    let mut resolver = Resolver::new();
    if let Some(i) = itag.and_then(|i| i.parse::<u32>().ok()) {
        resolver = resolver.with_itags(&[i]);
    }
    let info = resolver.resolve(id)?;
    let open = || -> music_core::Result<Box<dyn source::Source>> {
        let s = HttpStream::open(
            info.url.clone(),
            info.content_length,
            Box::new(|| Err(music_core::Error::Unavailable("no refresh".into()))),
        )?;
        source::open(&info, s)
    };
    let mut src = open()?;
    let mut full = Vec::new();
    while src.read(&mut full)? {}
    let frames = full.len() / 2;
    let mut src = open()?;
    let n = 9600;
    for f in [
        0,
        1,
        959,
        48000,
        1_234_567,
        frames / 2 + 17,
        frames - 50_000,
    ] {
        let t = Instant::now();
        src.seek(f as u64)?;
        let mut got = Vec::new();
        while got.len() < n * 2 && src.read(&mut got)? {}
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let want = &full[f * 2..(f + n).min(frames) * 2];
        let got = &got[..want.len().min(got.len())];
        let (mut sig, mut noise, mut worst) = (0f64, 0f64, 0f32);
        for (a, b) in want.iter().zip(got) {
            sig += (*a as f64).powi(2);
            noise += ((a - b) as f64).powi(2);
            worst = worst.max((a - b).abs());
        }
        // Where the samples line up best (0 = exact position).
        let mut best = (0i64, f64::MIN);
        for lag in -50i64..=50 {
            let mut acc = 0f64;
            for i in (2000..want.len() / 2 - 100).step_by(3) {
                let j = (i as i64 + lag) as usize;
                acc += want[i * 2] as f64 * got.get(j * 2).copied().unwrap_or(0.0) as f64;
            }
            if acc > best.1 {
                best = (lag, acc);
            }
        }
        println!(
            "seek to {f:>9}: offset {:>3} frames, SNR {:6.1} dB, max diff {worst:.1e}, {ms:.1} ms",
            best.0,
            10.0 * (sig / noise.max(1e-30)).log10()
        );
    }
    Ok(())
}

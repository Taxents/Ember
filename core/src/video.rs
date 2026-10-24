//! Music-video pictures, kept in step with the audio engine.
//!
//! The sound of a music video always comes from the audio engine, like any song. This only
//! shows its picture: a video-only H.264 stream (see `Resolver::resolve_video`) decoded by
//! Media Foundation (hardware through D3D11 when there is a GPU, else Windows' software
//! decoder), each frame handed over when the engine's position reaches its timestamp.
//!
//! The stream is fetched by the same `HttpStream` as audio (Media Foundation's own HTTP source
//! stalls on YouTube's URLs) and handed to Media Foundation as an `IStream`. YouTube's DASH MP4s
//! carry an edit list that makes Media Foundation's MP4 source report an empty stream, so it is
//! hidden (read as a `free` box) and its offset applied to the timestamps here instead.
//!
//! One thread per shown video. It exists only while the picture is on screen: stopping it frees
//! the decoder, the D3D device and the network source. Frames are read back one at a time,
//! only when due, so at most one decoded frame waits here. Stopping also cancels the stream
//! (Media Foundation's reads fail at once) and waits briefly for the thread to end.

use crate::audio::Clock;
use crate::audio::stream::HttpStream;
use crate::yt::{Resolver, VideoInfo};
use crate::{Error, Result};
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use windows::Win32::Foundation::{E_NOTIMPL, HMODULE, S_FALSE, S_OK, STG_E_INVALIDFUNCTION};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::*;
use windows::core::{GUID, HRESULT, Interface, implement, w};

/// A frame this late is skipped (decoding catches up faster than real time).
const LATE_MS: i64 = 100;
/// Shown this early: a frame is due when the audio is within this of it.
const EARLY_MS: i64 = 8;
/// The audio jumped back further than this, or ahead further than `JUMP_AHEAD_MS`: seek the
/// video instead of decoding up to it.
const JUMP_BACK_MS: i64 = 500;
const JUMP_AHEAD_MS: i64 = 3000;
/// Longest sleep while waiting, so pauses, seeks and stops are noticed quickly.
const POLL: Duration = Duration::from_millis(40);
/// While paused or waiting for the audio to start.
const IDLE_POLL: Duration = Duration::from_millis(100);
/// How long dropping a player waits for its thread to end.
const JOIN_WAIT: Duration = Duration::from_millis(500);

/// One decoded picture: `width × height` RGBA pixels, rows packed.
pub struct Frame<'a> {
    pub width: u32,
    pub height: u32,
    pub rgba: &'a [u8],
    /// Its time in the video.
    pub ms: u64,
}

/// What the player tells its owner.
pub enum Note<'a> {
    /// A frame to show now.
    Frame(Frame<'a>),
    /// The picture can't be shown (no usable format, decode or network failure). Final.
    Failed(Error),
}

/// Shows one music video's picture while it lives. Dropping it stops the thread and waits up
/// to [`JOIN_WAIT`] for it to end; one stuck longer (a read still waiting on the network) is
/// left to finish by itself, and may hand over one last frame the owner should ignore.
pub struct VideoPlayer {
    stop: Arc<AtomicBool>,
    thread: Option<(JoinHandle<()>, crossbeam_channel::Receiver<()>)>,
}

impl VideoPlayer {
    /// Starts decoding `video_id`'s picture, paced by `clock` while the engine plays that same
    /// id, from a stream about `height` pixels tall. `sink` gets every frame when it's due, from
    /// the player's thread.
    pub fn start(
        video_id: String,
        height: u32,
        resolver: Arc<Resolver>,
        clock: Clock,
        mut sink: impl FnMut(Note) + Send + 'static,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        let (done_tx, done) = crossbeam_channel::bounded(1);
        let spawned = std::thread::Builder::new()
            .name("video".into())
            .spawn(move || {
                unsafe {
                    let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
                }
                if let Err(e) = run(&video_id, height, &resolver, &clock, &st, &mut sink)
                    && !st.load(Ordering::Relaxed)
                {
                    log::warn!("video {video_id}: {e}");
                    sink(Note::Failed(e));
                }
                let _ = done_tx.send(());
            });
        let thread = match spawned {
            Ok(h) => Some((h, done)),
            Err(e) => {
                log::warn!("video thread: {e}");
                None
            }
        };
        VideoPlayer { stop, thread }
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        // Also makes the stream's reads fail, so a waiting `ReadSample` returns.
        self.stop.store(true, Ordering::Relaxed);
        if let Some((h, done)) = self.thread.take() {
            if done.recv_timeout(JOIN_WAIT).is_ok() {
                let _ = h.join();
            } else {
                log::info!("video: thread still busy, left to finish");
            }
        }
    }
}

/// Media Foundation, for the life of one thread.
struct Mf;

impl Mf {
    fn start() -> Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .map_err(media)?;
            if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
                CoUninitialize();
                return Err(media(e));
            }
        }
        Ok(Mf)
    }
}

impl Drop for Mf {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
}

fn media(e: windows::core::Error) -> Error {
    Error::Media(format!("{} ({:#010x})", e.message(), e.code().0))
}

/// Whether opening failed on the stream itself (refused, expired, network), which a fresh URL
/// may fix, rather than on what it holds (format, decoder), which it won't.
fn stream_error(e: &Error) -> bool {
    matches!(e, Error::Net(_) | Error::Unavailable(_))
}

/// The stream and its decoder: from cache, else after resolving again (an expired URL).
fn open(
    id: &str,
    height: u32,
    resolver: &Arc<Resolver>,
    cancel: &Arc<AtomicBool>,
) -> Result<(VideoInfo, Decoder)> {
    let resolve = || {
        resolver
            .resolve_video(id, height)?
            .ok_or_else(|| Error::Unavailable(format!("{id}: no video format")))
    };
    let info = resolve()?;
    match Decoder::open(&info, resolver, cancel) {
        Ok(d) => Ok((info, d)),
        Err(e) if stream_error(&e) && !cancel.load(Ordering::Relaxed) => {
            log::info!("video {id}: {e}; resolving again");
            resolver.invalidate_video(id);
            let info = resolve()?;
            let d = Decoder::open(&info, resolver, cancel)?;
            Ok((info, d))
        }
        Err(e) => Err(e),
    }
}

fn run(
    id: &str,
    height: u32,
    resolver: &Arc<Resolver>,
    clock: &Clock,
    stop: &Arc<AtomicBool>,
    sink: &mut dyn FnMut(Note),
) -> Result<()> {
    let _mf = Mf::start()?;
    let (info, mut dec) = open(id, height, resolver, stop)?;
    log::info!(
        "video {id}: itag {} {}x{} @{} ({}), starts {} ms in",
        info.itag,
        dec.width,
        dec.height,
        info.fps,
        if dec.hardware { "hardware" } else { "software" },
        dec.offset_ms
    );

    // The next frame, decoded but not yet read back, and its time.
    let mut held: Option<(i64, IMFSample)> = None;
    // Where the picture is: the newest frame decoded, or the last seek's target while the
    // decoder works up to it from the keyframe before.
    let mut cursor: i64 = 0;
    // The last seek's target: frames before it are skipped without being shown.
    let mut target: i64 = 0;
    // Nothing shown since opening or seeking: show the next frame even if paused.
    let mut fresh = true;
    let mut eof = false;
    let mut dropped = 0u32;
    while !stop.load(Ordering::Relaxed) {
        let Some(pos) = clock.position().filter(|p| p.id == id) else {
            std::thread::sleep(IDLE_POLL);
            continue;
        };
        let t = pos.ms as i64;
        // The audio moved somewhere else (seek, restart, or the page opened mid-song): follow
        // it.
        if t < cursor - JUMP_BACK_MS || t > cursor + JUMP_AHEAD_MS {
            held = None;
            dec.seek(t)?;
            cursor = t;
            target = t;
            fresh = true;
            eof = false;
            dropped = 0;
        }
        if held.is_none() {
            if eof {
                std::thread::sleep(IDLE_POLL);
                continue;
            }
            match dec.read()? {
                Some((ms, s)) => {
                    cursor = cursor.max(ms);
                    held = Some((ms, s));
                }
                None => {
                    eof = true;
                    continue;
                }
            }
        }
        let due = held.as_ref().map_or(0, |h| h.0);
        if due > t + EARLY_MS && !(fresh && pos.paused) {
            let wait = if pos.paused {
                IDLE_POLL
            } else {
                Duration::from_millis((due - t - EARLY_MS) as u64).min(POLL)
            };
            std::thread::sleep(wait);
            continue;
        }
        let (ms, sample) = held.take().expect("held frame");
        // Too late to be worth showing: on the way to a seek's target, or behind (then
        // still one in 15, so the picture never freezes).
        if ms < t - LATE_MS && (ms < target - LATE_MS || dropped < 15) {
            dropped += 1;
            continue;
        }
        dropped = 0;
        fresh = false;
        let (width, height) = (dec.width, dec.height);
        let rgba = dec.copy(&sample)?;
        sink(Note::Frame(Frame {
            width,
            height,
            rgba,
            ms: ms.max(0) as u64,
        }));
    }
    Ok(())
}

/// An IMFSourceReader over the downloading stream, giving RGBA frames.
struct Decoder {
    reader: IMFSourceReader,
    /// The hidden edit list's start: frame times are this much later than their place in the
    /// song.
    offset_ms: i64,
    /// Present with hardware decode: frames are GPU textures, read back through `staging`.
    d3d: Option<(ID3D11Device, ID3D11DeviceContext)>,
    _manager: Option<IMFDXGIDeviceManager>,
    staging: Option<ID3D11Texture2D>,
    hardware: bool,
    /// The reader's output is RGBA bytes already (`ABGR32`); otherwise BGRX (`RGB32`).
    rgba_out: bool,
    /// The RGBA output already has opaque alpha (learned from the first frame), so rows are
    /// plain copies.
    opaque: Option<bool>,
    /// Output size, and the picture's place in it (decoders pad to whole macroblocks).
    full: (u32, u32),
    crop: (u32, u32),
    width: u32,
    height: u32,
    frame: Vec<u8>,
}

const STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
/// `D3DFMT_A8B8G8R8`: R, G, B, A bytes (not in the `windows` crate's list).
const MF_VIDEO_FORMAT_ABGR32: GUID = GUID::from_u128(0x00000020_0000_0010_8000_00aa00389b71);

impl Decoder {
    fn open(info: &VideoInfo, resolver: &Arc<Resolver>, cancel: &Arc<AtomicBool>) -> Result<Self> {
        let r = resolver.clone();
        let (vid, height) = (info.video_id.clone(), info.height);
        let refresh = Box::new(move || {
            r.invalidate_video(&vid);
            r.resolve_video(&vid, height)?
                .map(|v| v.url)
                .ok_or_else(|| Error::Unavailable(format!("{vid}: no video format")))
        });
        let stream = HttpStream::open(info.url.clone(), info.content_length, refresh)?;
        let head = mp4_head(&mut stream.reader())?;
        let len = stream.len();
        let source: IStream = Source {
            inner: Mutex::new(stream),
            len,
            hide: head.edts,
            cancel: cancel.clone(),
        }
        .into();
        unsafe {
            let mut attrs: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attrs, 4).map_err(media)?;
            let attrs = attrs.ok_or_else(|| Error::Media("no attributes".into()))?;
            // Lets the reader convert (and, with D3D, do it on the GPU).
            attrs
                .SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)
                .map_err(media)?;
            let d3d = d3d_device();
            let mut manager = None;
            if let Some((dev, _)) = &d3d {
                let mut token = 0u32;
                let mut m: Option<IMFDXGIDeviceManager> = None;
                if MFCreateDXGIDeviceManager(&mut token, &mut m).is_ok()
                    && let Some(m) = m
                    && m.ResetDevice(dev, token).is_ok()
                {
                    attrs
                        .SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &m)
                        .map_err(media)?;
                    attrs
                        .SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)
                        .map_err(media)?;
                    manager = Some(m);
                }
            }
            let d3d = if manager.is_some() { d3d } else { None };
            let bytes = MFCreateMFByteStreamOnStream(&source).map_err(media)?;
            if let Ok(a) = bytes.cast::<IMFAttributes>() {
                let _ = a.SetString(&MF_BYTESTREAM_CONTENT_TYPE, w!("video/mp4"));
            }
            let reader = MFCreateSourceReaderFromByteStream(&bytes, &attrs).map_err(media)?;
            reader
                .SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)
                .map_err(media)?;
            reader.SetStreamSelection(STREAM, true).map_err(media)?;
            let mt = MFCreateMediaType().map_err(media)?;
            mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(media)?;
            // RGBA straight from the video processor when it can; else BGRX, swizzled here.
            mt.SetGUID(&MF_MT_SUBTYPE, &MF_VIDEO_FORMAT_ABGR32)
                .map_err(media)?;
            let rgba_out = reader.SetCurrentMediaType(STREAM, None, &mt).is_ok();
            if !rgba_out {
                mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
                    .map_err(media)?;
                reader
                    .SetCurrentMediaType(STREAM, None, &mt)
                    .map_err(media)?;
            }
            let mut d = Decoder {
                reader,
                offset_ms: head.offset_ms(),
                hardware: d3d.is_some(),
                d3d,
                _manager: manager,
                staging: None,
                rgba_out,
                opaque: None,
                full: (0, 0),
                crop: (0, 0),
                width: 0,
                height: 0,
                frame: Vec::new(),
            };
            d.read_size()?;
            Ok(d)
        }
    }

    /// The output size, after opening or a format change.
    fn read_size(&mut self) -> Result<()> {
        unsafe {
            let cur = self.reader.GetCurrentMediaType(STREAM).map_err(media)?;
            let fs = cur.GetUINT64(&MF_MT_FRAME_SIZE).map_err(media)?;
            let (w, h) = ((fs >> 32) as u32, fs as u32);
            self.full = (w, h);
            self.crop = (0, 0);
            self.width = w;
            self.height = h;
            // The visible part, when the decoder padded the picture.
            let mut area = MFVideoArea::default();
            let size = std::mem::size_of::<MFVideoArea>() as u32;
            let p = std::slice::from_raw_parts_mut(&mut area as *mut _ as *mut u8, size as usize);
            if cur
                .GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, p, None)
                .is_ok()
            {
                let (x, y) = (
                    area.OffsetX.value.max(0) as u32,
                    area.OffsetY.value.max(0) as u32,
                );
                let (cw, ch) = (area.Area.cx.max(0) as u32, area.Area.cy.max(0) as u32);
                if cw > 0 && ch > 0 && x + cw <= w && y + ch <= h {
                    self.crop = (x, y);
                    self.width = cw;
                    self.height = ch;
                }
            }
            self.frame = vec![0; (self.width * self.height * 4) as usize];
            self.staging = None;
        }
        Ok(())
    }

    /// The next frame and its time in ms, or `None` at the end.
    fn read(&mut self) -> Result<Option<(i64, IMFSample)>> {
        loop {
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut sample: Option<IMFSample> = None;
            unsafe {
                self.reader
                    .ReadSample(
                        STREAM,
                        0,
                        None,
                        Some(&mut flags),
                        Some(&mut ts),
                        Some(&mut sample),
                    )
                    .map_err(media)?;
            }
            if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
                return Err(Error::Media("video stream error".into()));
            }
            if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
                self.read_size()?;
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                return Ok(None);
            }
            if let Some(s) = sample {
                return Ok(Some((ts / 10_000 - self.offset_ms, s)));
            }
        }
    }

    fn seek(&mut self, ms: i64) -> Result<()> {
        let mut pv = PROPVARIANT::default();
        unsafe {
            let v = &mut *pv.Anonymous.Anonymous;
            v.vt = windows::Win32::System::Variant::VT_I8;
            v.Anonymous.hVal = (ms + self.offset_ms).max(0) * 10_000;
            self.reader
                .SetCurrentPosition(&GUID::zeroed(), &pv)
                .map_err(media)
        }
    }

    /// Reads a frame back into RGBA.
    fn copy(&mut self, sample: &IMFSample) -> Result<&[u8]> {
        unsafe {
            let buf = sample.GetBufferByIndex(0).map_err(media)?;
            let d3d = self.d3d.clone();
            if let (Some((dev, ctx)), Ok(dxgi)) = (&d3d, buf.cast::<IMFDXGIBuffer>()) {
                let mut p = std::ptr::null_mut();
                dxgi.GetResource(&ID3D11Texture2D::IID, &mut p)
                    .map_err(media)?;
                let tex = ID3D11Texture2D::from_raw(p);
                let sub = dxgi.GetSubresourceIndex().map_err(media)?;
                if self.staging.is_none() {
                    let mut d = D3D11_TEXTURE2D_DESC::default();
                    tex.GetDesc(&mut d);
                    let sd = D3D11_TEXTURE2D_DESC {
                        Width: d.Width,
                        Height: d.Height,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: d.Format,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Usage: D3D11_USAGE_STAGING,
                        BindFlags: 0,
                        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                        MiscFlags: 0,
                    };
                    let mut s = None;
                    dev.CreateTexture2D(&sd, None, Some(&mut s))
                        .map_err(media)?;
                    self.staging = s;
                }
                let staging = &self.staging.clone().expect("staging");
                ctx.CopySubresourceRegion(staging, 0, 0, 0, 0, &tex, sub, None);
                let mut m = D3D11_MAPPED_SUBRESOURCE::default();
                ctx.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut m))
                    .map_err(media)?;
                self.convert(m.pData as *const u8, m.RowPitch as isize);
                ctx.Unmap(staging, 0);
            } else if let Ok(b2) = buf.cast::<IMF2DBuffer>() {
                let mut p = std::ptr::null_mut();
                let mut pitch = 0i32;
                b2.Lock2D(&mut p, &mut pitch).map_err(media)?;
                self.convert(p, pitch as isize);
                b2.Unlock2D().map_err(media)?;
            } else {
                let mut p = std::ptr::null_mut();
                let mut len = 0u32;
                buf.Lock(&mut p, None, Some(&mut len)).map_err(media)?;
                if (len as usize) >= (self.full.0 * self.full.1 * 4) as usize {
                    self.convert(p, self.full.0 as isize * 4);
                }
                buf.Unlock().map_err(media)?;
            }
        }
        Ok(&self.frame)
    }

    /// Copies the visible part of a locked 32-bit frame into `self.frame` as RGBA, opaque.
    ///
    /// # Safety
    /// `src` points at a frame of `self.full` pixels with rows `pitch` bytes apart.
    unsafe fn convert(&mut self, src: *const u8, pitch: isize) {
        let (w, h) = (self.width as usize, self.height as usize);
        let (cx, cy) = (self.crop.0 as isize, self.crop.1 as isize);
        for y in 0..h {
            let row = unsafe {
                let start = src.offset((cy + y as isize) * pitch + cx * 4);
                std::slice::from_raw_parts(start, w * 4)
            };
            let out = &mut self.frame[y * w * 4..(y + 1) * w * 4];
            if self.rgba_out {
                let opaque = *self
                    .opaque
                    .get_or_insert_with(|| row.as_chunks::<4>().0.iter().all(|p| p[3] == 255));
                out.copy_from_slice(row);
                if !opaque {
                    for px in out.as_chunks_mut::<4>().0 {
                        px[3] = 255;
                    }
                }
            } else {
                for (o, i) in out
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip(row.as_chunks::<4>().0)
                {
                    o[0] = i[2];
                    o[1] = i[1];
                    o[2] = i[0];
                    o[3] = 255;
                }
            }
        }
    }
}

/// The downloading stream as a COM `IStream` for Media Foundation, with one box hidden.
#[implement(IStream)]
struct Source {
    inner: Mutex<HttpStream>,
    len: u64,
    /// Where the edit list's box type is, to read as `free`.
    hide: Option<u64>,
    /// The player is stopping: reads fail from now on.
    cancel: Arc<AtomicBool>,
}

impl ISequentialStream_Impl for Source_Impl {
    fn Read(&self, pv: *mut core::ffi::c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
        if self.cancel.load(Ordering::Relaxed) {
            return STG_E_INVALIDFUNCTION;
        }
        let mut s = self.inner.lock().unwrap();
        let Ok(at) = s.stream_position() else {
            return STG_E_INVALIDFUNCTION;
        };
        // SAFETY: COM hands us a buffer of `cb` bytes.
        let out = unsafe { std::slice::from_raw_parts_mut(pv as *mut u8, cb as usize) };
        let mut n = 0;
        while n < out.len() {
            if self.cancel.load(Ordering::Relaxed) {
                return STG_E_INVALIDFUNCTION;
            }
            match s.read(&mut out[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) => {
                    log::info!("video stream: {e}");
                    return STG_E_INVALIDFUNCTION;
                }
            }
        }
        if let Some(h) = self.hide {
            for (i, b) in b"free".iter().enumerate() {
                if let Some(k) = h
                    .checked_add(i as u64)
                    .and_then(|p| p.checked_sub(at))
                    .filter(|&k| k < n as u64)
                {
                    out[k as usize] = *b;
                }
            }
        }
        if !pcbread.is_null() {
            // SAFETY: a valid out pointer when not null.
            unsafe { *pcbread = n as u32 };
        }
        if n < out.len() { S_FALSE } else { S_OK }
    }

    fn Write(&self, _: *const core::ffi::c_void, _: u32, _: *mut u32) -> HRESULT {
        STG_E_INVALIDFUNCTION
    }
}

impl IStream_Impl for Source_Impl {
    fn Seek(&self, mv: i64, origin: STREAM_SEEK, newpos: *mut u64) -> windows::core::Result<()> {
        let to = match origin {
            STREAM_SEEK_SET => SeekFrom::Start(mv.max(0) as u64),
            STREAM_SEEK_CUR => SeekFrom::Current(mv),
            STREAM_SEEK_END => SeekFrom::End(mv),
            _ => return Err(STG_E_INVALIDFUNCTION.into()),
        };
        let p = self
            .inner
            .lock()
            .unwrap()
            .seek(to)
            .map_err(|_| windows::core::Error::from(STG_E_INVALIDFUNCTION))?;
        if !newpos.is_null() {
            // SAFETY: a valid out pointer when not null.
            unsafe { *newpos = p };
        }
        Ok(())
    }

    fn SetSize(&self, _: u64) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn CopyTo(
        &self,
        _: windows::core::Ref<IStream>,
        _: u64,
        _: *mut u64,
        _: *mut u64,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Commit(&self, _: &STGC) -> windows::core::Result<()> {
        Ok(())
    }

    fn Revert(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Stat(&self, stat: *mut STATSTG, _: &STATFLAG) -> windows::core::Result<()> {
        if stat.is_null() {
            return Err(STG_E_INVALIDFUNCTION.into());
        }
        // SAFETY: a valid out pointer, checked above.
        unsafe {
            *stat = STATSTG {
                r#type: STGTY_STREAM.0 as u32,
                cbSize: self.len,
                grfMode: STGM_READ,
                ..Default::default()
            };
        }
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IStream> {
        Err(E_NOTIMPL.into())
    }
}

/// What `mp4_head` finds in the first track: its edit list's box (where its type is) and how
/// far into the media the presentation starts.
#[derive(Default, Debug)]
struct Head {
    edts: Option<u64>,
    media_time: i64,
    timescale: u64,
    in_trak: bool,
}

impl Head {
    fn offset_ms(&self) -> i64 {
        if self.timescale > 0 && self.media_time > 0 {
            let ms = i128::from(self.media_time) * 1000 / i128::from(self.timescale);
            i64::try_from(ms).unwrap_or(0)
        } else {
            0
        }
    }
}

/// Reads an MP4's `moov` for its first track's edit list and media timescale. A failure to
/// read the stream comes back as the stream's own error; anything else is a format error.
fn mp4_head(r: &mut (impl Read + Seek)) -> Result<Head> {
    let err = |e: std::io::Error| match e.get_ref().and_then(|i| i.downcast_ref::<Error>()) {
        Some(inner) => inner.clone(),
        None => Error::Media(format!("mp4: {e}")),
    };
    let end = r.seek(SeekFrom::End(0)).map_err(err)?;
    let mut head = Head::default();
    walk(r, 0, end, 0, &mut head).map_err(err)?;
    Ok(head)
}

/// Boxes in `start..end` at `depth` (0 = file), following moov > trak > edts > elst and
/// moov > trak > mdia > mdhd. Stops after `moov`.
fn walk(
    r: &mut (impl Read + Seek),
    start: u64,
    end: u64,
    depth: u32,
    h: &mut Head,
) -> std::io::Result<()> {
    let bad = || std::io::Error::other("bad box size");
    let mut at = start;
    while at.checked_add(8).is_some_and(|h| h <= end) {
        r.seek(SeekFrom::Start(at))?;
        let mut b = [0u8; 8];
        r.read_exact(&mut b)?;
        let size = u32::from_be_bytes(b[..4].try_into().unwrap()) as u64;
        let kind: [u8; 4] = b[4..].try_into().unwrap();
        let (body, box_end) = match size {
            1 => {
                let mut l = [0u8; 8];
                r.read_exact(&mut l)?;
                (
                    at + 16,
                    at.checked_add(u64::from_be_bytes(l)).ok_or_else(bad)?,
                )
            }
            0 => (at + 8, end),
            s => (at + 8, at + s),
        };
        if box_end <= at || box_end > end || body > box_end {
            return Err(bad());
        }
        match (&kind, depth) {
            (b"moov", 0) => return walk(r, body, box_end, 1, h),
            (b"trak", 1) if !h.in_trak => {
                h.in_trak = true;
                return walk(r, body, box_end, 2, h);
            }
            (b"edts", 2) => {
                h.edts = Some(at + 4);
                walk(r, body, box_end, 3, h)?;
            }
            (b"mdia", 2) => walk(r, body, box_end, 3, h)?,
            (b"elst", 3) => {
                let mut v = [0u8; 8];
                r.read_exact(&mut v)?;
                let entries = u32::from_be_bytes(v[4..].try_into().unwrap());
                if entries > 0 {
                    if v[0] == 1 {
                        let mut e = [0u8; 16];
                        r.read_exact(&mut e)?;
                        h.media_time = i64::from_be_bytes(e[8..].try_into().unwrap());
                    } else {
                        let mut e = [0u8; 8];
                        r.read_exact(&mut e)?;
                        h.media_time = i32::from_be_bytes(e[4..].try_into().unwrap()) as i64;
                    }
                }
            }
            (b"mdhd", 3) => {
                let mut v = [0u8; 4];
                r.read_exact(&mut v)?;
                let skip = if v[0] == 1 { 16 } else { 8 };
                r.seek(SeekFrom::Current(skip))?;
                let mut t = [0u8; 4];
                r.read_exact(&mut t)?;
                h.timescale = u32::from_be_bytes(t) as u64;
            }
            _ => {}
        }
        at = box_end;
    }
    Ok(())
}

/// A D3D11 device that can decode video, on the default adapter.
fn d3d_device() -> Option<(ID3D11Device, ID3D11DeviceContext)> {
    unsafe {
        let mut dev = None;
        let mut ctx = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut dev),
            None,
            Some(&mut ctx),
        )
        .ok()?;
        let dev: ID3D11Device = dev?;
        // The decoder and our readback share the device from different threads.
        if let Ok(mt) = dev.cast::<ID3D11Multithread>() {
            let _ = mt.SetMultithreadProtected(true);
        }
        Some((dev, ctx?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(kind);
        b.extend_from_slice(body);
        b
    }

    #[test]
    fn finds_the_edit_list() {
        // elst v0: one entry, duration 1000, media_time 512, rate 1.0.
        let mut elst = vec![0, 0, 0, 0, 0, 0, 0, 1];
        elst.extend(1000u32.to_be_bytes());
        elst.extend(512u32.to_be_bytes());
        elst.extend([0, 1, 0, 0]);
        // mdhd v0: created, modified, timescale 12800, duration.
        let mut mdhd = vec![0u8; 12];
        mdhd.extend(12800u32.to_be_bytes());
        mdhd.extend([0u8; 8]);
        let edts = mp4_box(b"edts", &mp4_box(b"elst", &elst));
        let mdia = mp4_box(b"mdia", &mp4_box(b"mdhd", &mdhd));
        let tkhd = mp4_box(b"tkhd", &[0u8; 20]);
        let trak = mp4_box(b"trak", &[tkhd.clone(), edts, mdia].concat());
        let moov = mp4_box(b"moov", &[mp4_box(b"mvhd", &[0u8; 100]), trak].concat());
        let ftyp = mp4_box(b"ftyp", b"dash0000");
        let file = [ftyp.clone(), moov, mp4_box(b"mdat", &[0u8; 64])].concat();
        let h = mp4_head(&mut std::io::Cursor::new(&file)).unwrap();
        let at = h.edts.unwrap() as usize;
        assert_eq!(&file[at..at + 4], b"edts");
        assert_eq!(h.offset_ms(), 40);
        // No edit list: nothing hidden, no offset.
        let plain = [ftyp, mp4_box(b"moov", &mp4_box(b"trak", &tkhd))].concat();
        let h = mp4_head(&mut std::io::Cursor::new(&plain)).unwrap();
        assert!(h.edts.is_none());
        assert_eq!(h.offset_ms(), 0);
    }

    #[test]
    fn hostile_box_sizes_fail_cleanly() {
        let ftyp = mp4_box(b"ftyp", b"dash0000");
        // A 64-bit size that would wrap around.
        let mut huge = vec![0, 0, 0, 1];
        huge.extend_from_slice(b"moov");
        huge.extend_from_slice(&u64::MAX.to_be_bytes());
        let file = [ftyp.clone(), huge].concat();
        assert!(matches!(
            mp4_head(&mut std::io::Cursor::new(&file)),
            Err(Error::Media(_))
        ));
        // A 64-bit size smaller than its own header.
        let mut tiny = vec![0, 0, 0, 1];
        tiny.extend_from_slice(b"moov");
        tiny.extend_from_slice(&9u64.to_be_bytes());
        let file = [ftyp, tiny, vec![0; 16]].concat();
        assert!(mp4_head(&mut std::io::Cursor::new(&file)).is_err());
        // Edit-list and timescale values at their extremes.
        let h = Head {
            media_time: i64::MAX,
            timescale: 1,
            ..Default::default()
        };
        assert_eq!(h.offset_ms(), 0);
        let h = Head {
            media_time: i64::MAX,
            timescale: u64::MAX,
            ..Default::default()
        };
        assert_eq!(h.offset_ms(), 499);
    }

    #[test]
    fn stream_failures_are_told_apart_from_format_ones() {
        struct Failing;
        impl Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(Error::Net("connection reset".into()).into())
            }
        }
        impl Seek for Failing {
            fn seek(&mut self, _: SeekFrom) -> std::io::Result<u64> {
                Ok(64)
            }
        }
        let e = mp4_head(&mut Failing).unwrap_err();
        assert!(matches!(e, Error::Net(_)), "{e}");
        assert!(stream_error(&e));
        assert!(!stream_error(&Error::Media("no decoder".into())));
    }
}

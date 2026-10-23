# Implementation plan: minimum product

Status: draft, 2026-10-23. Scope is the **minimum product only**; nothing here is built for later
features. PLAN.md is the product vision. This file is *how* we build the first version.

---

## 1. What the minimum product is

**In:**

| Area | Included |
|---|---|
| Playback | YouTube Music streaming, gapless, 10-band EQ, volume, seek, shuffle, repeat |
| Account | Sign in once (Google, via a temporary login window), sign out |
| Navigation | Sidebar: Search, Home, Explore, Radio (button only, "coming soon"); Library: Recently played, Artists, Albums, Songs; Playlists: All playlists, Liked songs, your playlists |
| Pages | Home, Explore, Search results, Album, Artist, Playlist, the four Library pages, All playlists |
| Actions | Play, play next, add to queue, like/unlike, add to playlist, new playlist, start radio, go to album/artist |
| Now playing | Bottom bar; right panel with Lyrics / Queue (reorder, remove) |
| Lyrics | Synced: Unison → YouTube Music timed → LRCLIB; active line highlighted, word fill when the source has word timing; click a line to seek |
| Look | Datacentre palette (dark), Inter, immersive background (album colours through warm glass) |
| Artwork | Apple editorial album banners (4320×1080) with the fanart.tv artist logo; animated album covers (Apple motion art); artist logos on artist pages; cover-built banner when Apple has nothing |
| Listen Along | Private P2P sessions with friends via invite link; shared queue with song requests; every song starts within 10 ms on every machine |
| Windows | Media keys + Windows media flyout, taskbar previous/play/next buttons, light when minimized, game-aware priority |
| Hygiene | No ads or promoted shelves; your plays are reported to YouTube so recommendations learn |

**Out (not in this version):**
- The fullscreen stage.
- The light theme.
- Live radio, Automix and crossfade.
- Discord presence.
- Unreleased-song search.
- Settings beyond EQ, volume and sign-out.

---

## 2. Architecture

One process, two crates:

```
music/
├─ core/   library, no UI dependencies, unit-testable
│   ├─ audio/     http stream → demux → decode → resample → ring → output (+ EQ)
│   ├─ yt/        InnerTube client: auth, resolver, browse/search/next, parsers, actions
│   ├─ lyrics/    Unison, YouTube timed lyrics, LRCLIB; TTML/LRC parsing
│   ├─ art/       Apple (anonymous token, banners, motion clips), fanart.tv logos, MusicBrainz ids
│   ├─ together/  Listen Along: iroh transport, clock sync, scheduled actions, protocol
│   ├─ player.rs  queue, playback state machine, gapless hand-off, persistence
│   ├─ cache.rs   disk cache (images, JSON with TTL)
│   └─ net.rs     shared HTTP agent + small worker pool
└─ app/    binary: Slint UI, Windows integration, image pipeline, immersive background,
            motion-art player (Media Foundation)
```

**Why:**
- The core can be tested without a window.
- UI rebuilds don't recompile the audio stack.
- The boundary is one command channel in and one event channel out.

### Threads (no async runtime)

Everything is blocking I/O on a few threads. No tokio in the app itself: it would add weight and
gain nothing at this scale. The one exception is Listen Along's transport (iroh needs tokio): its
runtime is created when a session starts and dropped when it ends, so it costs nothing otherwise.

| Thread | Priority | Job | Idle behaviour |
|---|---|---|---|
| UI | normal | Slint event loop | sleeps until input or event |
| audio | MMCSS "Pro Audio", excluded from EcoQoS | cpal/WASAPI render callback: ring → EQ → volume → device | stream stopped after 5 s paused |
| decoder | below normal | demux, decode, trim, resample, fill ring; gapless hand-off | blocks on ring space / commands |
| stream fetch (1 per open track, max 2) | below normal | HTTP range download into memory | exits when the file is complete |
| net workers ×3 | below normal | API calls, images, lyrics (LIFO queue so visible things load first) | park when queue empty |
| motion art | below normal | Media Foundation hardware decode of one looping clip → Slint image | only exists while a motion cover is on screen |
| together (tokio, 1 worker) | normal | Listen Along transport and clock probes | only exists during a session |
| win hooks | — | foreground-change hook for game detection (event-driven, no polling) | sleeps |

Communication: `crossbeam-channel` for commands and events, `rtrb` SPSC ring for audio, and
atomics for position and levels.

---

## 3. Audio engine (performance, speed and quality first)

### 3.1 Format choice
- Prefer **itag 251** (Opus, ~160 kbps, 48 kHz). It's the best signed-out quality and matches the
  usual Windows device rate, so in the common case there's **no resampling**.
- Fallbacks: 250 (Opus, lower bitrate), then 140 (AAC 128k, 44.1 kHz; resampled).

### 3.2 Streaming: `HttpStream`
- A fetch thread downloads the file in **1 MiB range requests** into a growable in-memory buffer:
  3–5 MB per song, freed when the song ends.
- Readers block on a condvar until their byte range has arrived. Seeking past the downloaded part
  re-prioritises the fetch at the new offset.
- **Head start:** the first request is 256 KiB, so the first audio starts in about 0.2 s, as
  measured in spike B.
- **Expired link (403):** re-resolve once, then continue from the same byte offset.
- **Why not stream straight into the decoder:** a fully buffered song can't stutter on a network
  hiccup, and seeking is instant.

### 3.3 Demux
- **WebM/Opus: our own minimal EBML/WebM parser (~300 lines)** instead of symphonia's mkv.
- Reason: sample-exact gapless needs `CodecPrivate` (OpusHead pre-skip), `DiscardPadding` on the
  final block (end trim; symphonia ignores it, as spike B found), and `Cues` for fast, exact
  seeks. That's a small, fully understood surface.
- **MP4/AAC (fallback only):** symphonia 0.6 `isomp4` + `aac`, plus our own handling of the
  `elst` edit list / iTunSMPB priming (spike B saw 1600 frames not removed).

### 3.4 Decode
- **Opus:** `rusty-opus` 1.0.2. Measured float-exact against libopus, 370× realtime.
  - We patch or wrap it so output **isn't clamped to ±1.0**, keeping headroom for EQ.
  - Pre-skip is applied at the start, DiscardPadding at the end.
  - Seeks decode an 80 ms pre-roll and discard it (per RFC 7845).
- **AAC:** symphonia.
- Output is always **f32 interleaved**.

### 3.5 Resample (only when rates differ)
- `rubato` 5, synchronous sinc resampler with a long filter (quality over speed; still well under
  1% of a core), streamed in fixed blocks.
- Its fixed delay is compensated so gapless and positions stay exact.

### 3.6 Ring and output
- **Ring:** an `rtrb` SPSC f32 ring holding ~1 s (~384 KB at 48 kHz stereo).
- **Output:** cpal 0.18 → WASAPI shared mode at the device's mix rate.
- **Render callback** (wait-free; no locks, allocation or logging):
  1. Pop frames.
  2. Run the EQ, skipped entirely when flat.
  3. Apply volume, skipped at 100%.
  4. Write to the device.
- **Bit-perfect** when the EQ is flat, volume is 100% and the source rate equals the device rate.
- **Position:** the callback adds the frames it consumes to an atomic counter. The decoder records
  each track's start as an entry `(track_id, ring_frame_index)` in a boundary list on the control
  side. The UI derives the current track and position from these with no callback involvement.
- **Seek, or change track manually:** the decoder takes the consumer lock briefly to drain the
  ring (the callback's `try_lock` misses for one period and outputs silence), then refills.
  The audible gap is ≤ 20 ms.
- **Pause:** the callback outputs silence without consuming. After 5 s paused the stream is
  stopped (0% CPU), and it restarts on play.
- **Device change** (headphones plugged in): rebuild the output, keeping the ring.

### 3.7 Gapless
- As soon as a track starts, the player resolves the **next** track. The decoder opens it about
  30 s before the current one ends: it starts the fetch, parses the headers and primes the
  decoder.
- At end of stream, the decoder writes the next track's first sample straight after the last
  trimmed sample of the current one, into the same ring. There's no gap, no fade and no silence.
- If the next track isn't ready in time (a network failure), playback waits for it, then starts.
  That's the only time a gap can happen.

### 3.8 EQ
- 10 bands (31, 62, 125, 250, 500, 1k, 2k, 4k, 8k, 16k Hz), ±12 dB, plus a preamp.
- **Filters:** RBJ peaking biquads, with low/high shelf at the ends. Transposed Direct Form II in
  f32 with f64 coefficient design.
- **Changing settings:** the UI designs the coefficients and sends them through a tiny lock-free
  channel. The callback crossfades the old and new filter output over one period, so moving a
  slider never clicks.
- **Clipping protection:** an automatic preamp of −(max positive gain) by default.
- Presets: Flat, Bass, Vocal, Treble, plus custom. Stored in settings.

---

## 4. YouTube (InnerTube) client

### 4.1 Transport
- One `ureq` 3 agent (rustls, connection reuse, gzip), shared by everything.
- Requests are JSON POSTs to `/youtubei/v1/{browse,search,next,player,like/like,...}`.

### 4.2 Auth (sign-in)
- **Sign in** opens a small **WebView2 window** (`webview2-com`, not wry, to keep it small) at
  `accounts.google.com`, which redirects to `music.youtube.com`.
- When the YouTube Music page loads, we read its cookies through `ICoreWebView2CookieManager`.
  Then the WebView is **destroyed**: none of the browser runtime stays loaded.
- Cookies are stored **encrypted with Windows DPAPI** (`CryptProtectData`, current user) in
  `%APPDATA%\…\session.bin`.
- **Authenticated requests:**
  - `Cookie` header.
  - `Authorization: SAPISIDHASH <ts>_<sha1(ts + " " + __Secure-3PAPISID + " " + origin)>`
    (the ytmusicapi scheme, MIT).
  - `X-Goog-AuthUser: 0`.
- **Sign out** deletes the file.

### 4.3 Stream resolver
- The `visionos` client with a cached `visitorData` (from `visitor_id`; refreshed daily or on
  failure) calls `/player` and returns plain URLs (no JS, no token).
- **Validation:** check `playabilityStatus`. Pick itag 251 > 250 > 140.
- **URL cache:** in memory and on disk, keyed by videoId, honouring `expiresInSeconds` minus
  30 minutes.
- **Health:** a 403 *after* the first MiB marks the client broken and shows "Streaming
  temporarily unavailable". There's no other fallback in this version.

### 4.4 Data
These are the WEB_REMIX endpoints, signed in when available. Parsers are ported from ytmusicapi
(MIT) to Rust and kept tolerant: missing fields become `None`, never a crash.

| Feature | Endpoint |
|---|---|
| Home | `browse FEmusic_home` (+ continuations) |
| Explore | `browse FEmusic_explore`, `FEmusic_new_releases`, `FEmusic_charts`, `FEmusic_moods_and_genres` |
| Search | `search` (+ `music/get_search_suggestions`), filtered tabs: songs, albums, artists, playlists |
| Album | `browse MPRE…` |
| Artist | `browse UC…` (header photo, monthly audience, shelves) |
| Playlist | `browse VL…` (+ continuations) |
| Library | `FEmusic_history`, `FEmusic_library_corpus_artists` (+ `corpus_track_artists`), `FEmusic_liked_albums`, `FEmusic_liked_videos`, `FEmusic_liked_playlists` |
| Liked songs | `browse VLLM` |
| Radio / up next | `next` with `RDAMVM<videoId>` (+ continuations) |
| Lyrics (YouTube) | `next` → lyrics tab id → `browse` with ANDROID_MUSIC client (`timedLyricsModel`) |
| Like / unlike | `like/like`, `like/removelike` |
| Playlists | `playlist/create`, `browse/edit_playlist` (add / remove) |
| History reporting | WEB_REMIX `/player` (signed in) → `playbackTracking.videostatsPlaybackUrl` → GET with a fresh `cpn` when a song has played ≥ 30 s |

- **Ads and promotions** are dropped in the parsers: `adSlotRenderer`, promoted badges, upsell
  shelves and banners, and ad fields in player responses.
- **Fixtures:** recorded JSON responses (`core/tests/fixtures/`) drive parser unit tests, so a
  YouTube layout change shows up as a failing test with the exact renderer that moved.

### 4.5 Model
Plain Rust structs, shared by the core and the UI:
- `Track` (videoId, title, artists, album, duration, thumbnails, explicit, liked)
- `Album`, `Artist`, `Playlist`
- `Shelf` (title + items) and `Page` (header + shelves)

---

## 5. Lyrics
- **Order:**
  1. **Unison** `GET /lyrics?v=&song=&artist=&duration=`: TTML, syllable/word/line.
  2. **YouTube Music timed lyrics:** line.
  3. **LRCLIB** `/api/get`, then `/api/search` ranked by duration ±3 s: line or word.
- **Parsing:** a minimal hand-written TTML parser (`<p begin end>`, `<span begin end>`, `ttm:agent`
  for duets, background `x-bg`) and an LRC parser, both producing
  `Lyrics { lines: [{start, end, words: [{start, end, text}], agent, background}] }`.
- **Cache:** one JSON file per videoId. Misses are cached for 7 days.
- **Rendering cost:** a binary search per frame on the UI thread, and only while the lyrics panel
  is open and the window is visible.

---

## 6. Artwork: Apple banners, motion covers, artist logos

None of this needs an account or a key from the user. If Apple ever requires a login, the Apple
part is removed rather than asking for one.

### 6.1 Apple (anonymous)
- **Token:** fetch `music.apple.com/us/browse`, follow its JS bundles and pick the JWT whose
  payload has `iss: "AMPWebPlay"` (header order varies; spike C's scraper handles it). Cached on
  disk until its `exp`; re-scraped on a 401.
- **Matching** a YouTube album to Apple:
  1. `catalog/{storefront}/search?types=albums&term=<artist> <album>`.
  2. Score candidates: normalised title (strip "Deluxe", "Remastered", brackets), artist, track
     count, year. Prefer the exact title over deluxe editions.
  3. Accept above a threshold; otherwise "no match". Results, including misses, are cached
     (misses for 30 days).
- **Fetch:** `catalog/{sf}/albums/{id}?extend=editorialVideo,editorialArtwork`.
  - **Banner:** `editorialArtwork.storeFlowcase`, else `subscriptionHero` (4320×1080), requested
    at window width × DPI through Apple's `{w}x{h}` URL template.
  - **Motion cover:** `editorialVideo.motionSquareVideo1x1` (or `motionDetailSquare`) HLS
    playlist → the **768² AVC** rung. Never HEVC.
  - **Artist logo fallback:** `musicContentColorLogoTrimmed` on the artist resource.
- **Storefront:** taken from the system region, defaulting to `us`.

### 6.2 Motion covers
- **Where they play:** on the album page, in place of the crisp square cover on the banner. Only
  while that page is visible, the window is focused and no game is running; otherwise the static
  cover is shown.
- **Download:** the HLS segments of the 768² rung are fetched once and stored as one MP4 file
  (5–10 MB) in a disk cache with a 500 MB LRU cap.
- **Decode:** a Media Foundation Source Reader with hardware decode (DXVA), output NV12 → RGBA,
  looped. Measured in spike C at 0.13–0.44% CPU. Frames go into a reused Slint
  `SharedPixelBuffer` at 30 fps.
- **Lifetime:** the player thread and its D3D device are created when a motion cover appears and
  destroyed when it leaves the screen or the window is minimized.
- **Setting:** "Animated covers: On / Off" (default On).

### 6.3 Artist logos (fanart.tv)
- **Key:** our fanart.tv project key, read from the gitignored `secrets.toml` at build time
  (`build.rs` → `env!`). Users never need one.
- **Lookup:** MusicBrainz `ws/2/artist?query=artist:"<name>"&fmt=json` (one request at most per
  second, with a proper User-Agent), choose the best-scoring match, then fanart.tv
  `v3/music/<mbid>` → `hdmusiclogo` (highest likes first). Cached forever, including misses.
- **Fallback:** Apple's `musicContentColorLogoTrimmed`. No logo means no logo: no text in its
  place.
- **Where:** bottom-left of the album banner and over the artist page header.

### 6.4 Album banner
- **Apple banner present:** it fills the full width at ~40% of the window height, the logo sits
  bottom-left, and the bottom edge fades into the immersive background. The background's palette
  is taken from the banner.
- **No Apple banner:** the cover-built banner. The cover is scaled to the full width, blurred
  **once** on a worker (box blur ×3 on a downscaled copy), darkened, with vertical blind ribbing;
  the crisp (or animated) cover sits on the right.
- **No text on the banner.** The title block sits below it.

---

## 7. Listen Along

Friends only, by invite link. It's a fresh, trimmed rewrite: everyone plays the same YouTube
stream, so there's no cross-provider matching.

### 7.1 Transport and joining
- **iroh** (QUIC with hole punching, relay fallback). The endpoint exists only during a session.
- **Invite:** the host's node ticket in a link, `<app>://join/<ticket>`. The scheme is registered
  per user under `HKCU\Software\Classes` on first run.
- **Single instance:** a named pipe. Opening a link while the app runs forwards it to the running
  instance, which shows a "Join ⟨host⟩?" prompt.

### 7.2 Protocol
Versioned, length-prefixed `serde` messages on one bidirectional stream per participant:
- `Hello { name, version }`, `Bye`
- `Clock { t1, t2, t3 }` (probes)
- `Schedule { action, video_id, itag, frame, at_session_time }`, where `action` is play, pause,
  seek or next-song
- `Queue { items: [{ video_id, requested_by }] }`
- `Request { video_id, by }`: goes straight into the host's queue
- `Report { offset_us }`: each client's measured error, shown in the UI

### 7.3 Sync (within 10 ms at every song start; nothing during a song)
1. **Same bits:** the host picks the itag (251, falling back to 140 for everyone). Positions are
   decoded 48 kHz sample frames after pre-skip, so "frame N" is identical audio everywhere.
2. **Shared clock:** QueryPerformanceCounter as the time base. NTP-style probes (5/s for 2 s,
   then every 2 s); keep the lowest-RTT ~10% of samples; fit offset and skew by linear
   regression.
3. **Scheduled actions:** every play, pause, seek, skip and song change is broadcast as "frame F
   of V plays at session time T" with T ≈ now + 400 ms. Everyone, including the host, executes it
   at T. The next song's start is scheduled from the shared queue, so it's prefetched and
   gapless on every machine.
4. **Hitting T in the audio engine:**
   - The render callback reads cpal's playback timestamp for each buffer, giving a frame → QPC
     mapping (±1–2 ms).
   - A scheduled start is a pending marker in the callback: in the buffer that contains T, it
     outputs exactly the right number of silent frames before the first sample. Sample-accurate,
     no resampling, still bit-perfect.
   - The resampler's fixed delay is compensated when the device isn't 48 kHz.
5. **No mid-song correction.** Drift (2–12 ms per song) is cleared at the next song start. Only
   if a participant is > 100 ms off (a stall, sleep, device change) does it re-join at the right
   position with a short fade.

### 7.4 UI
- The Listen Along button in the now-playing bar opens a popover: start session / copy invite
  link, participants with their "± ms" figure, leave.
- Listeners see a "Listening with ⟨host⟩" chip. Listeners' transport buttons send requests to the
  host instead of acting locally.
- The queue panel shows who requested each song.

### 7.5 Verification
- **Automated:** host and listener on one PC, capture both outputs with WASAPI loopback,
  cross-correlate, assert |offset| ≤ 10 ms at every song start, seek, resume and skip over a
  10-minute session. A second run adds artificial jitter and path asymmetry.

---

## 8. UI (Slint 1.18, FemtoVG/OpenGL)

FemtoVG is the renderer measured at 54 MB open, 1.6% CPU animating, and 0% idle.

### 8.1 Structure

```
AppWindow
├─ Background (immersive)   6 tinted sprites + darkness + vignette + facade lines
├─ Sidebar                  Apple Music style, datacentre palette
├─ Content                  router stack with back/forward; one page component per route
├─ RightPanel (optional)    Lyrics | Queue; only instantiated when open
└─ NowPlayingBar
```

- **Pages are Slint components fed by models.** The Rust side owns a `Router` (route stack plus
  page data cache). Navigating fetches on a net worker and swaps the model when it arrives;
  pages show skeleton rows while loading.
- **Long lists** (playlists, library songs) use `ListView`, so only visible rows exist. More rows
  load at the end of the list via continuations.
- **Theme:** one `Theme` global with the six palette tokens. Amber "light" uses a pre-rendered
  glow sprite or drop shadow, never a per-frame blur.
- **Font:** Inter (variable, OFL) embedded with `import "Inter.ttf"`.

### 8.2 Images
- **Fetch:** sized per use. Google thumbnails take a `=wN-hN` suffix, so we request exactly the
  display size × DPI.
- **Disk cache:** `%LOCALAPPDATA%\…\img\` (SHA-1 of URL), LRU-capped at 300 MB.
- **Decode:** `image` (jpeg, png, webp) on a net worker. Visible images first (LIFO), and stale
  requests are dropped when you scroll past.
- **Memory:** an in-memory texture LRU of ~200 images, **dropped entirely on minimize**.

### 8.3 Immersive background
- **Per page:** the colour source is the album/playlist/artist art on those pages, and the
  now-playing track elsewhere.
- **Palette:** k-means (k=5) on a 64×64 thumbnail, on a net worker, about 1 ms. Each colour is
  pushed ~35% towards tungsten and dimmed slightly ("warm glass").
- **Drawing:** 6 `Image`s of one 128×128 soft radial sprite, `colorize`d, moved by a 30 fps timer.
  Then the darkness layer (40%), vignette and static facade lines.
- **The timer runs only** when the window is visible, focused and no game is detected. Otherwise
  it shows a frozen frame.
- **Palette change:** a 1.2 s crossfade (colour lerp in the same timer).

---

## 9. Windows integration
- **Media controls:** `SystemMediaTransportControls` via the `windows` crate, using
  `ISystemMediaTransportControlsInterop::GetForWindow` on our HWND. Title, artist and artwork
  (from the URL) are shown, and play, pause, next and previous are handled.
- **Taskbar buttons:** `ITaskbarList3::ThumbBarAddButtons` (prev / play-pause / next).
  - The window is subclassed (`SetWindowSubclass`) to receive their `WM_COMMAND`, plus
    `WM_SIZE` (minimize/restore) and `WM_ACTIVATE`.
- **On minimize:**
  1. Stop the background timer.
  2. Drop the texture and image caches and page models other than the current one.
  3. `EmptyWorkingSet`.

  On restore, the current page re-requests its images from the disk cache.
- **Game mode:**
  - A `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` check for fullscreen exclusive or borderless
    triggers it. It's event-driven, with no polling.
  - It turns on process EcoQoS and below-normal priority. The **audio thread opts out** with
    `SetThreadInformation(ThreadPowerThrottling, off)` plus MMCSS, so playback never starves.

---

## 10. Persistence

| File | Where | Content |
|---|---|---|
| `settings.json` | `%APPDATA%\<app>\` | volume, EQ (bands, preset, on/off), window size, sidebar state |
| `session.bin` | `%APPDATA%\<app>\` | DPAPI-encrypted cookies |
| `queue.json` | `%APPDATA%\<app>\` | queue, index, position, shuffle/repeat; saved on change, debounced |
| `cache\` | `%LOCALAPPDATA%\<app>\` | images (LRU 300 MB), lyrics, visitorData, stream URLs, API pages (short TTL), Apple token and matches, MusicBrainz ids, logo lookups |
| `motion\` | `%LOCALAPPDATA%\<app>\` | motion-cover MP4s (LRU 500 MB) |
| `secrets.toml` | repo root, gitignored | fanart.tv project key, compiled in at build time; never committed |
| `app.log` | `%LOCALAPPDATA%\<app>\` | rotating log, previous run kept |

---

## 11. Dependencies (deliberately few)

- **core:** `ureq` 3, `serde` + `serde_json`, `rusty-opus` 1.0.2,
  `symphonia` 0.6.1 (isomp4 + aac only), `rubato` 5, `cpal` 0.18, `rtrb`, `crossbeam-channel`,
  `sha1` (SAPISIDHASH), `log`, `iroh` (Listen Along only; brings tokio, used only during a
  session).
- **app:** `slint` 1.18 (backend-winit, renderer-femtovg only), `image` (jpeg, png, webp),
  `windows` 0.62 (media controls, taskbar, DPAPI, hooks, process info, WebView2 interop,
  Media Foundation + D3D11 for motion covers, named pipes),
  `webview2-com` (sign-in window only).
- **Not used:** tokio outside Listen Along, wry, souvlaki, env_logger (a 30-line file logger instead), yt-dlp/Node.

---

## 12. Build order (each step is runnable and verified before the next)

1. **Audio core plus CLI.**
   - `HttpStream`, WebM demux, Opus/AAC decode, ring, output, EQ, gapless, resolver.
   - CLI: `music-cli play <videoId>…`.
   - Verify:
     - Gapless: decode an album with continuous tracks; there's no discontinuity at the
       boundaries, compared with ffmpeg's concatenated decode.
     - A flat EQ is bit-exact.
     - Start latency is under 0.4 s.
     - CPU per audio-minute.
2. **InnerTube client.**
   - Models, parsers with fixtures, sign-in window, library, actions, history reporting.
   - Verify: parser tests, and a signed-in fetch of every endpoint in §4.4.
3. **UI shell.**
   - Window, theme, Inter, sidebar, router, now-playing bar, queue panel, EQ panel, media
     controls, taskbar buttons, minimize trim.
   - Verify: memory and CPU targets with the measure script from spike A.
4. **Pages.** Home, Explore, Search, Album (with banner), Artist, Playlist, the Library pages and
   All playlists.
5. **Lyrics panel and immersive background.**
6. **Artwork.** Apple token, matching, banners; fanart.tv logos; motion-cover player.
   - Verify: match rate on a sample of 50 library albums; motion cover CPU ≤ 0.5%; nothing
     decoding while minimized.
7. **Listen Along.** Transport, invite links, clock sync, scheduled starts, UI.
   - Verify: the loopback cross-correlation test (≤ 10 ms at every song start).
8. **Hardening.**
   - Error states (offline, expired session, unplayable track, broken resolver), game mode, and a
     long-run soak (8 h playback, memory flat).
   - Verify the final measurements against the targets.

### Targets the minimum product must meet

| Situation | RAM (private working set) | CPU | GPU |
|---|---|---|---|
| Minimized, playing | ≤ 20 MB | ≤ 0.5% | 0% |
| Minimized, paused | ≤ 10 MB | 0% | 0% |
| Open, idle | ≤ 120 MB | 0% | 0% |
| Open, background animating | ≤ 140 MB | ≤ 2% | ≤ 6% |
| Click play → first audio | ≤ 0.4 s (cached URL) / ≤ 0.6 s (new) | | |
| Track change (gapless) | 0 samples of gap | | |
| Album page with motion cover | +≤ 80 MB while on screen | ≤ 0.5% extra | video engine only |
| Listen Along | every song start within 10 ms across participants | | |

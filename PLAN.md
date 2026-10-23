# Music app v2: product and technical plan

Status: draft for review (2026-10-23). The legacy app is tagged `legacy-v1`, with backups in
`Documents\Music-backup-2026-10-23`.

---

## 1. What the app is

A YouTube Music player for Windows, written in Rust. It has two modes:

- **Background mode (minimized):** you press play, minimize the window, and play a game.
  The app costs effectively nothing.
- **Open mode:** a dark, glassy, colour-immersive interface that feels like an Apple app. It has
  a fullscreen "stage" for listening with lyrics and motion artwork.

YouTube does all the discovery: Home, radio, "up next", related, moods and charts. The app adds
no recommendation logic of its own.

---

## 2. Requirements, expanded and challenged

### R1: Cost while minimized ≤ legacy (90.3 MB, 0% CPU, 0% GPU)

**Targets, measured in Task Manager's "Memory" column (private working set) across all of the
app's processes:**

| State | RAM | CPU | GPU |
|---|---|---|---|
| **Minimized, playing** | **≤ 20 MB** | ≤ 0.5% | 0% |
| Minimized, paused | ≤ 10 MB | 0% | 0% |
| Open, idle | ≤ 150 MB | 0% | 0% (no redraw) |
| Open, immersive animating | ≤ 180 MB | ≤ 2% | ≤ 5% on an iGPU |

**How we get there:**

- **No WebView at all.** Legacy hosted Apple Music's MusicKit in WebView2. Those child processes
  probably weren't fully counted in the 90.3 MB figure, so the real legacy cost was likely higher.
- **Core and UI are separate.** A headless core (playback, queue, YouTube client, cache) owns all
  state. The UI is a view of that state that can be **destroyed**.
- *(Superseded: see 5b "one process, minimize to the taskbar".)* **Closing to tray drops the whole window.** That releases the UI component, the renderer, the
  GPU device, textures, fonts and the image cache. Only the audio threads and one network runtime
  stay alive.
- **Minimize to taskbar** must keep a window, so instead we free the textures and image cache and
  stop all timers.

**Challenge.** It hasn't been verified that Slint actually returns that memory when a window is
destroyed. A 2024 report says it didn't. That is risk #1, so **Spike A** measures it before we
commit to Slint (see §6).

**Challenge: stream resolving.** yt-dlp needs Deno. Spawning both for every song costs about
150 MB for 1–2 s and ships about 100 MB on disk.

- **Fix:** resolve streams natively in Rust as the fast path (InnerTube with whichever no-token
  client currently works). Fall back to yt-dlp only when the fast path fails.
- When yt-dlp is needed, resolve the next 2–3 songs in a single spawn, at low priority and with
  EcoQoS, well before they're needed.
- Stream URLs live for about 6 h and are cached.

### R2: Beautiful and animated, but performance first

The rules carried over from legacy, because they worked:

- Redraw only on change. There is no free-running render loop.
- Animations request frames only while they run.
- Never animate font sizes.
- Reduce-motion and game mode snap every animation to its end state.

**Challenges and alternatives:**

- **Real-time backdrop blur ("glass" over moving content)** is the classic performance killer:
  every frame becomes a multi-pass blur of the whole window.
  - **Alternative (recommended):** glass panels are a translucent tint over the *pre-blurred*
    artwork background, plus a 1-px top edge highlight, an inner gradient and a faint noise
    texture. That looks 95% the same at about 0 cost.
  - Slint's real backdrop-blur (1.19, Skia only) is reserved for at most one small element, such
    as the player bar, and only if Spike A shows it's cheap.
- **GPU renderers redraw the whole window per frame**, so an animated background means a
  full-window redraw at the animation rate.
  - The background animates at a **capped 30 fps**.
  - It runs only while the window is focused and visible and no game is running.
  - **The immersive background is on every page**: Home, search, artist, album, playlist,
    Now Playing and fullscreen. It's animated everywhere by default.
  - A setting covers slower machines: "Background motion: Everywhere / Now Playing & fullscreen
    only / Off (static, still colour-immersive)".
- **Motion artwork** is video decoding: a real but small cost. See R9.

### R3: YouTube's algorithm only

These come from InnerTube (WEB_REMIX):

- **Home** (`FEmusic_home`)
- **Radio / up next** (`next` with `RDAMVM<id>`)
- **Related**, **moods & genres**, **charts**, **new releases**
- **Search** with suggestions

**Challenge: signed-out YouTube barely personalises.** For "the power of the algorithm" you need
to **sign in**, and the app must **report what you play** back to YouTube (the playback-tracking
pings the official client sends). Otherwise YouTube never learns your taste.

- **Sign-in:** a one-time WebView2 login window. It captures the cookies and is then destroyed, so
  there is no ongoing cost.
- **Premium:** if your account has Premium, you also get Opus at ~256 kbps.

**Dropped:** every custom recommender from legacy: smart, vibe, recommend, radar, dj, blend,
discover and sound-search.

**No ads, by construction (2026-10-23).**
- Ads only exist inside YouTube's own player. We never run it: we request the song's audio stream
  directly and play it in our engine, so no audio or video ads are ever fetched or played. That
  includes Listen Along, and works on a free account.
- **Ad-tracking calls:** the parser drops the ad and ad-tracking fields from `/player` responses,
  and we never call ad or ad-tracking URLs. The only reporting we do is the playback-history ping
  that teaches YouTube your taste (R3), and that one is wanted.
- **Promoted content in browse responses:** items and shelves marked as promoted or sponsored
  (e.g. `adSlotRenderer`, promoted badges, "Upgrade to Premium" upsell shelves and banners) are
  filtered out when we parse Home, Explore and search, so they never reach the UI.
- **Sign-in window:** the one-time WebView2 login runs with a request filter that blocks known
  ad/tracking hosts. It closes as soon as the cookies are captured.
- **What it can't remove:** sponsorships an artist puts *inside* a song or music video, since
  they're part of the audio.

### R4: Immersive background v2

**Legacy:** a 48×48 pre-blurred cover drawn as 3 drifting textured quads, music-reactive through
a ≤20 Hz meter. It was cheap and effective, and we keep that core idea.

**Look: the background is a wall, lit by coloured fixtures just out of view** *(2026-10-23,
replaces the free-floating blobs)*.

- **Base:** the dark page colour (#140F0D), the same on every page and for every song: the wall
  itself never changes, only the lights falling on it. A darkness layer and a vignette sit over
  everything.
- **Lights:** 5 sources, each one of the cover's palette colours, placed just past the window
  edges (two along the top, one at the bottom, one each side). Each throws a soft **cone** across
  the wall: brightest where it enters, fading as it travels. Lights mix additively where they
  overlap.
- **Bounce:** each light also lays a much wider, dimmer wash, ~40% desaturated, centred about
  halfway along its cone: the colour reflected back off the room.
- **The wall's surface:** a faint concrete grain and thin panel seams are *multiplied* over the
  light, so they only show where light lands and vanish in the dark. Seams get a 1 px warm
  highlight on one side, as if catching the light.
- **Motion:** the fixtures stay put; their cones sway slowly (±0.18 rad) and their brightness
  breathes (±15%). Nothing floats. Music-reactive pulses add to the brightness.
- **Tuned defaults (from the mockup, 2026-10-23):**
  - Light strength 60%.
  - Bounce 45%.
  - Cone width 55% (cone breadth relative to its length).
  - Wall texture 40%, panel seams 35%.
  - Darkness 30%.
  - Motion 0.3×.

  Mockup units: a ~680×400 stage with cones 1.25× the stage height long, so in the app they
  scale with the window.

**How it's drawn (cheap, no shaders; Slint has no custom shaders anyway):**

- **Two shared sprites**, generated once at startup: a soft cone (a radial falloff squashed
  into a half-ellipse) and a large soft disc for the bounce. Each light is the cone sprite
  tinted, rotated and scaled, plus the disc tinted with its desaturated colour.
- **Wall texture:** one static greyscale image (grain + seams) at window size, regenerated only
  on resize, drawn with multiply-style darkening over the lights.
- **Per frame:** 5 cone quads + 5 bounce quads + 1 texture quad + 1 darkness quad + 1 vignette
  quad. Negligible GPU work even at 1440p.
- **Palette:** 4–6 colours extracted once per track on a worker (k-means on a 64×64 thumbnail).
  Darkness/saturation are clamped so white text is always legible.
- **Off / reduced motion:** the same picture, frozen.

**Incorporated into the UI, not just behind it.** Every surface reads the live palette and the
light positions:

1. **Glass panels pick up nearby colour.**
   - The edge/border gradient of each panel (sidebar, cards, player bar) is tinted by the lights
     nearest it.
   - Panels look lit by the background, the way glass catches coloured light.
   - Updated at a low rate (≈ 4 Hz) from the light positions: a handful of colour lerps, no
     per-pixel work.
2. **Accent colour per track** *(superseded 2026-10-23: controls use fixed tungsten amber, see
   R7. The track palette still drives the background and glows)*. The most legible palette
   colour (contrast-checked) drives the
   progress bar, play button, selection, active lyric line, focus rings and hover glows.
3. **Coloured glows.**
   - Artwork gets a soft shadow in its own colour: a pre-blurred copy of the cover drawn under it.
   - Hovered cards glow in the accent colour.
4. **Page headers dissolve into it.** Artist and album heroes fade at their bottom edge into the
   background, so the page and the background read as one surface.
5. **Lyrics.** The active line takes the accent with a faint glow. Past and future lines are white
   at reduced opacity, letting the colour show through.
6. **Music-reactive motion (optional).** Cone sway follows tempo, and bass makes the lights
   "breathe". The meter comes from our own engine at no extra cost.
7. **Track change.** The lights recolour with a ~1.2 s crossfade while staying in motion, so the room
   changes colour instead of cutting.
8. **Fullscreen.** The same lit wall, with stronger, wider lights. Motion artwork can sit on top (R9).

**Settings:** blur, darkness and motion speed sliders (defaults 85 / 40% / 0.3×), plus
"Background motion: Everywhere / Now Playing & fullscreen only / Off".


### R5: Streamlined UI, windowed and fullscreen

**Window layout: Apple TV style (chosen 2026-10-23).** Cinematic hero art at the top of every
page, content rows that lift on hover, minimal chrome.

- **Navigation: left sidebar (decided 2026-10-23, replaces the top tab strip).**
  - **Styled like Apple Music's sidebar (decided 2026-10-23)**, in the datacentre palette:
    - About 220 px wide, collapsible to icons.
    - A slightly translucent carbon surface over the immersive background, with a smoked-brown
      hairline on its right edge.
    - Rounded search field at the very top, with a magnifier icon and a placeholder.
    - Compact rows (~30 px): a small line icon in **tungsten amber** before each label, label in
      warm ivory, 13 px.
    - **Small section headers in muted beige** ("Library", "Playlists"). The Playlists header can
      collapse its section.
    - **Selection:** a rounded rectangle in smoked brown behind the row, label in ivory and the
      icon staying amber. Hover is a fainter version. No glow or bloom on the sidebar; it stays
      quiet like Apple's.
    - Playlist rows show a tiny square cover instead of an icon.
  - **Top section:**
    - **Search:** a field at the top of the sidebar (Ctrl+F focuses it). Results open in the
      content area.
    - **Home:** YouTube Music's Home feed, with heroes and shelves.
    - **Explore:** new releases, charts by country, moods & genres (YouTube's editorial
      playlists).
    - **Radio:** live internet radio, as in legacy (Radio Garden). **v1 ships only the button
      and an empty "coming soon" page; the feature itself is deferred** to after the app works
      well.
  - **Library** section (header), in this order (decided 2026-10-23):
    - **Recently played:** your YouTube Music history.
    - **Artists:** artists you subscribe to or have in your library.
    - **Albums:** albums saved to your library.
    - **Songs:** songs saved to your library.
  - **Playlists** section (header, collapsible), in this order:
    - **All playlists:** a page with every playlist as a grid, plus "New playlist".
    - **Liked songs:** YouTube's "Your Likes" auto-playlist (`LM`).
    - Then each of your playlists (made and saved) by name with a tiny cover, flat (YouTube Music
      has no folders), most recently updated first.
  - Not in the sidebar: Uploads.
  - **Bottom:** your account avatar and name (opens settings and sign-out).
- Back/forward buttons sit at the top of the content area.
- **Content:** fills the rest of the window. The lyrics/queue panel on the right is the only
  other side panel.
- **Now-playing bar (decided 2026-10-23, replaces the floating pill):**
  - A full-width bar docked to the bottom edge, solid and functional, not floating.
  - The progress line runs along its top edge in the accent colour and is draggable.
  - Left: artwork, title and artist (both clickable).
  - Centre: shuffle, previous, play/pause, next, repeat.
  - Right: like, lyrics, queue, Listen Along, volume, fullscreen.
  - It's a darker tint of the room's colours, so it belongs to the background rather than
    floating over it.
- **Right panel for lyrics and queue (decided 2026-10-23):**
  - The lyrics and queue buttons in the now-playing bar open a docked panel on the right
    (~340 px, resizable). A segmented switch at its top toggles Lyrics · Queue.
  - The panel **pushes** the content narrower rather than covering it, so pages stay usable.
    Pressing either button again or Esc closes it.
  - It opens and closes with a ~250 ms slide. Its open/closed state and tab are remembered.
  - Styling: a darker tint of the room's colours (like the bar), hairline left edge, and the
    lyrics' active line in the accent colour.
  - In fullscreen, lyrics take over the stage instead of using the panel.
  - **Cost:** when closed, the panel isn't instantiated at all. When open on lyrics, it redraws
    only on line changes and during the scroll animation.
- Clicking the bar's artwork opens Now Playing.
- **Album and playlist pages stay functional**, in the spirit of Cider's album view:
  - The cover sits beside the header.
  - The track list runs the full width, with a search-in-list box and sorting.
  - Each row has a "⋯" menu.
  - The playing row is highlighted in the accent colour.
- **Focus zoom:** tiles scale about 1.1× with a white ring and a deeper shadow on hover or keyboard
  focus. The animation only runs during the 250 ms transition.

**Albums and playlists are first-class:**

- **Home** (was "Listen now"):
  - Hero (top YouTube pick).
  - Rows: "Albums for you", "Your playlists", "Recently played" (albums and playlists, not
    single songs), "Mixed for you" (YouTube mixes), then the rest of YouTube's Home shelves.
- **Library pages** (opened from the sidebar):
  - Albums opens as an **album wall**: large covers, sorted by recently played or added.
  - Artists, Songs, Liked songs and Recently played are functional lists.
- **Album page:**
  - **Full-width banner hero (decided 2026-10-23):**
    - The banner spans the whole window width, about 40% of the window height. It uses Apple's
      editorial album artwork (the wide banner, e.g. `bannerUber`, otherwise another wide
      `editorialArtwork` crop).
    - **The banner carries no text (decided 2026-10-23).** Only the artist logo (fanart.tv) sits
      on it, bottom-left, white, with a soft shadow for legibility.
    - **Below the banner:** the album title, "Album · year · genre", and
      Play / Shuffle / Like / ⋯, then the track list.
    - The banner's bottom edge fades into the immersive background, which takes its colours from
      the banner, and the title block starts right below.
    - **No Apple banner:** the banner is built from the YouTube cover (scaled, softly blurred and
      darkened across the full width), with the crisp square cover on the right.
    - **No logo:** the banner stays clean, with no text in its place. The artist name appears
      below the banner with the title.
    - **Motion:** static by default. If Apple has a wide motion video for the album, the banner can
      play it while the album page is visible and the window is focused. This is an option, off by
      default, per the motion-art decision.
  - **Track list (decided 2026-10-23):**
    - No container panel; the list spans the full window width with a small side margin.
    - Rows are tall (~68 px), each on its own darker strip (black ~35%), rounded, with a 2 px gap
      between rows.
    - Track numbers have no background of their own.
    - The playing row is tinted with the accent.
  - Explicit badges, a disc and duration footer, and the label/℗.
  - Rows: "More by …" and "Fans also like".
- **Playlist page:**
  - Same structure. The hero art is the playlist's own cover, or a 2×2 mosaic of its first four
    album covers.
  - Owner, song count and total length.
  - Track list rows include artwork.
  - A "Suggested songs" row from YouTube's playlist suggestions.
- **Artist page:**
  - Wide banner hero with the logo (fanart.tv) or the name in large type.
  - Top songs, then album and single rows, "Fans also like", and the bio.

**Where the immersive background is used, and what colours it**

The background is the room every page sits in. The wall stays the same; the lights' colours come
from whatever the page is about:

| Page | Light colours | Behaviour |
|---|---|---|
| Album, playlist, artist | **That page's artwork** | The hero dissolves into light of the same colours, so the page and the room are one surface. Crossfades ~1.2 s on navigation. |
| Listen now, Browse, Library, Search | **The now-playing song** | Hovering a tile for more than 400 ms gently pulls one light towards that tile's colours as a preview, then lets it go. |
| Now Playing, fullscreen | **The now-playing song** at full intensity | Stronger, wider lights; music-reactive motion if enabled; motion artwork on top. |
| Nothing playing | No lights: just the wall | The lights fade in when something starts playing. |

**Always in the accent colour** of the current room: the play button, the artist link, the
playing row, focus rings and the progress bar.

**Cost:** changing the room's colours is a crossfade that only runs during the 1.2 s transition.
Hover previews are debounced so quick mouse movement doesn't trigger them. The background's
per-frame cost is unchanged (R4).

**Fullscreen "stage"** (F11, or click the artwork):

- Large art or motion art on the left, synced lyrics on the right, or lyrics only.
- Minimal controls that fade out after 3 s idle.
- Exits with Esc.

**Game guard** must exclude our own fullscreen window.

### R6: Metadata sources (simplified 2026-10-23)

> **Apple album banners and animated covers: kept (decided 2026-10-23)**, on the condition that
> they never need an Apple login. They don't:
> - They use the anonymous token built into Apple's public music.apple.com website, the same way
>   the legacy app fetched catalogue data without anyone signing in.
> - If Apple ever starts requiring a login for them, the feature is dropped rather than asking for
>   one.
> - Albums Apple has no match for show the static YouTube cover.

**Private app.** It's shared with friends only, never released. No attribution lines anywhere:
no "From Wikipedia", no lyric source or contributor credits. No key is ever needed from a user;
the one developer key (fanart.tv) is embedded at build time from a gitignored file.

| Need | Source | Notes |
|---|---|---|
| Artist page data | **YouTube Music** | Name, wide header photo, bio text, **monthly audience** (no subscriber count), top songs with plays, albums, singles & EPs, videos, "Featured on", "Fans might also like", artist radio/shuffle |
| Album page data | **YouTube Music** | Title, type, year, artists, large cover, track count, length, explicit badges, plays per track, other versions, more by artist, description when present |
| Song data | **YouTube Music** | Title, artists, album, length, plays, Related tab, song ↔ music-video switch |
| Playlists, Home, Explore | **YouTube Music** | Home shelves, new releases, charts by country, moods & genres editorial playlists, playlist suggestions |
| **Lyrics** | **Unison** (Better Lyrics' crowdsourced DB, `unison.betterlyrics.org/lyrics`, keyed by YouTube videoId, syllable/word/line TTML; reads need only a self-generated key id) | Silent fallbacks so songs Unison lacks still get lyrics: YouTube Music timed lyrics, then LRCLIB. Presentation ideas from Better Lyrics (see R5), implemented natively; GPLv3, so no copied code |
| **Artist logo** | **fanart.tv** `hdmusiclogo` (embedded developer project key) | Used wherever the artist's name is the hero: artist page hero, album page artist line (small), Now Playing / fullscreen, Listen now "featured artist" hero. Fallback: the name in large heavy type in the accent colour. fanart.tv looks artists up by MusicBrainz ID, so we do one hidden MusicBrainz name → ID lookup per artist (≤1 req/s, cached forever). |

**Not used (decided 2026-10-23):** Wikidata facts, fanart.tv backgrounds, MusicBrainz release
data, ISRC, Genius, song credits pages, and Apple editorial notes.

### R7: Visual language ("designed by Apple", not Material or YouTube Music)

**Art direction (2026-10-23): "datacentre at night".** Apple-grade software crossed with a
glass-fronted late-1970s IBM building glowing orange in the dark: warm tungsten light, dark glass
facades, oxide-red feature walls. Original, not a copy of Apple Music or Spotify. **Illuminated,
not decorated:** amber is light emitted from inside the interface, not a generic accent colour.

| Token | Hex | Use |
|---|---|---|
| Carbon black | `#17110F` | Primary background (replaces #0B0B0F) |
| Oxide red | `#76291E` | Feature surfaces, selected areas, the playing row |
| Tungsten amber | `#E9A15F` | Active controls, progress, focus, glows: anything "lit" |
| Warm ivory | `#F1E5D2` | Primary text, light surfaces |
| Smoked brown | `#342520` | Dividers, secondary surfaces, inactive controls |
| Muted beige | `#B7A69A` | Secondary text, metadata |

- **Dark by default.** A light appearance uses warm-ivory backgrounds, deep oxide text and the
  same amber light.
- **"Adjusted to songs":**
  - The immersive background keeps each album's own colours, but sees them **through warm smoked
    glass**: hues are pulled ~35% towards tungsten and slightly dimmed, so every album glows like
    a different lit room in the same building.
  - The fixed palette above carries the chrome. **Amber replaces the old per-track accent** for
    controls, so the UI stays recognisable whatever's playing.
- **Light as material:**
  - Lit elements (play button, progress line, active tab, focus) get a soft amber bloom.
  - It's a pre-rendered glow sprite or drop-shadow, not a blur pass, so there's no extra
    per-frame cost.
- **Facade motifs, used sparingly:**
  - Faint vertical mullion lines across the background (smoked brown, 1–2 px, wide spacing).
  - Thin "ceiling light" strips glowing at the top edge.
  - Fine vertical-blind ribbing over banners.
  - All static, so they cost nothing per frame.
- **Typography:**
  - Inter for UI text.
  - Proposed: **IBM Plex Mono** (open licence) for small signage-style labels: kickers,
    track numbers, times, counts. It's the datacentre-signage voice. Pending decision.
- **Superseded:** cool white "glass" tints. Glass surfaces are now carbon or smoked brown at
  60–90% with smoked-brown hairlines.

**Earlier palette notes (superseded by the table above):**

- **Palette:** near-black base (#0B0B0F) with the immersive colour underneath. The glass is
  white at 6–10% over the blur, with hairline borders at white 8–12%.
- **Edges:** gradient edges, a top highlight and a soft long shadow.
- **Shapes:** generous radii (12–20 px on cards, continuous-looking corners), large artwork, lots
  of breathing room. No hard square grids, ripples, FABs or chips.
- **Typography:** big bold titles, quiet secondary text, a tight weight hierarchy.
- **Motion:** spring animations (Slint 1.18 has them) for panels, hovers and transitions.
  Nothing bounces for longer than about 300 ms.

**Challenge: SF Pro's licence restricts it to Apple platforms.** We can't ship it.

- **Use Inter (variable)**, embedded at about 0.8 MB. It's free and very close to SF.
  *(Decided 2026-10-23: Inter only. No SF Pro, not even when it's installed.)*
- The alternative is **Segoe UI Variable**: already in memory on Windows 11, so zero cost, but it
  looks less "Apple".

### R8: Functional, not just pretty

- **Keyboard first:**
  - Space to play/pause, ←/→ to seek, Ctrl+F for search, Ctrl+L for lyrics, F11 for fullscreen.
- **Windows integration:** SMTC (media keys and the Windows media flyout), and taskbar thumbnail buttons. **No global hotkeys** (decided 2026-10-23).
- **Queue:** drag reorder, "play next", "add to queue", remove, clear.
- **Search:** results as you type, debounced, with the top result first.
  - **Unreleased songs, leaks, remixes and fan uploads: DEFERRED (2026-10-23, "skip for now").**
    - YouTube Music's search only covers its music catalogue. Uploads by ordinary channels play
      fine in YouTube Music once saved to a playlist, but its search never shows them.
    - **Fix:** every search also queries regular YouTube search (InnerTube `WEB` `search`,
      videos only) in parallel.
    - The results are merged into a separate **"From YouTube"** section under the catalogue
      results.
    - Filters: drop anything over ~15 min and anything already in the catalogue results (same
      videoId or same normalised title and artist).
    - Light title clean-up ("Artist - Title (Unreleased) [HQ]" becomes title "Title", artist
      "Artist", tag "Unreleased").
    - They play like any other song (everything is a videoId to our resolver), can be added to
      playlists, and start radio.
    - Cost: one extra request per search (debounced). Not in phase 2; revisit later.
- **Your library:** right-click actions everywhere (like, add to playlist, go to artist/album,
  start radio).
- **Remembered state:** last queue and position are restored on launch.
- **Gapless playback;** crossfade is optional.
- **Game guard:** carried over from legacy.

### R9: Editorial and personal content per artist, album and song; animated covers

- **Artist page:** wide hero (YouTube Music header), the fanart.tv logo over the hero when
  available, monthly audience, top songs, albums/singles, "fans also like" (YouTube related), bio.
- **Album page:** full-width editorial banner with the artist logo on it (see R5), otherwise a cover-built banner.
  Motion cover when available. Track list, year and label.
- **Song:** lyrics (Unison), "start radio", related.
- **Personal content (needs sign-in):** your play history ("recently played"), whether you've
  liked it, and which of your playlists contain it.

**Animated covers: challenge and plan.**

- **Cost:** a 720p H.264 loop, hardware-decoded through Media Foundation, is roughly 1–2% CPU plus
  a texture upload per frame at 24–30 fps.
- **When they play:** only on the Now Playing / fullscreen stage and the album page hero, while the
  window is focused and visible and no game is running.
- **Elsewhere:** the static cover.
- **Caching:** clips are downloaded once (5–10 MB each) and cached on disk with an LRU cap
  (~500 MB).
- **Toggle:** "Motion artwork: On / Fullscreen only / Off".
- **HEVC 4K variants:** never used, because Windows may not have the codec and they cost too much.

---

## 3. What we keep from legacy, and what we cut

**Port (proven code):**

- Native audio engine (wait-free render thread, gapless, crossfade, DSP and meter)
- Game guard
- Immersive background cost model
- Lyrics parsing (LRC, LRCLIB lyricsfile, estimated word timing)
- Image loader (LIFO, LRU, disk cache)
- Media controls (SMTC)
- Apple token scraper
- Logging

**Cut:**

- Apple Music and Spotify playback, Deezer, local-file library
- All custom recommendation modules
- Replay / Wrapped, Release Radar, AI DJ, DJ sets, playlist transfer, Genius, AutoEQ,
  sound search, visualizer, Live Radio, accounts UI

**Also kept (decided 2026-10-23):**

- Discord presence
- EQ and crossfade
- Listen Along, **reworked and trimmed** (see "Listen Along v2" below)

**Also cut:** Last.fm scrobbling, local files.

**Deferred until the app works well:** a new custom **Automix** implementation (DJ-style
transitions), designed from scratch later. The engine is designed from day one so it can slot in
later:
- Two decks mixing into one output.
- Sample-accurate cue scheduling.
- Per-deck EQ and filters.
- A meter/onset stream.

Not in v1.

**Listen Along v2.** Legacy was ~5.4k lines, much of it there because host and listeners could be
on different providers (cross-catalogue matching, per-provider start latency, WebView decks).
With YouTube Music only, everyone plays the same stream for the same `videoId`, so it shrinks to
roughly a third.

- **Keep:**
  - The iroh P2P transport (QUIC, hole punching, relay fallback), started only while a session
    exists and stopped afterwards, so there's zero cost when not in use.
  - `amnative://` invite links (renamed to the new app), forwarded to the running instance.
  - NTP-style clock sync.
  - Song requests going straight into the host's queue, showing who added what.
- **Protocol:** a handful of versioned messages: `Hello{name}`,
  `State{videoId, position_ms, playing, host_clock}`, `Queue{videoIds}`, `Request{videoId, by}`,
  `Bye`.
- **Sync:** see "Listen Along sync: the design for sample-accurate playback" below.
- **Drop:**
  - Cross-provider resolving and matching.
  - Per-provider resolver traits.
  - WebView deck quirks.
  - Legacy follow-mode complexity.
- **UI:**
  - A Listen Along button in the now-playing bar opens a small popover: start a session / copy
    the invite link, participants with avatars, and leave.
  - Listeners see a "Listening with ⟨host⟩" chip in the bar.
  - The queue panel shows who requested each song.

**Listen Along sync: the design for sample-accurate playback (2026-10-23)**

**Target:**
- At every song start (and after every seek, pause/resume and skip), every participant plays the
  same audio at the same wall-clock instant, **within 10 ms** (decided 2026-10-23). In practice
  it lands at a few ms.
- During a song, playback is left untouched. Natural clock drift (typically 2–12 ms per song)
  is cleared at the next song start.
- It's verified by measurement, not by eye.

**Out of scope (decided 2026-10-23):** Bluetooth and other device latency Windows doesn't report.
No per-device offsets or calibration.

**Why legacy drifted:**
- Different providers meant different masters and encodes, so "the same position" wasn't the same
  audio.
- WebView decks had unknown output latency.
- The host's own playback was the reference, so its buffering jitter leaked into everyone else.
- Starts weren't scheduled on a shared clock, so even the starting point was off before any drift.

**The design:**

1. **Same bits.**
   - The host announces the exact stream format (itag) and everyone plays that one. The session
     falls back to a format every participant can get: Opus 251, then AAC 140.
   - Positions are expressed in **decoder sample frames** at 48 kHz, YouTube Opus's native rate,
     after the codec's pre-skip/priming is removed. So "frame N" is bit-identical audio on every
     machine.
   - Seeks are sample-exact: seek to the preceding packet, then decode and discard up to frame N.
2. **One shared timeline, not "follow the host".**
   - A session clock is agreed (below). Every playback action (play, pause, seek, skip, next
     song) is broadcast as a **scheduled action**: "frame F of videoId V plays at session time T",
     with T ≈ now + 400 ms.
   - Everyone, **including the host**, executes it at T. Nobody chases anybody.
   - The host's UI reacts instantly (the button state changes); the audio follows at T.
   - Song changes are pre-scheduled from the shared queue, so every participant has already
     prefetched and pre-decoded the next song, and the transition is gapless and simultaneous.
3. **Clock sync (NTP-style over the existing QUIC link, continuously).**
   - Use QueryPerformanceCounter (monotonic, sub-µs) as the local time base, never the system
     clock.
   - Exchange 4 timestamps per probe, 5 probes/s for the first 2 s, then one every 2 s.
   - Keep a sliding window. Use only the **lowest-RTT ~10% of samples**, since queueing delay is
     what corrupts the estimate.
   - Fit offset **and skew** (ppm) with a linear regression, so the clock is predicted between
     probes.
   - Prefer the direct hole-punched path. On iroh's relay the path is less symmetric, so the
     expected error is shown.
   - The residual error is half the path asymmetry: typically a few ms at most on home connections, even over the relay.
4. **Know exactly which sample the device is playing.**
   - Keep **cpal** (no engine rewrite). Its callback playback timestamps plus a running count of
     frames delivered give a frame→QPC mapping that's accurate to ~1–2 ms, well inside the
     budget.
   - If the device isn't 48 kHz, the resampler's fixed delay is known and compensated.
5. **Sync at the start of every song; no correction while it plays (decided 2026-10-23).**
   - Audio is never resampled or speed-adjusted for sync. When the device runs at 48 kHz, playback
     stays bit-perfect.
   - Each song starts at its scheduled instant on every machine (step 2), which puts everyone
     within a few ms.
   - **Song boundaries re-sync automatically.** The next song's start is scheduled on the shared
     clock too, so any drift accumulated during a song is dropped at the boundary. The next song
     simply starts on time, and the ≤ a-few-ms gap or overlap falls in the silence between songs.
   - **Expected drift within one song:** sound-card clocks differ by typically 10–50 ppm, about
     2–12 ms over a 4-minute song. The worst case (±100 ppm on both ends) is ~50 ms on a long
     track. That's audible only if two people are in the same room, and it's gone at the next
     song.
   - **Safety net, not correction:** if a participant ends up > 100 ms off mid-song (a network
     stall, the PC sleeping, a device change), it re-joins at the correct position with a short
     fade. That only happens after something broke, never as routine drift handling.
   - A listener that stalls rejoins *at the correct current position*; it never "catches up" by
     playing fast.
6. **Measured, not guessed.**
   - Every client reports its live lock error to the session. The Listen Along popover shows each
     participant as "± 2 ms ●" (offset at the last song start plus current drift).
   - **Automated test harness:** run host and listener on one PC, capture both outputs with
     WASAPI loopback, cross-correlate, and assert |offset| ≤ 10 ms at every song start, seek,
     resume and skip over a 10-minute session. Mid-song drift is logged, not asserted. A second test adds artificial network jitter and asymmetry.

**Honest limit: voice chat.**
- If you're on Discord together, Discord itself delays voices by 50–150 ms. The *music* is in
  sync, but a friend's reaction arrives late.
- There's no fix that keeps everyone's music aligned. Delaying your own music to match the call
  would desync it for everyone else.

**Cost:** clock probes are ~100 bytes/s and the PI controller is a few float ops per audio
period. Zero when no session is active.

**Decisions (2026-10-23):**

- **Audience:** shared privately with friends. Developer keys and tokens never go in a public
  repo; they're loaded from a gitignored file at build time.
- **Account:** signed-in free YouTube account, so personalised Home and radio, with Opus 251
  (~160 kbps) as the best stream.
- **Motion artwork default:** Fullscreen + Now Playing only.

---

## 4. Architecture

```
music (workspace)
├─ core/          headless; no UI deps. Lives for the whole process.
│   ├─ engine/    ported audio engine + WebM demux + Opus decoder (rusty-opus) + AAC (symphonia)
│   ├─ ytm/       InnerTube client: home, search, browse, next/radio, lyrics, library, likes,
│   │             playback reporting; cookie session
│   ├─ resolve/   stream URL resolver: native fast path → yt-dlp(+deno) fallback; URL cache;
│   │             hot-swappable, auto-updates yt-dlp
│   ├─ meta/      lyrics (Unison → YTM → LRCLIB), fanart.tv logos (+ MusicBrainz id lookup), Apple banners/motion art (anonymous token, no login)
│   ├─ cache/     disk cache (JSON + images + clips), LRU caps
│   ├─ player/    queue, playback state, gapless prefetch, crossfade, SMTC, taskbar buttons
│   └─ guard/     game guard, EcoQoS
└─ app/           Slint UI. On minimize: drops textures and caches, then trims the working set.
    ├─ ui/*.slint design system (tokens, Glass, Card, Shelf, PlayerBar, LyricsView, Stage)
    ├─ bridge     core state snapshot → Slint models; UI actions → core commands
    ├─ immersive  palette extraction, lit-wall layers, motion
    └─ video      Media Foundation motion-art player (optional)
```

**Threads:**

| Thread | Priority | Notes |
|---|---|---|
| audio | real-time | |
| decode ×2 | background | |
| net | background | one tokio current-thread runtime |
| image/meta worker | background | started on demand, exits when idle |
| UI | — | only while a window exists |

**Streams:** best available format.

1. Premium Opus 774
2. Opus 251
3. AAC 140

Audio is streamed with HTTP range requests into the decoder ring, so nothing goes to disk.

---

## 5. Risks

1. **YouTube changes break streaming.** This happened twice between Aug and Oct 2026. The
   resolver is pluggable, auto-updates yt-dlp, and shows a clear "updating stream resolver" state.
2. **ToS.** Unofficial YouTube and Apple access. Fine for personal use, risky to distribute.
3. **Slint memory on window destroy.** Spike A: GPU driver memory isn't released on destroy, but trimming the working set on minimize gets the app to ~7 MB (see 5b).
4. **Young pure-Rust Opus decoders.** Spike B: rusty-opus is float-exact versus libopus and fast;
   small container fixes needed.
6. **A single working no-token client (visionos).** Health-checked failover to yt-dlp.
5. **Apple matching misses.** Enhancement only.

---

## 5b. Spike results (2026-10-23)

Code is in `spikes/` (throwaway). Machine: Ryzen 7 7800X3D, RTX 4070 Ti, Windows 11. RAM is
private working set (Task Manager's "Memory"). CPU is a % of all 16 threads.

### A: Slint 1.18.1, album-page mock with a 6-blob background at 30 fps

| Renderer | Open idle | Animating | Minimized | Window destroyed | Recreated |
|---|---|---|---|---|---|
| FemtoVG (OpenGL) | 54 MB, 0% | 57 MB, 1.6% CPU, 5.8% GPU | 57 MB, ~0% | **56 MB** (driver stays loaded) | 58 MB |
| FemtoVG (wgpu/DX12) | 89 MB, 0% | 100 MB, 1.0% CPU, 3.4% GPU | 106 MB | **34 MB** | 93 MB |
| Skia | (fell back to CPU raster: Skia's wgpu surface rejected Vulkan; not a valid GPU result) | 6.2% CPU | | 39 MB | |
| Software | 9 MB, 0% | 9 MB, **6.2% CPU** (~1 core) | 9 MB | **3 MB** | 9 MB |

**Findings:**
- Idle is truly 0% CPU/GPU on every renderer.
- **A destroyed window doesn't give the GPU drivers' memory back** (34–56 MB stays). The process
  floor with no GPU loaded is ~3 MB.
- **Follow-up test: trim the working set on minimize.**
  - When minimized, the app calls `EmptyWorkingSet`, handing its idle pages back to Windows.
  - FemtoVG GL **minimized: 57 MB → 6.6 MB**, 0% CPU/GPU.
  - Restore drew its first frame in **6.5 ms**, and memory stayed ~7.5 MB afterwards because
    only the pages actually used come back.
- **Decision (2026-10-23): one process, minimize to the taskbar, no tray.**
  - On minimize: stop all animation and timers, drop textures, the image cache and any motion-art
    decoder, then trim the working set.
  - Estimated **~10–20 MB while playing minimized** (audio decode ~9 MB stays hot). To be
    confirmed in phase 1.
  - Restore is effectively instant. If a game has used up RAM, restore may need some pages from
    disk (expected tens to a few hundred ms).
  - The two-process idea is dropped.
- **Quick control mid-game, without leaving the game:**
  - Media keys and the Windows media flyout (SMTC).
  - Taskbar thumbnail buttons: previous, play/pause, next.
  - No global hotkey (decided 2026-10-23). Restore the window from the taskbar.

### B: YouTube resolving and decoding

- **Native fast path works:** the InnerTube `visionos` client, signed out, with a cached
  visitorData returns plain itag 251/140 URLs. No JS, no PO token. Tested on 5/5 videos.
  - Player call 64–148 ms; first audio byte 165–301 ms total.
  - Streams at 5–11 MB/s, not throttled. URLs live ~6 h.
- **Other clients:**
  - `android_vr` URLs die after 1 MiB (403).
  - `tv_samsung`/web clients need JS signature deciphering.
  - web/android/ios are SABR-only.
  - **Health checks must test past the first MiB.**
- **yt-dlp:** the installed 2026.03.17 build is too old and returns dead android_vr URLs. A
  ≥2026.10 build plus Deno or Node (Node 24 is installed) is the fallback. ~2.5 s and ~90 MB per
  spawn, so batch-resolve ahead.
- **Decode** (symphonia 0.6.1 + rusty-opus 1.0.2):
  - Opus matches ffmpeg to 135 dB SNR (float-exact). It runs at 370× realtime, ~155 ms CPU per
    audio-minute, ~9 MB.
  - AAC matches to 132 dB, 1100× realtime.
  - First PCM 160–280 ms after the first range request.
  - **To fix:** Opus end-padding trim (7–10 ms, needed for gapless), AAC priming (36 ms), and
    rusty-opus's ±1.0 clamp (loses headroom before volume/EQ). `opus-decoder` is too slow
    (3× realtime); don't use it.
- **Metadata, signed out:**
  - Search: 0.4–0.6 s. Radio: 50 tracks + continuation.
  - **Unison** works: Latch returned syllable-timed TTML in 0.3 s. 3 of 4 test songs weren't in
    it yet, so the YouTube/LRCLIB fallbacks matter. Reads don't check `x-key-id`.

### C: Apple motion art and banners (anonymous token, no login)

- **Token:** scraped in 0.37 s, made robust to header order.
- **Matching:** 5/5 albums matched first. Prefer an exact title over deluxe editions.
- **Banner art:** every album has wide art at **storeFlowcase / subscriptionHero 4320×1080**.
  This is the full-width album banner. `bannerUber` never appeared.
- **Motion covers:** 3/5 albums.
- **Bonus:** Apple also has artist logos (`musicContentColorLogoTrimmed`, 4826×1264). This is a
  possible fallback when fanart.tv has none.
- **Variants:** square clips are AVC 360–1080 (no 720; 768² is the closest) plus HEVC 4K. Loops
  are 24 s at 30 fps.
- **Decode cost** (Media Foundation hardware decode, looping, with frames copied to the CPU):
  - 768²: **0.13–0.44% CPU**, ~3–5% GPU video engine, 1.7–2.2 ms readback per frame.
  - NV12 without conversion: 0.2% CPU.
  - 1080² (12 Mbps): 0.2–0.95% CPU.
  - Software decode: 1.1–1.8% CPU and 110–190 MB.
  - Hardware RAM ~60–80 MB, mostly D3D device and driver; shared with the renderer in practice.
- **Decision:** use 768² AVC, and the low 1080 rung only for a big fullscreen stage. Never HEVC.
  "Fullscreen + Now Playing" motion art is cheap.

---

## 6. Phases

0. **Spikes** (decide before building):
   - **A:** Slint (FemtoVG and Skia): a window with a blurred image, shadows and a 30 fps
     animation. Measure RAM/CPU/GPU open, minimized, and after destroy-to-tray. Done, see 5b.
   - **B:** stream resolve: native InnerTube client vs yt-dlp. Opus decode with rusty-opus.
   - **C:** motion art: Media Foundation decode of one Apple clip into a Slint image. Measure CPU.
1. **Core playback (redefined 2026-10-23):** a lightweight core written fresh, not ported.
   - **Engine:** one deck, no transitions or crossfade for now; songs play out. Gapless
     playback (sample-exact trims), a 10-band EQ, and volume. Bit-perfect when EQ is flat, volume
     is 100% and the device is at the source rate.
   - **Streaming:** HTTP range streaming.
   - **Resolver:** native visionos resolver only; **no yt-dlp/Node bundled** unless it proves
     necessary.
   - **Player:** queue with persistence.
   - **Windows:** SMTC, taskbar buttons, minimize trim, game guard.
   - **Test harness window:** play by link or video ID.
2. **YouTube Music client:** search, home, radio, artist/album/playlist, sign-in, library,
   playback reporting.
3. **UI shell + design system:** sidebar, pages, player bar, queue panel.
4. **Immersive v2 + fullscreen stage + lyrics.**
5. **Artist/album richness:** logos, banners, bios, editorial.
6. **Motion artwork** (optional).
7. **Polish:** keyboard, remembered state, measurements against the R1 targets.
8. **Listen Along v2.**
9. **Later, once the app works well:** custom Automix, and live Radio (the sidebar button
   ships in v1 with a "coming soon" page).

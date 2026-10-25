//! Playing: starting songs and lists, autoplay, skipping and seeking, the engine's events,
//! the now-playing bar, the meter, lyrics and the media keys.

use crate::*;

impl App {
    /// A song card plays with the other songs beside it.
    pub(crate) fn play_shelf_track(&mut self, row: usize, i: usize, item: Item) {
        let items: Vec<Item> = match &self.rows[row] {
            RowRef::Shelf(items, _) => items.clone(),
            _ => vec![item],
        };
        let start = items[..i.min(items.len())]
            .iter()
            .filter(|x| matches!(x, Item::Track(_)))
            .count();
        self.play_list(pages::tracks_of(items), Some(start), None);
    }

    /// The play button on a card: it plays, opening nothing.
    pub(crate) fn card_play(&mut self, row: usize, i: usize) {
        match self.card_item(row, i).cloned() {
            Some(Item::Track(_)) if self.radio_card_clicked(row, i) => {}
            Some(item @ Item::Track(_)) => self.play_shelf_track(row, i, item),
            Some(item) => self.play_item(item, Mode::Play),
            None => {}
        }
    }

    pub(crate) fn save_plays_later(&mut self) {
        self.plays_timer
            .start(TimerMode::SingleShot, Duration::from_secs(5), || {
                with(|a| settings::save_plays(&a.plays));
            });
    }

    // ---- playback --------------------------------------------------------------------------

    /// Plays from row `i`, with the tracks beside it. -1 shuffles the page (or what its banner
    /// stands for), -2 plays it from the top in order.
    pub(crate) fn play_page(&mut self, i: i32) {
        if i < 0 {
            let shuffle = i == -1;
            match self.header.as_ref().and_then(|h| h.play.clone()) {
                Some(HeroPlay::Item(item)) => {
                    self.play_item(item, if shuffle { Mode::Shuffle } else { Mode::Play })
                }
                Some(HeroPlay::Artist {
                    radio,
                    shuffle: mix,
                }) => {
                    let r = if shuffle {
                        mix.or(radio)
                    } else {
                        radio.or(mix)
                    };
                    if let Some(r) = r {
                        self.play_watch(r, shuffle);
                    }
                }
                None => {
                    let own: Vec<Track> = self
                        .page_tracks
                        .iter()
                        .zip(&self.page_groups)
                        .filter(|(_, g)| **g == 0)
                        .map(|(t, _)| t.clone())
                        .collect();
                    self.play_list(own, None, Some(shuffle));
                }
            }
            return;
        }
        let Some(ti) = self.track_at(i as usize) else {
            return;
        };
        // From search or a song page, like YouTube Music: the song, then its radio, not the
        // other results.
        if matches!(
            self.history[self.at].kind,
            PageKind::Search | PageKind::Song
        ) {
            let t = self.page_tracks[ti].clone();
            // A music video found by search opens on its song page, the picture centred.
            let video =
                t.kind == TrackKind::Video && self.history[self.at].kind == PageKind::Search;
            if video {
                self.autoplay_from(t.clone(), None);
                return self.open_video(&t);
            }
            return self.autoplay_from(t, None);
        }
        let group = self.page_groups[ti];
        let mut start = 0;
        let mut tracks = Vec::new();
        for (j, t) in self.page_tracks.iter().enumerate() {
            if self.page_groups[j] == group {
                if j == ti {
                    start = tracks.len();
                }
                tracks.push(t.clone());
            }
        }
        // A list showing only its top few (an artist's Top songs): the whole list plays, from
        // its "See all" playlist. The clicked song starts at once; the rest follow when loaded.
        let full = (group != 0)
            .then(|| {
                self.rows[..i as usize].iter().rev().find_map(|r| match r {
                    RowRef::Heading(m) => Some(m.clone()),
                    _ => None,
                })
            })
            .flatten()
            .flatten()
            .filter(|m| m.browse_id.starts_with("VL"));
        let Some(more) = full else {
            return self.play_list(tracks, Some(start), None);
        };
        let shown: Vec<String> = tracks.iter().map(|t| t.video_id.clone()).collect();
        let chosen = shown[start].clone();
        if !self.listening() {
            self.play_list(tracks, Some(start), None);
        }
        self.play_seq += 1;
        let seq = self.play_seq;
        let client = self.client.clone();
        workers::run_urgent(
            {
                let id = more.browse_id.clone();
                move || client.playlist_all(&id, 500).map(|p| p.tracks)
            },
            move |res| {
                with(|a| match res {
                    // Something else was played since.
                    Ok(_) if a.play_seq != seq => {}
                    Ok(_) if !a.listening() && !a.player.queue().starts_with(&shown) => {}
                    Ok(all) if a.listening() => {
                        let start = all.iter().position(|t| t.video_id == chosen);
                        a.play_list(all, start, None);
                    }
                    Ok(all) => {
                        let rest: Vec<Track> = all
                            .into_iter()
                            .filter(|t| !t.unavailable && !shown.contains(&t.video_id))
                            .collect();
                        if rest.is_empty() {
                            return;
                        }
                        let ids = rest.iter().map(|t| t.video_id.clone()).collect();
                        for t in rest {
                            a.known.insert(t.video_id.clone(), t);
                        }
                        a.player.append(ids);
                        a.queue_changed();
                    }
                    Err(e) => log::warn!("{}: {e}", more.browse_id),
                });
            },
        );
    }

    /// Plays `tracks` from `start` (else a random one when shuffling, else the first). `shuffle`
    /// sets shuffle on or off; `None` leaves it.
    pub(crate) fn play_list(
        &mut self,
        tracks: Vec<Track>,
        start: Option<usize>,
        shuffle: Option<bool>,
    ) {
        if self.listening() {
            let tracks: Vec<Track> = tracks.into_iter().filter(|t| !t.unavailable).collect();
            self.ask_host(listen::Req::Play {
                tracks,
                start,
                shuffle,
            });
            return;
        }
        self.cancel_skip();
        self.autoplay = None;
        if let Some(on) = shuffle {
            self.player.set_shuffle(on);
        }
        let chosen = start
            .and_then(|s| tracks.get(s))
            .map(|t| t.video_id.clone());
        let playable: Vec<Track> = tracks.into_iter().filter(|t| !t.unavailable).collect();
        if playable.is_empty() {
            return;
        }
        let start = match (&chosen, shuffle) {
            (Some(id), _) => playable.iter().position(|t| &t.video_id == id).unwrap_or(0),
            (None, Some(true)) => (music_core::paths::unix_now() as usize) % playable.len(),
            (None, _) => 0,
        };
        let ids: Vec<String> = playable.iter().map(|t| t.video_id.clone()).collect();
        for t in playable {
            self.known.insert(t.video_id.clone(), t);
        }
        self.resume_at = None;
        self.player.play_queue(ids, start);
        self.queue_changed();
        self.show_now_playing(false);
    }

    /// Fetches the tracks of an album, playlist or artist (their radio, or shuffle), or a
    /// song's radio, then plays or queues them.
    pub(crate) fn play_item(&mut self, item: Item, mode: Mode) {
        if let (Item::Track(t), Mode::Play) = (&item, mode) {
            return self.autoplay_from(t.clone(), None);
        }
        self.play_seq += 1;
        let seq = self.play_seq;
        let client = self.client.clone();
        workers::run_urgent(
            move || -> music_core::Result<Vec<Track>> {
                let watch = |r: WatchRef| {
                    client
                        .watch_next(
                            r.video_id.as_deref(),
                            Some(&r.playlist_id),
                            r.params.as_deref(),
                        )
                        .map(|w| w.tracks)
                };
                Ok(match item {
                    Item::Track(t) if matches!(mode, Mode::Next | Mode::Queue) => vec![t],
                    Item::Track(t) => {
                        let mut radio = client.radio(&t.video_id)?.tracks;
                        if radio.first().map(|r| &r.video_id) != Some(&t.video_id) {
                            radio.insert(0, t);
                        }
                        radio
                    }
                    Item::Album(a) => client.album(&a.id)?.tracks,
                    Item::Playlist(p) => client.playlist(&p.id)?.tracks,
                    Item::Artist(a) => {
                        let p = client.artist(&a.id)?;
                        let r = if mode == Mode::Shuffle {
                            p.shuffle.or(p.radio)
                        } else {
                            p.radio.or(p.shuffle)
                        };
                        match r {
                            Some(r) => watch(r)?,
                            None => Vec::new(),
                        }
                    }
                    Item::Link(_) => Vec::new(),
                })
            },
            move |res| {
                with(|a| match res {
                    Ok(tracks) => a.use_tracks(tracks, mode, seq),
                    Err(e) => log::warn!("play: {e}"),
                });
            },
        );
    }

    /// Plays a song at once (unless it's already playing) and lets YouTube's radio fill the
    /// queue after it, from `chip` (else "All").
    pub(crate) fn autoplay_from(&mut self, t: Track, chip: Option<(usize, WatchRef)>) {
        self.play_seq += 1;
        let id = t.video_id.clone();
        if self.player.current() != Some(id.as_str()) {
            self.play_list(vec![t], Some(0), Some(false));
        }
        // Listening along: the host's app grows the radio.
        if self.listening() {
            return;
        }
        let (picked, watch) = match chip {
            Some((i, w)) => (i, Some(w)),
            None => (0, None),
        };
        let chips = match self.autoplay.take() {
            Some(a) if watch.is_some() => a.chips,
            _ => Vec::new(),
        };
        let seq = self.play_seq;
        self.autoplay = Some(Autoplay {
            seed: id.clone(),
            chips,
            picked,
            seq,
        });
        self.show_autoplay();
        let client = self.client.clone();
        workers::run_urgent(
            move || match &watch {
                Some(w) => client.watch_next(Some(&id), Some(&w.playlist_id), w.params.as_deref()),
                None => client.radio(&id),
            },
            move |r| {
                with(|a| match r {
                    Ok(w) => a.autoplay_arrived(w, seq),
                    Err(e) => log::warn!("autoplay: {e}"),
                });
            },
        );
    }

    /// The radio arrived: it becomes everything after the song, if that's still what plays.
    pub(crate) fn autoplay_arrived(&mut self, w: WatchNext, seq: u64) {
        let Some(ap) = &mut self.autoplay else {
            return;
        };
        // Asked again since (another choice, the radio button) or something else plays.
        if ap.seq != seq || self.player.current() != Some(ap.seed.as_str()) {
            return;
        }
        // The choices come with the first answer; picking one keeps them.
        if ap.chips.is_empty() {
            ap.picked = w.chips.iter().position(|c| c.selected).unwrap_or(0);
            ap.chips = w.chips;
        }
        let seed = ap.seed.clone();
        let rest: Vec<Track> = w
            .tracks
            .into_iter()
            .filter(|t| t.video_id != seed && !t.unavailable)
            .collect();
        let ids = rest.iter().map(|t| t.video_id.clone()).collect();
        for t in rest {
            self.known.insert(t.video_id.clone(), t);
        }
        self.player.replace_rest(ids);
        self.queue_changed();
        self.show_autoplay();
    }

    /// An autoplay choice was picked: the radio again from the song playing now.
    pub(crate) fn autoplay_chip(&mut self, i: usize) {
        let Some(chip) = self.autoplay.as_ref().and_then(|a| a.chips.get(i)).cloned() else {
            return;
        };
        let current = self
            .player
            .current()
            .and_then(|id| self.known.get(id))
            .cloned();
        if let Some(t) = current {
            self.autoplay_from(t, Some((i, chip.watch)));
        }
    }

    pub(crate) fn show_autoplay(&self) {
        let ui = self.ui();
        let g = ui.global::<Ui>();
        let (labels, picked): (Vec<SharedString>, i32) = match &self.autoplay {
            Some(a) => (
                a.chips.iter().map(|c| c.label.clone().into()).collect(),
                a.picked as i32,
            ),
            None => (Vec::new(), -1),
        };
        g.set_autoplay_chips(ModelRc::new(VecModel::from(labels)));
        g.set_autoplay_picked(picked);
    }

    /// Plays an artist's radio or shuffle.
    pub(crate) fn play_watch(&mut self, r: WatchRef, shuffle: bool) {
        self.play_seq += 1;
        let seq = self.play_seq;
        let client = self.client.clone();
        workers::run_urgent(
            move || {
                client
                    .watch_next(
                        r.video_id.as_deref(),
                        Some(&r.playlist_id),
                        r.params.as_deref(),
                    )
                    .map(|w| w.tracks)
            },
            move |res| {
                with(|a| match res {
                    Ok(tracks) => {
                        let mode = if shuffle { Mode::Shuffle } else { Mode::Play };
                        a.use_tracks(tracks, mode, seq)
                    }
                    Err(e) => log::warn!("play: {e}"),
                });
            },
        );
    }

    pub(crate) fn use_tracks(&mut self, tracks: Vec<Track>, mode: Mode, seq: u64) {
        // Queueing adds up; playing is only for the latest click.
        if matches!(mode, Mode::Play | Mode::Shuffle) && seq != self.play_seq {
            return;
        }
        match mode {
            Mode::Play => self.play_list(tracks, None, Some(false)),
            Mode::Shuffle => self.play_list(tracks, None, Some(true)),
            Mode::Next | Mode::Queue => {
                let playable = tracks.iter().filter(|t| !t.unavailable).cloned().collect();
                if self.ask_host(listen::Req::Add {
                    tracks: playable,
                    next: mode == Mode::Next,
                }) {
                    return;
                }
                let ids: Vec<String> = tracks
                    .iter()
                    .filter(|t| !t.unavailable)
                    .map(|t| t.video_id.clone())
                    .collect();
                if ids.is_empty() {
                    return;
                }
                for t in tracks {
                    self.known.insert(t.video_id.clone(), t);
                }
                if mode == Mode::Next {
                    self.player.insert_next(ids);
                } else {
                    self.player.append(ids);
                }
                self.queue_changed();
                self.show_now_playing(false);
            }
        }
    }

    /// Next (1) or previous (-1). The first press acts at once; presses that follow quickly
    /// only move along the queue (showing each song), and the one they stop at plays, so
    /// pressing a lot doesn't start and abandon a load per press.
    pub(crate) fn skip(&mut self, dir: i64) {
        if self.ask_host(if dir > 0 {
            listen::Req::Next
        } else {
            listen::Req::Previous
        }) {
            return;
        }
        if !self.skip_timer.running() {
            if dir > 0 {
                self.player.next();
            } else {
                self.player.previous();
            }
        } else if self.player.step_to(dir) {
            // What was loading (or playing) is no longer wanted.
            self.player.engine.stop();
            self.skip_pending = true;
            self.resume_at = None;
            self.show_now_playing(false);
            self.queue_changed();
        }
        self.skip_timer
            .start(TimerMode::SingleShot, Duration::from_millis(400), || {
                with(|a| {
                    if std::mem::take(&mut a.skip_pending) {
                        a.player.play_current();
                    }
                });
            });
    }

    /// Tells YouTube a song was played once 30 s of it were heard (a shorter song: once it's
    /// nearly over), so skipping through a queue doesn't fill the history.
    pub(crate) fn report_play_when_heard(&mut self, pos: Option<&music_core::audio::Position>) {
        let (Some(p), Some((id, duration))) = (pos, &self.play_report) else {
            return;
        };
        if p.id != *id {
            return;
        }
        if p.ms < heard_enough(p.duration_ms.or(*duration)) {
            return;
        }
        let id = id.clone();
        self.play_report = None;
        if !self.client.signed_in() {
            return;
        }
        let client = self.client.clone();
        workers::spawn(move || {
            if let Err(e) = client.report_play(&id) {
                log::warn!("history: {e}");
            }
        });
    }

    /// A song started being heard: the one before goes into the listening log if enough of it
    /// was. Live radio isn't logged.
    pub(crate) fn start_hearing(&mut self, id: &str, duration_ms: Option<u64>) {
        self.finish_hearing();
        if self.settings.listening_history && !radio::is_station(id) {
            self.hearing = Some(Hearing {
                id: id.to_string(),
                duration_ms,
                heard_ms: 0,
                last_ms: 0,
                last_at: std::time::Instant::now(),
                playing: false,
            });
        }
    }

    /// Adds what's been heard since the last look: the position moved on by no more than the
    /// time passed (a seek forward isn't listening; a pause isn't either).
    fn track_hearing(&mut self, pos: Option<&music_core::audio::Position>) {
        let Some(h) = &mut self.hearing else { return };
        let now = std::time::Instant::now();
        match pos {
            Some(p) if p.id == h.id => {
                if p.duration_ms.is_some() {
                    h.duration_ms = p.duration_ms;
                }
                let passed = now.duration_since(h.last_at).as_millis() as u64;
                if h.playing && p.ms >= h.last_ms && p.ms - h.last_ms <= passed + 1_500 {
                    h.heard_ms += p.ms - h.last_ms;
                }
                h.last_ms = p.ms;
                h.playing = !p.paused;
            }
            // Loading or seeking: nothing heard meanwhile.
            _ => h.playing = false,
        }
        h.last_at = now;
    }

    /// The song heard is over (another started, the queue ended, the app closes): into the log
    /// if it was heard long enough to count as played, as for YouTube's history, signed in or
    /// not.
    pub(crate) fn finish_hearing(&mut self) {
        let Some(mut h) = self.hearing.take() else {
            return;
        };
        // Last seen playing: it played on since (a minimized window isn't looked at often), up to
        // its end.
        if h.playing {
            let rest = h.duration_ms.map_or(0, |d| d.saturating_sub(h.last_ms));
            h.heard_ms += (h.last_at.elapsed().as_millis() as u64).min(rest);
        }
        if h.heard_ms < heard_enough(h.duration_ms) {
            return;
        }
        let Some(t) = self.known.get(&h.id) else {
            return;
        };
        let secs = (h.heard_ms / 1000) as u32;
        let at = music_core::paths::unix_now().saturating_sub(u64::from(secs));
        Arc::make_mut(&mut self.listens).record(t, at, secs);
        self.forget_pages(PageKind::Artists);
    }

    /// Quick skips are over: what's played now was chosen some other way.
    pub(crate) fn cancel_skip(&mut self) {
        self.skip_timer.stop();
        self.skip_pending = false;
    }

    pub(crate) fn play_pause(&mut self) {
        if self.ask_host(listen::Req::Toggle) {
            return;
        }
        self.cancel_skip();
        match self.player.engine.position() {
            // Hosting Listen Along: everyone starts again together.
            Some(p) if p.paused => {
                if !self.together_resume() {
                    self.player.engine.resume();
                    self.toggled = Some((true, std::time::Instant::now()));
                }
            }
            Some(_) => {
                self.player.engine.pause();
                self.toggled = Some((false, std::time::Instant::now()));
                self.together_paused();
            }
            None => self.player.play_current(),
        }
        self.tick();
    }

    pub(crate) fn seek(&mut self, ms: u64) {
        if !self.ask_host(listen::Req::Seek(ms)) {
            self.player.engine.seek(ms);
        }
        self.tick();
    }

    pub(crate) fn set_volume(&mut self, v: f32) {
        self.settings.volume = v;
        // Perceptual curve; the top stays bit-exact.
        self.player.engine.set_volume(v * v);
        self.save_later();
    }

    pub(crate) fn eq_changed(&mut self) {
        self.player.engine.set_eq(&self.settings.eq);
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_eq_enabled(self.settings.eq.enabled);
        g.set_eq_gains(ModelRc::new(VecModel::from(
            self.settings.eq.gains.to_vec(),
        )));
        g.set_eq_preset(self.settings.eq_preset.clone().into());
        self.save_later();
    }

    pub(crate) fn on_engine(&mut self, e: Event) {
        // A failure is shown only for the song meant to be heard, not a preload.
        let failed_now =
            matches!(&e, Event::Failed { id, .. } if self.player.current() == Some(id.as_str()));
        // Skipping quickly: a song passed over that starts after all mustn't take the queue back.
        if self.skip_pending
            && matches!(&e, Event::Started { id, .. } if self.player.current() != Some(id.as_str()))
        {
            return;
        }
        self.player.on_event(&e);
        self.together_engine(&e);
        match &e {
            Event::Started { id, duration_ms } => {
                // Before anything looks at the position: the song before is over.
                self.start_hearing(id, *duration_ms);
                // Its length, kept with the song (and the saved queue, for the bar after a
                // restart).
                if let Some(d) = duration_ms
                    && let Some(t) = self.known.get_mut(id)
                    && t.duration_ms.is_none()
                {
                    t.duration_ms = Some(*d);
                }
                self.video_sync();
                if let Some(ms) = self.resume_at.take().filter(|&ms| ms > 5000) {
                    self.player.engine.seek(ms);
                }
                self.queue_changed();
                if self.lyrics_wanted() {
                    self.lyrics_for_current();
                } else {
                    self.forget_lyrics();
                }
                // Nothing after this song (a single, a search result, a list's last song): let
                // its radio follow, once per song.
                let station = radio::is_station(id);
                if station {
                    self.station_started(id);
                }
                if !station
                    && !self.listening()
                    && self.queue_from >= self.queue_ids.len()
                    && self.player.repeat == Repeat::Off
                    && self.autoplay.as_ref().is_none_or(|ap| ap.seed != *id)
                    && let Some(t) = self.known.get(id).cloned()
                {
                    self.autoplay_from(t, None);
                }
                self.show_now_playing(true);
                // Into YouTube's history once it's really been listened to (see `tick`); live
                // radio isn't YouTube's to know about.
                self.play_report = (!station).then(|| (id.clone(), *duration_ms));
            }
            Event::Finished => {
                self.tick();
                self.finish_hearing();
            }
            Event::Failed { id, error, .. } => {
                log::warn!("{id}: {error}");
                if failed_now && radio::is_station(id) {
                    self.station_failed(id, error);
                } else if failed_now && !matches!(error, music_core::Error::Cancelled) {
                    let title = self
                        .known
                        .get(id)
                        .map_or("this song".to_string(), |t| t.title.clone());
                    let msg = match error {
                        music_core::Error::Unavailable(_) => {
                            format!("'{title}' isn't available to play")
                        }
                        music_core::Error::Net(_) => {
                            self.went_offline();
                            format!("Couldn't play '{title}': check your connection")
                        }
                        _ => format!("Couldn't play '{title}'"),
                    };
                    self.notify(msg);
                }
            }
            Event::Device(d) => log::info!("device: {d}"),
            Event::EngineError(e) => log::error!("engine: {e}"),
            Event::DeviceUnsupported(e) => log::error!("device: {e}"),
            Event::Title { id, title } => {
                self.together_title(id, title);
                self.station_title(id, title);
            }
            Event::Cueing { .. } | Event::Cued { .. } => {}
        }
    }

    /// Updates the bar, media flyout and taskbar for the current track.
    pub(crate) fn show_now_playing(&mut self, started: bool) {
        let ui = self.ui();
        let g = ui.global::<Ui>();
        let Some(t) = self
            .player
            .current()
            .and_then(|id| self.known.get(id))
            .cloned()
        else {
            g.set_np(NowPlaying::default());
            return;
        };
        let px = self.px(BAR_ART);
        let art = images::url_for(&t.thumbs, px)
            .and_then(|u| self.images.get(&u, px, image_loaded))
            .unwrap_or_default();
        let np = NowPlaying {
            active: true,
            title: t.title.clone().into(),
            artist: t.artist_line().into(),
            art,
            duration_ms: t.duration_ms.unwrap_or(0) as i32,
            position_ms: if started {
                0
            } else {
                self.resume_at.unwrap_or(0) as i32
            },
            playing: started,
            loading: false,
            liked: t.like == Some(Like::Like),
        };
        g.set_np(np);
        self.reported_playing = None;
        if let Some(m) = &self.media {
            let art_url = images::url_for(&t.thumbs, 300);
            let album = t.album.as_ref().map(|a| a.name.as_str()).unwrap_or("");
            m.track(&t.title, &t.artist_line(), album, art_url.as_deref());
        }
        self.station_now_playing(&t);
        self.tick();
    }

    /// Turns the engine's band levels into 0..1 bars: each band against its own recent peak (so
    /// quiet songs move too), jumping up at once and falling back smoothly.
    /// Every redraw is the whole window, so things that move share frames: the clock moves on
    /// every 8th tick (~4 Hz), and levels (the sung line's glow, the playing row's bars) go out
    /// ~15 times a second, or with any frame drawn anyway.
    pub(crate) fn meter_tick(&mut self) {
        self.meter_ticks = self.meter_ticks.wrapping_add(1);
        let drew = if self.meter_ticks.is_multiple_of(8) {
            self.tick();
            true
        } else {
            self.lyrics_tick()
        };
        if !self.meter_timer.running() {
            return; // `tick` stopped it
        }
        let write = drew || self.meter_ticks.is_multiple_of(2);
        let raw = self.player.engine.levels();
        for (i, (peak, shown)) in self.meter.iter_mut().enumerate() {
            let v = raw[i];
            // The peak forgets over ~10 s, so the scale follows the song.
            *peak = v.max(*peak * 0.997).max(1e-4);
            let x = (v / *peak).clamp(0.0, 1.0);
            // Bass gets a punchier curve, like the old immersive background's pulse.
            let x = if i == 0 { x * x } else { x.powf(1.4) };
            *shown = x.max(*shown * 0.82);
            if write {
                self.levels.set_row_data(i, *shown);
            }
        }
    }

    /// Fetches the playing song's lyrics for the panel, and lets a song page of it follow along.
    pub(crate) fn lyrics_for_current(&mut self) {
        let Some(id) = self.player.current().map(str::to_string) else {
            return;
        };
        for ri in 0..self.rows.len() {
            if let RowRef::Details(d) = &self.rows[ri]
                && let Some(mut row) = self.page_rows.row_data(ri)
            {
                row.live = *d == id;
                self.page_rows.set_row_data(ri, row);
            }
        }
        if self.lyrics.as_ref().is_some_and(|l| l.id == id) {
            return;
        }
        self.lyrics = Some(lyric_view::Follow::new(id.clone(), Default::default()));
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_lyrics(ModelRc::default());
        g.set_lyric_line(-1);
        g.set_lyric_pause_row(-1);
        g.set_lyrics_missing(false);
        g.set_lyrics_note("Finding lyrics…".into());
        let Some(t) = self.known.get(&id).cloned() else {
            self.no_lyrics();
            return;
        };
        let client = self.client.clone();
        workers::run_background(
            move || pages::lyrics_for(&client, &t, None),
            move |found| {
                with(|a| {
                    if a.lyrics.as_ref().is_none_or(|l| l.id != id) {
                        return;
                    }
                    let ui = a.ui();
                    let g = ui.global::<Ui>();
                    match found {
                        Some(l) => {
                            let f = lyric_view::Follow::new(id, l);
                            g.set_lyrics(ModelRc::from(f.model.clone()));
                            g.set_lyrics_note(SharedString::default());
                            a.lyrics = Some(f);
                            a.lyrics_tick();
                        }
                        None => a.no_lyrics(),
                    }
                });
            },
        );
    }

    /// The playing song has no lyrics: the lyrics controls dim, and a panel showing lyrics shows
    /// the queue instead (it stays there until Lyrics is chosen again).
    pub(crate) fn no_lyrics(&mut self) {
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_lyrics_note("No lyrics for this song.".into());
        g.set_lyrics_missing(true);
        if g.get_panel_open() && g.get_panel_tab() == 0 {
            g.set_panel_tab(1);
        }
    }

    /// Lyrics are fetched (from YouTube and, if allowed, the lyrics services) only once they've
    /// been shown this session, or while they're on screen.
    pub(crate) fn lyrics_wanted(&self) -> bool {
        let ui = self.ui();
        let g = ui.global::<Ui>();
        self.lyrics_opened || (g.get_panel_open() && g.get_panel_tab() == 0)
    }

    /// Nothing known about the playing song's lyrics (they weren't asked for).
    pub(crate) fn forget_lyrics(&mut self) {
        self.lyrics = None;
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_lyrics(ModelRc::default());
        g.set_lyric_line(-1);
        g.set_lyric_pause_row(-1);
        g.set_lyrics_missing(false);
        g.set_lyrics_note(SharedString::default());
    }

    /// Moves the lyrics to the playback position: the sung line at 4 Hz from `tick`, and its
    /// word-by-word fill at ~30 Hz from the meter timer, the fill only while lyrics are on
    /// screen (the panel's lyrics tab, or this song's page).
    /// Moves the lyrics along; whether any row changed (a frame is drawn anyway).
    pub(crate) fn lyrics_tick(&mut self) -> bool {
        if self
            .lyrics
            .as_ref()
            .is_none_or(|l| l.lyrics.lines.is_empty())
        {
            return false;
        }
        let Some(p) = self.player.engine.position() else {
            return false;
        };
        let ui = self.ui();
        let g = ui.global::<Ui>();
        let Some(f) = self.lyrics.as_mut() else {
            return false;
        };
        let page = g.get_page();
        let visible = lyrics_on_screen(&g, &page, &f.id);
        let theme = ui.global::<Theme>();
        let (sung, todo) = (theme.get_amber(), theme.get_ivory().with_alpha(0.5));
        if let Some(line) = f.at(p.ms, visible, sung, todo) {
            g.set_lyric_line(line);
        }
        let (row, through) = f.pause.map_or((-1, 0.0), |(r, k)| (r as i32, k));
        let drew = f.drew;
        // The waiting dots move ~15 times a second, unless this tick draws anyway.
        if drew || row != g.get_lyric_pause_row() || self.meter_ticks.is_multiple_of(2) {
            g.set_lyric_pause(through);
        }
        g.set_lyric_pause_row(row);
        drew
    }

    pub(crate) fn tick(&mut self) {
        let pos = self.player.engine.position();
        self.lyrics_tick();
        let mut playing = pos.as_ref().is_some_and(|p| !p.paused);
        // The engine takes a pause or resume a moment after it's asked: until it has, show what
        // was asked for, or the button flips back and the clock stops while the song plays.
        if let Some((asked, at)) = self.toggled {
            if pos.is_some() && playing != asked && at.elapsed() < Duration::from_secs(2) {
                playing = asked;
            } else {
                self.toggled = None;
            }
        }
        let ui = self.ui();
        let g = ui.global::<Ui>();
        let mut np = g.get_np();
        if let Some(p) = &pos {
            np.position_ms = p.ms as i32;
            if let Some(d) = p.duration_ms {
                np.duration_ms = d as i32;
            }
        }
        np.playing = playing;
        self.track_hearing(pos.as_ref());
        self.report_play_when_heard(pos.as_ref());
        // Compared with what Windows was told, not with `np`: a new song sets `np` first.
        if self.reported_playing != Some(playing) {
            self.reported_playing = Some(playing);
            if let Some(m) = &self.media {
                m.status(if np.active { Some(playing) } else { None });
            }
            if let Some(h) = self.hwnd {
                win::taskbar_playing(h, playing);
            }
            if !playing {
                self.save_queue();
            }
        }
        // The flyout's seek bar: once a second, and at once after a jump (a seek, a new song).
        if let Some(m) = &self.media {
            let (sec, dur) = (
                np.position_ms.max(0) as u64 / 1000,
                np.duration_ms.max(0) as u64,
            );
            let (was_sec, was_dur) = self.reported_timeline;
            if dur != was_dur || sec != was_sec {
                self.reported_timeline = (sec, dur);
                m.timeline(np.position_ms.max(0) as u64, dur);
            }
        }
        // A song that's current but not audible yet (loading, seeking, a skip) keeps the clock
        // asking for a while: its start can come without an event that would wake it again. A
        // queue restored at start isn't waited for until it's played, nor one played to its end.
        let waiting = pos.is_none()
            && self.player.current().is_some()
            && self.resume_at.is_none()
            && !self.player.ended();
        let waited = if waiting {
            self.waiting_since
                .get_or_insert_with(std::time::Instant::now)
                .elapsed()
        } else {
            self.waiting_since = None;
            Duration::ZERO
        };
        // The play button spins while it waits (not forever: 20 s, as the clock does).
        np.loading = waiting && waited < Duration::from_secs(20);
        g.set_np(np);
        if waiting && (3000..3250).contains(&waited.as_millis()) {
            log::warn!(
                "{}: no audible position after 3 s",
                self.player.current().unwrap_or_default()
            );
        }
        let want = (playing || waiting && waited < Duration::from_secs(20)) && !self.minimized;
        // The meter's timer moves the clock and the lyrics too (see `meter_tick`).
        if want && !self.meter_timer.running() {
            g.set_levels(ModelRc::from(self.levels.clone()));
            self.meter_timer
                .start(TimerMode::Repeated, Duration::from_millis(33), || {
                    with(|a| a.meter_tick());
                });
        } else if !want && self.meter_timer.running() {
            self.meter_timer.stop();
        }
    }

    pub(crate) fn remote(&mut self, r: Remote) {
        log::info!("remote: {r:?}");
        match r {
            Remote::PlayPause => self.play_pause(),
            Remote::Play => {
                if self.player.engine.position().is_none_or(|p| p.paused) {
                    self.play_pause();
                }
            }
            Remote::Pause => {
                if self.player.engine.position().is_some_and(|p| !p.paused) {
                    self.play_pause();
                }
            }
            Remote::Next => self.skip(1),
            Remote::Previous => self.skip(-1),
            Remote::Back => self.go(-1),
            Remote::Forward => self.go(1),
            Remote::Minimized => self.minimize(),
            Remote::Restored => self.restore(),
            Remote::Seek(ms) => self.seek(ms),
        }
    }
}

/// Lyrics for song `id` are showing: the panel's lyrics tab, or that song's page.
fn lyrics_on_screen(g: &Ui, page: &PageState, id: &str) -> bool {
    (g.get_panel_open() && g.get_panel_tab() == 0)
        || (page.kind == PageKind::Song && page.id.as_str() == id)
}

/// How much of a song must be heard for it to count as played: 30 s, or nearly the whole of a
/// shorter one.
fn heard_enough(duration_ms: Option<u64>) -> u64 {
    duration_ms.map_or(30_000, |d| d.saturating_sub(2_000).clamp(1_000, 30_000))
}

/// The song playing, while it's heard.
pub(crate) struct Hearing {
    id: String,
    duration_ms: Option<u64>,
    heard_ms: u64,
    /// Where it was and when, at the last look, and whether it was playing then.
    last_ms: u64,
    last_at: std::time::Instant,
    playing: bool,
}

//! The queue panel, and saving the queue and settings.

use crate::*;

impl App {
    /// Rebuilds the queue panel (the songs after the playing one) and marks the playing row on
    /// the page.
    pub(crate) fn queue_changed(&mut self) {
        // Songs still in the queue keep the artwork they have rather than blinking and loading it
        // again.
        let from = self.queue_from.min(self.queue_ids.len());
        let mut had: HashMap<String, Image> = self.queue_ids[from..]
            .iter()
            .zip(self.queue_rows.iter())
            .filter(|(_, r)| r.art.size().width > 0)
            .map(|(id, r)| (id.clone(), r.art))
            .collect();
        let (order, at) = self.player.up_next();
        self.queue_ids = order.iter().map(|s| s.to_string()).collect();
        self.queue_from = (at + 1).min(self.queue_ids.len());
        let rows: Vec<TrackRow> = self.queue_ids[self.queue_from..]
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let mut row = match self.known.get(id) {
                    Some(t) => self.row_for(t, false, true),
                    None => TrackRow {
                        title: id.clone().into(),
                        ..Default::default()
                    },
                };
                // Listen Along: who added it.
                if let Some(by) = self.added_by(id, self.queue_from + i) {
                    row.artist = format!("{} \u{b7} added by {by}", row.artist).into();
                }
                if let Some(art) = had.remove(id) {
                    row.art = art;
                }
                row
            })
            .collect();
        self.queue_rows.set_vec(rows);
        self.together_queue_changed();
        let current = self.player.current().map(str::to_string);
        for (ri, r) in self.rows.iter().enumerate() {
            let RowRef::Track(ti) = r else { continue };
            let is = current.as_deref() == Some(self.page_tracks[*ti].video_id.as_str());
            if let Some(mut row) = self.page_rows.row_data(ri)
                && row.current != is
            {
                row.current = is;
                self.page_rows.set_row_data(ri, row);
            }
        }
        let ui = self.ui();
        let g = ui.global::<Ui>();
        g.set_queue_current(at as i32);
        g.set_shuffle(self.player.shuffle());
        g.set_repeat(match self.player.repeat {
            Repeat::Off => 0,
            Repeat::All => 1,
            Repeat::One => 2,
        });
        self.save_later();
    }

    pub(crate) fn save_queue(&self) {
        let (order, at) = self.player.up_next();
        let tracks: Vec<Track> = order
            .iter()
            .filter_map(|id| {
                let mut t = self.known.get(*id).cloned()?;
                t.duration_ms = t.duration_ms.or_else(|| self.lengths.get(*id).copied());
                Some(t)
            })
            .collect();
        // Only when every song made it: a gap would shift the positions.
        let shuffled_from = if self.player.shuffle() && tracks.len() == order.len() {
            self.player.order().to_vec()
        } else {
            Vec::new()
        };
        SavedQueue {
            tracks,
            shuffled_from,
            at,
            position_ms: self
                .player
                .engine
                .position()
                .map(|p| p.ms)
                .or(self.resume_at)
                .unwrap_or(0),
            shuffle: self.player.shuffle(),
            repeat: self.player.repeat,
            autoplay: self
                .autoplay
                .as_ref()
                .map(|a| (a.seed.clone(), a.chips.clone(), a.picked)),
        }
        .save();
    }

    /// Saves settings and queue a second after the last change.
    pub(crate) fn save_later(&mut self) {
        self.save_timer
            .start(TimerMode::SingleShot, Duration::from_secs(1), || {
                with(|a| {
                    a.settings.save();
                    a.save_queue();
                });
            });
    }

    pub(crate) fn panel_changed(&mut self) {
        let ui = self.ui();
        let g = ui.global::<Ui>();
        self.settings.panel_open = g.get_panel_open();
        self.settings.panel_tab = g.get_panel_tab();
        self.save_later();
        if g.get_panel_open() && g.get_panel_tab() == 0 {
            // Lyrics asked for: from now on they're fetched for every song.
            self.lyrics_opened = true;
            self.lyrics_for_current();
        }
        // Lyrics shown now: fill the sung line at once, even while paused.
        self.lyrics_tick();
    }
}

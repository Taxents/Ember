//! "Recommended" under a playlist, like YouTube Music's suggestions: songs it hasn't got,
//! fetched after the page so it never waits on them. On your own playlist each can be added,
//! and Refresh brings a new set.

use crate::*;
use std::collections::HashSet;

/// How many show at once.
const SHOWN: usize = 10;
/// Requests one fetch may make to find that many new songs.
const TRIES: usize = 3;

#[derive(Default)]
pub(crate) struct Suggested {
    /// The page (its generation) they're for.
    generation: u64,
    /// Fetched, not shown yet.
    pool: Vec<Track>,
    next: Option<SuggestionsFrom>,
    /// What shows now, kept for the page being laid out again.
    shown: Vec<Track>,
    /// Everything shown or fetched, so a new set never repeats one.
    seen: HashSet<String>,
    /// Their rows are the page's last.
    pub(crate) on_page: bool,
    loading: bool,
    /// Refresh was asked for: the next set replaces this one when it comes.
    refreshing: bool,
    /// Songs added from here (their `keys`), on every playlist this run: YouTube can take a
    /// while to list them in the playlist, and suggests them meanwhile.
    added: HashSet<String>,
}

/// What a song is known by: its video, and its title and artist, as YouTube suggests other
/// releases of songs the playlist has (the deluxe album's, the single's).
fn keys(t: &Track) -> [String; 2] {
    let artist = t.artists.first().map(|a| a.name.as_str()).unwrap_or("");
    [
        t.video_id.clone(),
        format!(
            "{}
{}",
            t.title.to_lowercase(),
            artist.to_lowercase()
        ),
    ]
}

fn known(set: &HashSet<String>, t: &Track) -> bool {
    keys(t).iter().any(|k| set.contains(k))
}

/// Fetches until `pool` holds `SHOWN` songs that aren't in `skip` (or YouTube has no more).
fn fill(
    client: &Client,
    mut pool: Vec<Track>,
    mut next: Option<SuggestionsFrom>,
    mut skip: HashSet<String>,
) -> music_core::Result<(Vec<Track>, Option<SuggestionsFrom>)> {
    skip.extend(pool.iter().flat_map(keys));
    for _ in 0..TRIES {
        if pool.len() >= SHOWN {
            break;
        }
        let Some(from) = next.take() else { break };
        let s = client.playlist_suggestions(&from)?;
        next = s.next;
        for t in s.tracks {
            if !t.unavailable && !known(&skip, &t) {
                skip.extend(keys(&t));
                pool.push(t);
            }
        }
    }
    Ok((pool, next))
}

impl App {
    /// The playlist's own songs (their `keys`).
    fn in_playlist(&self) -> HashSet<String> {
        let mut have: HashSet<String> = self.page_tracks[..self.main_len]
            .iter()
            .flat_map(keys)
            .collect();
        have.extend(self.suggested.added.iter().cloned());
        have
    }

    /// A playlist has been laid out: fetch its recommendations, or show again the ones it had
    /// (the same page laid out afresh).
    pub(crate) fn suggest_start(&mut self, from: Option<SuggestionsFrom>) {
        if self.suggested.generation == self.generation {
            return self.suggest_show();
        }
        self.suggested = Suggested {
            generation: self.generation,
            added: std::mem::take(&mut self.suggested.added),
            ..Default::default()
        };
        if from.is_some() {
            self.suggested.next = from;
            self.suggest_fetch();
        }
    }

    /// Tops the pool up from YouTube, then shows a set if none is showing.
    fn suggest_fetch(&mut self) {
        let mut skip = self.in_playlist();
        let s = &mut self.suggested;
        if s.loading {
            return;
        }
        s.loading = true;
        skip.extend(s.seen.iter().cloned());
        let (pool, next) = (s.pool.clone(), s.next.clone());
        let generation = self.generation;
        let client = self.client.clone();
        workers::run(
            move || fill(&client, pool, next, skip),
            move |r| {
                with(|a| {
                    if a.suggested.generation != generation {
                        return;
                    }
                    let s = &mut a.suggested;
                    s.loading = false;
                    match r {
                        Ok((pool, next)) => {
                            s.seen.extend(pool.iter().flat_map(keys));
                            s.pool = pool;
                            s.next = next;
                            if std::mem::take(&mut s.refreshing) {
                                s.shown.clear();
                                a.suggest_hide();
                            }
                            a.suggest_show();
                        }
                        // Only the extras are lost: the page stands without them.
                        Err(e) => log::warn!("recommendations: {e}"),
                    }
                });
            },
        );
    }

    /// Puts the recommendations under the playlist, once the whole of it is here.
    pub(crate) fn suggest_show(&mut self) {
        if self.suggested.generation != self.generation
            || self.suggested.on_page
            || self.more.is_some()
            || self.loading_more
        {
            return;
        }
        let have = self.in_playlist();
        let s = &mut self.suggested;
        s.shown.retain(|t| !known(&have, t));
        if s.shown.is_empty() {
            s.pool.retain(|t| !known(&have, t));
            let n = s.pool.len().min(SHOWN);
            s.shown = s.pool.drain(..n).collect();
        }
        if s.shown.is_empty() {
            return;
        }
        s.on_page = true;
        let tracks = s.shown.clone();
        let refresh = !s.pool.is_empty() || s.next.is_some();
        let editable = self.header.as_ref().is_some_and(|h| h.editable);
        let current = self.player.current().map(str::to_string);
        let group = self.page_groups.iter().copied().max().unwrap_or(0) + 1;
        let heading = TrackRow {
            title: "Recommended".into(),
            suggested: refresh,
            ..Default::default()
        };
        self.push_row(RowRef::Heading(None), RowKind::Heading, heading);
        // `suggested` on the column titles: no "Plays" over the add buttons.
        let columns = TrackRow {
            suggested: true,
            ..Default::default()
        };
        self.push_row(RowRef::Columns, RowKind::Columns, columns);
        for (n, t) in tracks.into_iter().enumerate() {
            let mut row = self.row_for(&t, current.as_deref() == Some(t.video_id.as_str()), false);
            row.number = n as i32 + 1;
            row.suggested = editable;
            self.push_row(RowRef::Track(self.page_tracks.len()), RowKind::Track, row);
            self.page_tracks.push(t);
            self.page_groups.push(group);
        }
    }

    /// Takes the recommendations' rows off the page (they're its last).
    fn suggest_hide(&mut self) {
        if !self.suggested.on_page {
            return;
        }
        self.suggested.on_page = false;
        let Some(at) = self
            .rows
            .iter()
            .rposition(|r| matches!(r, RowRef::Heading(None)))
        else {
            return;
        };
        while self.rows.len() > at {
            self.rows.pop();
            self.page_rows.remove(self.page_rows.row_count() - 1);
        }
        self.art_index.clear();
        self.page_tracks.truncate(self.main_len);
        self.page_groups.truncate(self.main_len);
    }

    /// A new set, from what's fetched already or else from YouTube.
    pub(crate) fn suggestions_refresh(&mut self) {
        let have = self.in_playlist();
        let s = &mut self.suggested;
        if s.generation != self.generation {
            return;
        }
        s.pool.retain(|t| !known(&have, t));
        if s.pool.len() >= SHOWN || (s.next.is_none() && !s.pool.is_empty()) {
            s.shown.clear();
            self.suggest_hide();
            self.suggest_show();
        } else if s.next.is_some() {
            // The set showing stays until the new one comes.
            s.refreshing = true;
            self.suggest_fetch();
        }
    }

    /// Adds a recommended track to your playlist: here at once (it joins the end of the list),
    /// then on YouTube (reloading the page if that fails).
    pub(crate) fn suggestion_add(&mut self, row: usize) {
        let Some(ti) = self.track_at(row).filter(|&ti| ti >= self.main_len) else {
            return;
        };
        let r = &self.history[self.at];
        if r.kind != PageKind::Playlist || !self.header.as_ref().is_some_and(|h| h.editable) {
            return;
        }
        let playlist = r.id.trim_start_matches("VL").to_string();
        self.forget_page();
        let t = self.page_tracks.remove(ti);
        self.page_groups.remove(ti);
        self.rows.remove(row);
        self.page_rows.remove(row);
        self.art_index.clear();
        for r in self.rows.iter_mut() {
            if let RowRef::Track(j) = r
                && *j > ti
            {
                *j -= 1;
            }
        }
        // The ones after it move up a place.
        for i in row..self.rows.len() {
            if let Some(mut d) = self.page_rows.row_data(i) {
                d.number -= 1;
                self.page_rows.set_row_data(i, d);
            }
        }
        self.suggested.shown.retain(|s| s.video_id != t.video_id);
        self.suggested.added.extend(keys(&t));
        let id = t.video_id.clone();
        self.add_page_tracks(vec![t]);
        if self.suggested.shown.is_empty() {
            self.suggestions_refresh();
        }
        let client = self.client.clone();
        let generation = self.generation;
        workers::run_ordered(
            move || client.add_to_playlist(&playlist, &[id]),
            move |r| {
                if let Err(e) = r {
                    with(|a| {
                        if a.generation == generation {
                            a.reload();
                        }
                        a.account_failed("add to the playlist", &e, None);
                    });
                }
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(id: &str, title: &str, artist: &str) -> Track {
        Track {
            video_id: id.into(),
            title: title.into(),
            artists: vec![Named {
                name: artist.into(),
                id: None,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn other_releases_count_as_known() {
        let have: HashSet<String> = keys(&song("pfnnmwLR8vY", "Adaptation", "The Weeknd"))
            .into_iter()
            .collect();
        // The deluxe album's copy of a song the playlist has.
        assert!(known(
            &have,
            &song("RaOh1W_kB9g", "adaptation", "The Weeknd")
        ));
        assert!(known(
            &have,
            &song("pfnnmwLR8vY", "Adaptation (Live)", "Someone")
        ));
        assert!(!known(
            &have,
            &song("cJVpXPSXGtk", "Adaptation", "Someone else")
        ));
    }
}

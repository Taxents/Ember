//! The queue on top of the engine: what plays now, what plays next.

use crate::audio::{Engine, Event, Position};
use std::collections::HashSet;

/// What the queue needs from the engine (a recording fake in tests).
pub trait Playback {
    fn play(&self, id: &str);
    fn set_next(&self, id: Option<&str>);
    fn seek(&self, ms: u64);
    fn position(&self) -> Option<Position>;
}

impl Playback for Engine {
    fn play(&self, id: &str) {
        Engine::play(self, id)
    }
    fn set_next(&self, id: Option<&str>) {
        Engine::set_next(self, id)
    }
    fn seek(&self, ms: u64) {
        Engine::seek(self, ms)
    }
    fn position(&self) -> Option<Position> {
        Engine::position(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

pub struct Player<E = Engine> {
    pub engine: E,
    /// The list as the user made it (edits included), whatever the play order.
    queue: Vec<String>,
    /// Play order: a permutation of `queue`'s indices (in order unless shuffled).
    order: Vec<usize>,
    /// Position in `order`.
    at: usize,
    pub repeat: Repeat,
    shuffle: bool,
    seed: u64,
    /// What failed to open ahead since the last track started: not opened ahead again.
    skip: HashSet<String>,
    /// Tracks that failed in a row, so a queue where nothing plays stops instead of looping.
    failures: usize,
    /// Listening along: the queue only mirrors someone else's, and never drives the engine.
    pub follower: bool,
    /// Played to the end: nothing is audible, the current track is the one that ended.
    ended: bool,
}

impl<E: Playback> Player<E> {
    pub fn new(engine: E) -> Self {
        Player {
            engine,
            queue: Vec::new(),
            order: Vec::new(),
            at: 0,
            repeat: Repeat::Off,
            shuffle: false,
            seed: random_seed(),
            skip: HashSet::new(),
            failures: 0,
            follower: false,
            ended: false,
        }
    }

    /// Replace the queue and start playing item `start`.
    pub fn play_queue(&mut self, ids: Vec<String>, start: usize) {
        self.queue = ids;
        self.failures = 0;
        self.order = (0..self.queue.len()).collect();
        self.at = start.min(self.queue.len().saturating_sub(1));
        if self.shuffle {
            self.reshuffle();
        }
        self.play_current();
    }

    pub fn current(&self) -> Option<&str> {
        self.order.get(self.at).map(|&i| self.queue[i].as_str())
    }

    pub fn queue(&self) -> &[String] {
        &self.queue
    }

    /// The queue in play order, and the position of the current track in it.
    pub fn up_next(&self) -> (Vec<&str>, usize) {
        (
            self.order.iter().map(|&i| self.queue[i].as_str()).collect(),
            self.at,
        )
    }

    /// The queue played to its end (nothing after the last song).
    pub fn ended(&self) -> bool {
        self.ended
    }

    pub fn shuffle(&self) -> bool {
        self.shuffle
    }

    /// Where each track in play order sits in the queue as it was before shuffling.
    pub fn order(&self) -> &[usize] {
        &self.order
    }

    /// Restores a shuffled queue exactly: `queue` in its own order, `order` the play order as
    /// indices into it. False (and nothing changed) when `order` isn't a reordering of `queue`.
    pub fn restore_shuffled(&mut self, queue: Vec<String>, order: Vec<usize>, at: usize) -> bool {
        let mut seen = vec![false; queue.len()];
        if order.len() != queue.len()
            || !order
                .iter()
                .all(|&i| i < seen.len() && !std::mem::replace(&mut seen[i], true))
        {
            return false;
        }
        self.queue = queue;
        self.order = order;
        self.at = at.min(self.queue.len().saturating_sub(1));
        self.shuffle = true;
        true
    }

    /// Restores a saved queue without starting playback (`play_current` starts it).
    pub fn restore(&mut self, ids: Vec<String>, at: usize) {
        self.queue = ids;
        self.order = (0..self.queue.len()).collect();
        self.at = at.min(self.queue.len().saturating_sub(1));
    }

    /// Moves `dir` tracks on (or back) without playing: skips pressed in quick succession move
    /// along, and only where they stop gets played. Whether it moved.
    pub fn step_to(&mut self, dir: i64) -> bool {
        match self.step(dir, true) {
            Some(i) if i != self.at => {
                self.at = i;
                true
            }
            _ => false,
        }
    }

    /// Plays the current track from the start (after `restore`, or to retry).
    pub fn play_current(&mut self) {
        if self.follower {
            return;
        }
        if let Some(id) = self.current().map(str::to_string) {
            self.engine.play(&id);
        }
    }

    /// Inserts tracks right after the current one ("play next"), in the list too, so they
    /// stay there when shuffle goes off.
    pub fn insert_next(&mut self, ids: Vec<String>) {
        if self.order.is_empty() {
            return self.play_queue(ids, 0);
        }
        let first = self.order[self.at] + 1;
        let n = ids.len();
        self.queue.splice(first..first, ids);
        for i in &mut self.order {
            if *i >= first {
                *i += n;
            }
        }
        let at = self.at + 1;
        self.order.splice(at..at, first..first + n);
        self.update_next();
    }

    /// Appends tracks to the end of the queue.
    pub fn append(&mut self, ids: Vec<String>) {
        if self.order.is_empty() {
            return self.play_queue(ids, 0);
        }
        let first = self.queue.len();
        self.queue.extend(ids);
        self.order.extend(first..self.queue.len());
        self.update_next();
    }

    /// Keeps the current song playing and makes `ids` everything after it (autoplay).
    pub fn replace_rest(&mut self, ids: Vec<String>) {
        let Some(cur) = self.current().map(str::to_string) else {
            return self.play_queue(ids, 0);
        };
        self.queue = std::iter::once(cur).chain(ids).collect();
        self.order = (0..self.queue.len()).collect();
        self.at = 0;
        self.shuffle = false;
        self.update_next();
    }

    /// Plays the track at `pos` in play order.
    pub fn jump(&mut self, pos: usize) {
        if pos < self.order.len() {
            self.at = pos;
            self.play_current();
        }
    }

    /// Removes the track at `pos` in play order. The current track can't be removed.
    pub fn remove(&mut self, pos: usize) {
        if pos >= self.order.len() || pos == self.at {
            return;
        }
        let gone = self.order.remove(pos);
        self.queue.remove(gone);
        for i in &mut self.order {
            if *i > gone {
                *i -= 1;
            }
        }
        if pos < self.at {
            self.at -= 1;
        }
        self.update_next();
    }

    /// Moves the track at play-order position `from` to `to`: in the list itself, or while
    /// shuffled only in the play order (shuffle off brings back the list as it was).
    pub fn move_item(&mut self, from: usize, to: usize) {
        let n = self.order.len();
        if from >= n || to >= n || from == to {
            return;
        }
        if self.shuffle {
            let item = self.order.remove(from);
            self.order.insert(to, item);
        } else {
            // In order, play position and list index are the same.
            let id = self.queue.remove(from);
            self.queue.insert(to, id);
        }
        self.at = if self.at == from {
            to
        } else if from < self.at && to >= self.at {
            self.at - 1
        } else if from > self.at && to <= self.at {
            self.at + 1
        } else {
            self.at
        };
        self.update_next();
    }

    pub fn next(&mut self) {
        if let Some(i) = self.step(1, true) {
            self.at = i;
            self.play_current();
        }
    }

    /// Previous track, or restart this one if more than 3 s in (or played to the end).
    pub fn previous(&mut self) {
        // Live radio has nothing to restart: always the previous station.
        let live = self.current().is_some_and(crate::radio::is_station);
        let pos = self.engine.position();
        if !live && (self.ended || pos.as_ref().is_some_and(|p| p.ms > 3000))
            || self.at == 0 && self.repeat != Repeat::All
        {
            if self.follower {
                return;
            }
            // Nothing audible to seek in (the end, or a failed start): play it again.
            match pos {
                Some(_) => self.engine.seek(0),
                None => self.play_current(),
            }
            return;
        }
        if let Some(i) = self.step(-1, true) {
            self.at = i;
            self.play_current();
        }
    }

    pub fn set_shuffle(&mut self, on: bool) {
        if on == self.shuffle {
            return;
        }
        self.shuffle = on;
        let cur = self.order.get(self.at).copied();
        if on {
            self.reshuffle();
        } else {
            self.order = (0..self.queue.len()).collect();
            self.at = cur.unwrap_or(0);
        }
        self.update_next();
    }

    pub fn set_repeat(&mut self, r: Repeat) {
        self.repeat = r;
        self.update_next();
    }

    /// Keeps the queue in step with the engine. Call for every engine event.
    pub fn on_event(&mut self, e: &Event) {
        match e {
            Event::Finished => self.ended = true,
            Event::Started { id, .. } => {
                self.ended = false;
                // A gapless hand-over moved us on by itself: catch up.
                if self.current() != Some(id.as_str()) {
                    if let Some(i) = self
                        .step(1, false)
                        .filter(|&i| self.queue[self.order[i]] == *id)
                    {
                        self.at = i;
                    } else if let Some(i) = self.order.iter().position(|&q| self.queue[q] == *id) {
                        self.at = i;
                    }
                }
                self.failures = 0;
                self.skip.clear();
                self.update_next();
            }
            Event::Failed { .. } if self.follower => {}
            // The next track took over by itself; its `Started` moves the queue on.
            Event::Failed {
                id,
                handed_over: true,
                ..
            } => log::debug!("{id} broke off, the next track carries on"),
            Event::Failed { id, .. } if self.current() == Some(id.as_str()) => {
                self.failures += 1;
                if self.failures < self.order.len() {
                    self.next();
                }
            }
            Event::Failed { id, .. } => {
                self.skip.insert(id.clone());
                self.update_next();
            }
            _ => {}
        }
    }

    /// The song that follows the current one by itself, if any.
    pub fn upcoming(&self) -> Option<String> {
        (1..=self.order.len() as i64)
            .map_while(|d| self.step(d, false))
            .map(|i| self.queue[self.order[i]].clone())
            .find(|id| !self.skip.contains(id))
            .filter(|_| !self.current().is_some_and(crate::radio::is_station))
    }

    fn update_next(&mut self) {
        if self.follower {
            return;
        }
        // A station never ends by itself, so nothing is opened ahead after it.
        let next = (1..=self.order.len() as i64)
            .map_while(|d| self.step(d, false))
            .map(|i| self.queue[self.order[i]].clone())
            .find(|id| !self.skip.contains(id))
            .filter(|_| !self.current().is_some_and(crate::radio::is_station));
        self.engine.set_next(next.as_deref());
    }

    /// The order index `dir` steps away, honouring repeat. `manual` skips ignore repeat-one.
    fn step(&self, dir: i64, manual: bool) -> Option<usize> {
        let n = self.order.len();
        if n == 0 {
            return None;
        }
        if self.repeat == Repeat::One && !manual {
            return Some(self.at);
        }
        let i = self.at as i64 + dir;
        if (0..n as i64).contains(&i) {
            Some(i as usize)
        } else if self.repeat != Repeat::Off {
            Some(i.rem_euclid(n as i64) as usize)
        } else {
            None
        }
    }

    /// Fisher–Yates with the current track first.
    fn reshuffle(&mut self) {
        let cur = self.order.get(self.at).copied();
        let mut rest: Vec<usize> = (0..self.queue.len()).filter(|i| Some(*i) != cur).collect();
        for i in (1..rest.len()).rev() {
            self.seed ^= self.seed << 13;
            self.seed ^= self.seed >> 7;
            self.seed ^= self.seed << 17;
            rest.swap(i, (self.seed % (i as u64 + 1)) as usize);
        }
        self.order = cur.into_iter().chain(rest).collect();
        self.at = 0;
    }
}

/// A different shuffle every launch (and per player): the std hasher's random keys, the time
/// and the process id. Never 0, which xorshift can't leave.
fn random_seed() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    h.write_u128(now.unwrap_or_default().as_nanos());
    h.write_u32(std::process::id());
    h.finish() | 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use std::cell::RefCell;

    /// Records what the queue asks of the engine.
    #[derive(Default)]
    struct Fake {
        played: RefCell<Vec<String>>,
        next: RefCell<Option<String>>,
        seeks: RefCell<Vec<u64>>,
        pos: RefCell<Option<Position>>,
    }

    impl Playback for Fake {
        fn play(&self, id: &str) {
            self.played.borrow_mut().push(id.into());
        }
        fn set_next(&self, id: Option<&str>) {
            *self.next.borrow_mut() = id.map(Into::into);
        }
        fn seek(&self, ms: u64) {
            self.seeks.borrow_mut().push(ms);
        }
        fn position(&self) -> Option<Position> {
            self.pos.borrow().clone()
        }
    }

    fn ids(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    fn player(list: &str, start: usize) -> Player<Fake> {
        let mut p = Player::new(Fake::default());
        p.play_queue(ids(list), start);
        p
    }

    #[test]
    fn a_shuffled_queue_comes_back_as_it_was() {
        let mut p = Player::new(Fake::default());
        assert!(p.restore_shuffled(ids("a b c d"), vec![2, 0, 3, 1], 1));
        assert_eq!(shown(&p), "c [a] d b");
        assert!(p.shuffle());
        // Shuffle off: the order from before shuffling, still on the same song.
        p.set_shuffle(false);
        assert_eq!(shown(&p), "[a] b c d");
        // Not a reordering of the queue: refused, nothing changed.
        let mut q = Player::new(Fake::default());
        assert!(!q.restore_shuffled(ids("a b c"), vec![0, 0, 1], 0));
        assert!(!q.restore_shuffled(ids("a b c"), vec![0, 1], 0));
        assert!(!q.restore_shuffled(ids("a b"), vec![0, 5], 0));
        assert_eq!(shown(&q), "");
    }

    /// The queue as shown: play order, the current one in brackets.
    fn shown(p: &Player<Fake>) -> String {
        let (order, at) = p.up_next();
        order
            .iter()
            .enumerate()
            .map(|(i, id)| {
                if i == at {
                    format!("[{id}]")
                } else {
                    id.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn played(p: &Player<Fake>) -> String {
        p.engine.played.borrow().join(" ")
    }

    fn next(p: &Player<Fake>) -> Option<String> {
        p.engine.next.borrow().clone()
    }

    fn at_ms(p: &Player<Fake>, ms: u64) {
        *p.engine.pos.borrow_mut() = p.current().map(|id| Position {
            id: id.into(),
            ms,
            duration_ms: None,
            paused: false,
        });
    }

    fn started(p: &mut Player<Fake>, id: &str) {
        p.on_event(&Event::Started {
            id: id.into(),
            duration_ms: None,
        });
    }

    fn failed(p: &mut Player<Fake>, id: &str) {
        p.on_event(&Event::Failed {
            id: id.into(),
            error: Error::Media("test".into()),
            handed_over: false,
        });
    }

    #[test]
    fn plays_the_chosen_item_and_preloads_the_next() {
        let mut p = player("a b c", 1);
        assert_eq!(played(&p), "b");
        assert_eq!(shown(&p), "a [b] c");
        // What follows is opened ahead once the track is heard.
        assert_eq!(next(&p), None);
        started(&mut p, "b");
        assert_eq!(next(&p).as_deref(), Some("c"));
    }

    #[test]
    fn next_and_previous_walk_the_queue() {
        let mut p = player("a b c", 0);
        p.next();
        p.next();
        assert_eq!(played(&p), "a b c");
        assert_eq!(next(&p), None);
        // The end with repeat off: nothing more.
        p.next();
        assert_eq!(played(&p), "a b c");
        p.previous();
        assert_eq!(shown(&p), "a [b] c");
        // More than 3 s in: back to the start of this one.
        at_ms(&p, 5000);
        p.previous();
        assert_eq!(shown(&p), "a [b] c");
        assert_eq!(*p.engine.seeks.borrow(), [0]);
    }

    #[test]
    fn previous_on_the_first_item_restarts_it() {
        let mut p = player("a b", 0);
        at_ms(&p, 1000);
        p.previous();
        assert_eq!(*p.engine.seeks.borrow(), [0]);
        assert_eq!(played(&p), "a");
    }

    #[test]
    fn repeat_all_wraps_and_repeat_one_preloads_itself() {
        let mut p = player("a b", 1);
        assert_eq!(next(&p), None);
        p.set_repeat(Repeat::All);
        assert_eq!(next(&p).as_deref(), Some("a"));
        p.next();
        assert_eq!(shown(&p), "[a] b");
        p.previous();
        assert_eq!(shown(&p), "a [b]");
        p.set_repeat(Repeat::One);
        assert_eq!(next(&p).as_deref(), Some("b"));
        // A skip still moves on.
        p.next();
        assert_eq!(shown(&p), "[a] b");
    }

    #[test]
    fn shuffle_keeps_the_current_track_and_off_restores_the_order() {
        let mut p = player("a b c d e f g h", 2);
        p.set_shuffle(true);
        let (order, at) = p.up_next();
        assert_eq!((order[0], at), ("c", 0));
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted.join(" "), "a b c d e f g h");
        p.next();
        let cur = p.current().unwrap().to_string();
        p.set_shuffle(false);
        assert_eq!(p.current(), Some(cur.as_str()));
        assert_eq!(p.up_next().0.join(" "), "a b c d e f g h");
    }

    #[test]
    fn remove_and_move_keep_the_current_track() {
        let mut p = player("a b c d", 2);
        p.remove(0);
        assert_eq!(shown(&p), "b [c] d");
        // The current one can't be removed.
        p.remove(1);
        assert_eq!(shown(&p), "b [c] d");
        p.remove(2);
        assert_eq!(shown(&p), "b [c]");
        assert_eq!(next(&p), None);
        p.move_item(1, 0);
        assert_eq!(shown(&p), "[c] b");
        assert_eq!(next(&p).as_deref(), Some("b"));
        assert_eq!(played(&p), "c");
    }

    #[test]
    fn insert_next_goes_right_after_the_current_track() {
        let mut p = player("a b c", 0);
        p.insert_next(ids("x y"));
        assert_eq!(shown(&p), "[a] x y b c");
        assert_eq!(next(&p).as_deref(), Some("x"));
        p.append(ids("z"));
        assert_eq!(shown(&p), "[a] x y b c z");
    }

    #[test]
    fn a_gapless_hand_over_moves_the_queue_on() {
        let mut p = player("a b c", 0);
        started(&mut p, "a");
        started(&mut p, "b");
        assert_eq!(shown(&p), "a [b] c");
        assert_eq!(next(&p).as_deref(), Some("c"));
        // Nothing new was asked to play: the engine moved on by itself.
        assert_eq!(played(&p), "a");
    }

    #[test]
    fn a_failed_track_is_skipped_and_a_failed_preload_passed_over() {
        let mut p = player("a b c d", 0);
        failed(&mut p, "b");
        assert_eq!(next(&p).as_deref(), Some("c"));
        failed(&mut p, "a");
        assert_eq!(shown(&p), "a [b] c d");
        assert_eq!(played(&p), "a b");
    }

    #[test]
    fn a_track_that_breaks_off_into_the_next_one_needs_no_new_load() {
        let mut p = player("a b c", 0);
        started(&mut p, "a");
        p.on_event(&Event::Failed {
            id: "a".into(),
            error: Error::Media("test".into()),
            handed_over: true,
        });
        assert_eq!(played(&p), "a");
        started(&mut p, "b");
        assert_eq!(shown(&p), "a [b] c");
        assert_eq!(played(&p), "a");
    }

    #[test]
    fn a_queue_where_nothing_plays_stops() {
        let mut p = player("a b", 0);
        p.set_repeat(Repeat::All);
        failed(&mut p, "a");
        failed(&mut p, "b");
        // Two failures in a queue of two: no endless loop.
        assert_eq!(played(&p), "a b");
    }

    #[test]
    fn the_end_of_the_queue_stays_on_the_last_track() {
        let mut p = player("a b", 1);
        failed(&mut p, "b");
        assert_eq!(shown(&p), "a [b]");
        assert_eq!(played(&p), "b");
    }

    #[test]
    fn nothing_left_after_the_current_track_clears_the_preload() {
        let mut p = player("a b", 0);
        started(&mut p, "a");
        assert_eq!(next(&p).as_deref(), Some("b"));
        p.remove(1);
        assert_eq!(next(&p), None);
    }

    #[test]
    fn edits_survive_shuffle_on_and_off() {
        let mut p = player("a b c d e", 0);
        p.remove(2);
        p.move_item(3, 1);
        p.insert_next(ids("x"));
        assert_eq!(shown(&p), "[a] x e b d");
        p.set_shuffle(true);
        p.set_shuffle(false);
        assert_eq!(shown(&p), "[a] x e b d");
        assert_eq!(p.queue().join(" "), "a x e b d");
    }

    #[test]
    fn edits_while_shuffled_stay_after_shuffle_off() {
        let mut p = player("a b c d e f", 0);
        p.set_shuffle(true);
        let gone = p.up_next().0[3].to_string();
        p.remove(3);
        p.insert_next(ids("x"));
        assert_eq!(p.up_next().0[1], "x");
        p.set_shuffle(false);
        let want: Vec<&str> = ["a", "x", "b", "c", "d", "e", "f"]
            .into_iter()
            .filter(|id| *id != gone)
            .collect();
        assert_eq!(p.up_next().0, want);
        assert_eq!(p.queue().len(), 6);
    }

    #[test]
    fn a_move_while_shuffled_changes_only_the_play_order() {
        let mut p = player("a b c d", 0);
        p.set_shuffle(true);
        let moved = p.up_next().0[3].to_string();
        p.move_item(3, 1);
        assert_eq!(p.up_next().0[1], moved);
        assert_eq!(next(&p).as_deref(), Some(moved.as_str()));
        p.set_shuffle(false);
        assert_eq!(shown(&p), "[a] b c d");
    }

    #[test]
    fn each_player_shuffles_differently() {
        let list = "a b c d e f g h i j k l m n o p q r s t";
        let order = || {
            let mut p = player(list, 0);
            p.set_shuffle(true);
            p.up_next().0.join(" ")
        };
        assert_ne!(order(), order());
    }

    #[test]
    fn previous_after_the_queue_ended_replays_the_last_track() {
        let mut p = player("a b", 1);
        started(&mut p, "b");
        p.on_event(&Event::Finished);
        // The engine has nothing audible any more.
        *p.engine.pos.borrow_mut() = None;
        p.previous();
        assert_eq!(played(&p), "b b");
    }

    #[test]
    fn previous_on_a_first_track_that_never_started_tries_it_again() {
        let mut p = player("a b", 0);
        failed(&mut p, "b");
        p.previous();
        assert_eq!(played(&p), "a a");
        assert!(p.engine.seeks.borrow().is_empty());
    }
}

//! The queue on top of the engine: what plays now, what plays next.

use crate::audio::{Engine, Event};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

pub struct Player {
    pub engine: Engine,
    queue: Vec<String>,
    /// Play order: indices into `queue` (shuffled or not).
    order: Vec<usize>,
    /// Position in `order`.
    at: usize,
    pub repeat: Repeat,
    shuffle: bool,
    seed: u64,
}

impl Player {
    pub fn new(engine: Engine) -> Self {
        Player {
            engine,
            queue: Vec::new(),
            order: Vec::new(),
            at: 0,
            repeat: Repeat::Off,
            shuffle: false,
            seed: 0x9E3779B97F4A7C15,
        }
    }

    /// Replace the queue and start playing item `start`.
    pub fn play_queue(&mut self, ids: Vec<String>, start: usize) {
        self.queue = ids;
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

    pub fn shuffle(&self) -> bool {
        self.shuffle
    }

    /// Restores a saved queue without starting playback (`play_current` starts it).
    pub fn restore(&mut self, ids: Vec<String>, at: usize) {
        self.queue = ids;
        self.order = (0..self.queue.len()).collect();
        self.at = at.min(self.queue.len().saturating_sub(1));
    }

    /// Plays the current track from the start (after `restore`, or to retry).
    pub fn play_current(&mut self) {
        if let Some(id) = self.current().map(str::to_string) {
            self.engine.play(&id);
        }
    }

    /// Inserts tracks right after the current one ("play next").
    pub fn insert_next(&mut self, ids: Vec<String>) {
        if self.order.is_empty() {
            return self.play_queue(ids, 0);
        }
        let first = self.queue.len();
        let n = ids.len();
        self.queue.extend(ids);
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
        self.order.remove(pos);
        if pos < self.at {
            self.at -= 1;
        }
        self.update_next();
    }

    /// Moves the track at play-order position `from` to `to`.
    pub fn move_item(&mut self, from: usize, to: usize) {
        let n = self.order.len();
        if from >= n || to >= n || from == to {
            return;
        }
        let item = self.order.remove(from);
        self.order.insert(to, item);
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

    /// Previous track, or restart this one if more than 3 s in.
    pub fn previous(&mut self) {
        if self.engine.position().is_some_and(|p| p.ms > 3000)
            || self.at == 0 && self.repeat != Repeat::All
        {
            self.engine.seek(0);
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
            Event::Started { id, .. } => {
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
                self.update_next();
            }
            Event::Failed { id, .. } if self.current() == Some(id.as_str()) => self.next(),
            Event::Failed { .. } => self.update_next(),
            _ => {}
        }
    }

    fn update_next(&mut self) {
        let next = self
            .step(1, false)
            .map(|i| self.queue[self.order[i]].clone());
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

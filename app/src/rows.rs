//! The page's rows, laid out in Rust so the page knows its exact height.
//!
//! Slint's ListView guesses its total height from the rows on screen, which on pages that mix
//! banners, shelves and tracks makes the page jump as it scrolls. Here every row's height comes
//! from its kind and the page's measurements, so each row has a fixed place. The UI gets only
//! the rows near the view (with their place), and a Flickable as tall as the whole page.

use crate::{RowKind, TrackRow};
use slint::{Model, ModelNotify, ModelRc, ModelTracker};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// How far past the view rows are kept, so a scroll rarely waits for one.
const OVERSCAN: f32 = 600.0;

/// What row heights depend on, from the page view.
#[derive(Clone, Copy, PartialEq)]
pub struct Metrics {
    pub banner_h: f32,
    pub grid_card: f32,
    pub shelf_card: f32,
    pub view_h: f32,
    pub view_w: f32,
    /// The title row has Play and Shuffle.
    pub playable: bool,
    /// A music video's panel shows under the banner (the header row holds it).
    pub video: bool,
}

impl Metrics {
    /// The music video panel's height under the banner, as PageView draws it.
    fn video_h(&self) -> f32 {
        ((self.view_w - 68.0) * 9.0 / 16.0)
            .min(self.view_h * 0.75)
            .max(0.0)
    }
}

#[derive(Default)]
struct State {
    rows: Vec<TrackRow>,
    /// Heights the UI measured (a song's details), by row.
    measured: Vec<Option<f32>>,
    /// Where each row starts, and the page's height last.
    tops: Vec<f32>,
    /// The rows the UI has: `start..end`.
    start: usize,
    end: usize,
    scroll: f32,
}

/// The page's rows. Indices are into the whole page; the UI's model is the window near the view.
#[derive(Clone)]
pub struct PageRows(Rc<Inner>);

struct Inner {
    state: RefCell<State>,
    metrics: Cell<Metrics>,
    notify: ModelNotify,
    /// Told the page's height and row count after every change.
    on_size: RefCell<Box<dyn Fn(f32, usize)>>,
}

impl PageRows {
    pub fn new() -> Self {
        PageRows(Rc::new(Inner {
            state: Default::default(),
            metrics: Cell::new(Metrics {
                banner_h: 400.0,
                grid_card: 170.0,
                shelf_card: 176.0,
                view_h: 800.0,
                view_w: 1000.0,
                playable: false,
                video: false,
            }),
            notify: Default::default(),
            on_size: RefCell::new(Box::new(|_, _| {})),
        }))
    }

    /// The model the UI's page list shows.
    pub fn model(&self) -> ModelRc<TrackRow> {
        ModelRc::from(Rc::new(Window(self.0.clone())))
    }

    pub fn on_size(&self, f: impl Fn(f32, usize) + 'static) {
        *self.0.on_size.borrow_mut() = Box::new(f);
    }

    pub fn row_count(&self) -> usize {
        self.0.state.borrow().rows.len()
    }

    pub fn row_data(&self, i: usize) -> Option<TrackRow> {
        self.0.state.borrow().rows.get(i).cloned()
    }

    pub fn set_row_data(&self, i: usize, row: TrackRow) {
        let resized = {
            let mut s = self.0.state.borrow_mut();
            let Some(old) = s.rows.get_mut(i) else {
                return;
            };
            let m = self.0.metrics.get();
            let resized = height(old, None, &m) != height(&row, None, &m);
            *old = row;
            resized
        };
        if resized {
            self.relayout();
        } else {
            let s = self.0.state.borrow();
            if (s.start..s.end).contains(&i) {
                let at = i - s.start;
                drop(s);
                self.0.notify.row_changed(at);
            }
        }
    }

    /// Replaces every row (a new page): the view goes back to the top.
    pub fn set_vec(&self, rows: Vec<TrackRow>) {
        {
            let mut s = self.0.state.borrow_mut();
            s.measured = vec![None; rows.len()];
            s.rows = rows;
            s.start = 0;
            s.end = 0;
            s.scroll = 0.0;
        }
        self.0.notify.reset();
        self.relayout();
    }

    pub fn push(&self, row: TrackRow) {
        let n = self.row_count();
        self.insert(n, row);
    }

    pub fn insert(&self, i: usize, row: TrackRow) {
        let shown = {
            let mut s = self.0.state.borrow_mut();
            let i = i.min(s.rows.len());
            s.rows.insert(i, row);
            s.measured.insert(i, None);
            if i < s.start {
                s.start += 1;
                s.end += 1;
                None
            } else if i < s.end {
                s.end += 1;
                Some(i - s.start)
            } else {
                None
            }
        };
        if let Some(at) = shown {
            self.0.notify.row_added(at, 1);
        }
        self.relayout();
    }

    pub fn remove(&self, i: usize) -> TrackRow {
        let (row, shown) = {
            let mut s = self.0.state.borrow_mut();
            let row = s.rows.remove(i);
            s.measured.remove(i);
            let shown = if i < s.start {
                s.start -= 1;
                s.end -= 1;
                None
            } else if i < s.end {
                s.end -= 1;
                Some(i - s.start)
            } else {
                None
            };
            (row, shown)
        };
        if let Some(at) = shown {
            self.0.notify.row_removed(at, 1);
        }
        self.relayout();
        row
    }

    pub fn set_metrics(&self, m: Metrics) {
        if self.0.metrics.replace(m) != m {
            self.relayout();
        }
    }

    pub fn set_playable(&self, playable: bool) {
        let m = self.0.metrics.get();
        self.set_metrics(Metrics { playable, ..m });
    }

    /// Shows or hides the music video panel under the banner.
    pub fn set_video(&self, video: bool) {
        let m = self.0.metrics.get();
        self.set_metrics(Metrics { video, ..m });
    }

    /// The UI measured a row whose height depends on its content.
    pub fn set_measured(&self, i: usize, h: f32) {
        {
            let mut s = self.0.state.borrow_mut();
            match s.measured.get_mut(i) {
                Some(m) if *m != Some(h) => *m = Some(h),
                _ => return,
            }
        }
        self.relayout();
    }

    /// The page scrolled: `top` is how far down the view starts.
    pub fn scrolled(&self, top: f32) {
        self.0.state.borrow_mut().scroll = top.max(0.0);
        self.sync_window();
    }

    /// Recomputes every row's place, then the window and the rows whose place moved.
    fn relayout(&self) {
        let (total, count) = {
            let mut s = self.0.state.borrow_mut();
            let m = self.0.metrics.get();
            let mut tops = Vec::with_capacity(s.rows.len() + 1);
            let mut y = 0.0;
            for (r, h) in s.rows.iter().zip(&s.measured) {
                tops.push(y);
                y += height(r, *h, &m);
            }
            tops.push(y);
            s.tops = tops;
            (y, s.rows.len())
        };
        (self.0.on_size.borrow())(total, count);
        self.sync_window();
        // Places may have moved: the UI re-reads what it has.
        let n = {
            let s = self.0.state.borrow();
            s.end - s.start
        };
        for i in 0..n {
            self.0.notify.row_changed(i);
        }
    }

    /// Moves the window to the rows near the view: rows that left are dropped and rows that
    /// came are added at either end, so the rest of the UI's rows stay as they are.
    fn sync_window(&self) {
        let (old, new) = {
            let mut s = self.0.state.borrow_mut();
            let view_h = self.0.metrics.get().view_h;
            let n = s.rows.len();
            let from = s.scroll - OVERSCAN;
            let to = s.scroll + view_h + OVERSCAN;
            // The first row ending below `from`, and the first row starting below `to`.
            let start = s.tops[1..].partition_point(|&b| b <= from).min(n);
            let end = s.tops[..n].partition_point(|&t| t < to).max(start);
            let old = (s.start, s.end);
            s.start = start;
            s.end = end;
            (old, (start, end))
        };
        let ((s0, e0), (s1, e1)) = (old, new);
        if (s0, e0) == (s1, e1) {
            return;
        }
        if s1 >= e0 || e1 <= s0 || e0 == s0 {
            // A jump: nothing in common.
            self.0.notify.reset();
            return;
        }
        // The start first, then the end, counting from the new start.
        if s1 < s0 {
            self.0.notify.row_added(0, s0 - s1);
        } else if s1 > s0 {
            self.0.notify.row_removed(0, s1 - s0);
        }
        if e1 > e0 {
            self.0.notify.row_added(e0 - s1, e1 - e0);
        } else if e1 < e0 {
            self.0.notify.row_removed(e1 - s1, e0 - e1);
        }
    }
}

/// A row's height on the page. Must match how the page draws it.
fn height(r: &TrackRow, measured: Option<f32>, m: &Metrics) -> f32 {
    let tiles = r.cards.row_count() > 0 && r.cards.row_data(0).is_some_and(|c| c.tile);
    match r.kind {
        RowKind::Header => m.banner_h + 16.0 + if m.video { m.video_h() + 24.0 } else { 0.0 },
        RowKind::Title => {
            if m.playable {
                176.0
            } else {
                124.0
            }
        }
        RowKind::Heading => 60.0,
        RowKind::Shelf => {
            (if r.title.is_empty() { 8.0 } else { 52.0 })
                + if tiles {
                    m.shelf_card * 0.56
                } else {
                    m.shelf_card + 46.0
                }
                + 26.0
        }
        RowKind::Grid => {
            (if tiles {
                m.grid_card * 0.56
            } else {
                m.grid_card + 46.0
            }) + 22.0
        }
        RowKind::Footer => 56.0,
        RowKind::Columns => 22.0,
        RowKind::Details => measured.unwrap_or(200.0),
        RowKind::Track => 64.0,
        // As the row it stands in for: a track, a titled shelf or a grid line.
        RowKind::Skeleton => match r.number {
            1 => 52.0 + m.shelf_card + 46.0 + 26.0,
            2 => m.grid_card + 46.0 + 22.0,
            _ => 64.0,
        },
    }
}

/// The UI's view of the page: the rows near the view, each with its index and place.
struct Window(Rc<Inner>);

impl Model for Window {
    type Data = TrackRow;

    fn row_count(&self) -> usize {
        let s = self.0.state.borrow();
        s.end - s.start
    }

    fn row_data(&self, i: usize) -> Option<TrackRow> {
        let s = self.0.state.borrow();
        let at = s.start + i;
        if at >= s.end {
            return None;
        }
        let mut r = s.rows.get(at)?.clone();
        r.index = at as i32;
        r.y = s.tops[at];
        r.h = s.tops[at + 1] - s.tops[at];
        Some(r)
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.0.notify
    }
}

/// Rows that can be read and rewritten by index: the page's and the queue's.
pub trait RowStore {
    fn count(&self) -> usize;
    fn get(&self, i: usize) -> Option<TrackRow>;
    fn put(&self, i: usize, row: TrackRow);
}

impl RowStore for PageRows {
    fn count(&self) -> usize {
        self.row_count()
    }
    fn get(&self, i: usize) -> Option<TrackRow> {
        self.row_data(i)
    }
    fn put(&self, i: usize, row: TrackRow) {
        self.set_row_data(i, row);
    }
}

impl RowStore for slint::VecModel<TrackRow> {
    fn count(&self) -> usize {
        Model::row_count(self)
    }
    fn get(&self, i: usize) -> Option<TrackRow> {
        Model::row_data(self, i)
    }
    fn put(&self, i: usize, row: TrackRow) {
        Model::set_row_data(self, i, row);
    }
}

impl<T: RowStore + ?Sized> RowStore for Rc<T> {
    fn count(&self) -> usize {
        (**self).count()
    }
    fn get(&self, i: usize) -> Option<TrackRow> {
        (**self).get(i)
    }
    fn put(&self, i: usize, row: TrackRow) {
        (**self).put(i, row);
    }
}

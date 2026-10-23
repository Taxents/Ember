//! The lyrics view: the UI's lines, and the sung line filling word by word.
//!
//! Every line is drawn as styled text. Lines not being sung are plain bold text, dimmed by the
//! view. The sung line (and its background vocals) is rebuilt from the playback position about
//! 30 times a second while the lyrics are on screen: words already sung in the accent, the word
//! being sung filling left to right a character at a time, the rest in faded ivory.

use crate::LyricLine;
use music_core::lyrics::{self, Line, Lyrics};
use slint::{Color, Model, StyledText, VecModel};
use std::rc::Rc;

/// Lines and words start this much early, so they land with the voice.
const LEAD_MS: u64 = 150;

/// The playing song's lyrics as shown, and where the singing is.
pub struct Follow {
    pub id: String,
    pub lyrics: Lyrics,
    pub model: Rc<VecModel<LyricLine>>,
    /// The main line being sung (its row), -1 before the first or when not synced.
    pub line: i32,
    /// Rows drawn with a live fill, and the markup each has now.
    filled: Vec<(usize, String)>,
    /// A long instrumental gap: the row of the line after it, and how far through it (0..1).
    pub pause: Option<(usize, f32)>,
}

impl Follow {
    pub fn new(id: String, lyrics: Lyrics) -> Self {
        let model = Rc::new(VecModel::from(rows(&lyrics)));
        Follow {
            id,
            lyrics,
            model,
            line: -1,
            filled: Vec::new(),
            pause: None,
        }
    }

    /// Moves to playback position `ms`; returns the sung line when it changed. The fill is only
    /// redrawn when `visible`; `sung` and `todo` are its colours.
    pub fn at(&mut self, ms: u64, visible: bool, sung: Color, todo: Color) -> Option<i32> {
        let lines = &self.lyrics.lines;
        let t = ms + LEAD_MS;
        let line = if self.lyrics.synced() {
            lyrics::line_at(lines, t).map_or(-1, |i| i as i32)
        } else {
            -1
        };
        let pause = if self.lyrics.synced() {
            pause_at(lines, line, t)
        } else {
            None
        };
        let paused = pause.is_some() && self.pause.is_none();
        self.pause = pause;
        let changed = line != self.line;
        if changed || paused {
            // Back to plain; the view fades the line from all lit (`done`) into that.
            for (i, _) in std::mem::take(&mut self.filled) {
                let (done, _) = fill(&lines[i], u64::MAX, sung, todo);
                if let Some(mut r) = self.model.row_data(i) {
                    r.done = styled(&done, &r.text);
                    self.model.set_row_data(i, r);
                }
                self.set_fill(i, &plain(&lines[i].text()), "");
            }
            self.line = line;
        }
        if visible && line >= 0 && self.pause.is_none() {
            let g = line as usize;
            let group = (g..lines.len()).take_while(|&i| i == g || lines[i].background);
            for i in group {
                let (m, glow) = fill(&self.lyrics.lines[i], t, sung, todo);
                match self.filled.iter_mut().find(|f| f.0 == i) {
                    Some(f) if f.1 == m => continue,
                    Some(f) => f.1 = m.clone(),
                    None => self.filled.push((i, m.clone())),
                }
                self.set_fill(i, &m, &glow);
            }
        }
        changed.then_some(line)
    }

    fn set_fill(&self, i: usize, markup: &str, glow: &str) {
        if let Some(mut r) = self.model.row_data(i) {
            r.fill = styled(markup, &r.text);
            r.glow = styled(glow, "");
            self.model.set_row_data(i, r);
        }
    }
}

/// Gaps this long between lines show the waiting dots.
const PAUSE_MS: u64 = 4000;

/// In a long gap at `t` (before the first line, or after the sung line's group has ended): the
/// row of the next line and how far through the gap.
fn pause_at(lines: &[Line], line: i32, t: u64) -> Option<(usize, f32)> {
    let (from, next) = if line < 0 {
        (0, lines.iter().position(|l| !l.background)?)
    } else {
        let g = line as usize;
        let group = (g..lines.len()).take_while(|&i| i == g || lines[i].background);
        let end = group.map(|i| lines[i].end).max()?;
        let next = (g + 1..lines.len()).find(|&i| !lines[i].background)?;
        (end, next)
    };
    let start = lines[next].start;
    (start >= from + PAUSE_MS && t >= from + 300 && t < start)
        .then(|| (next, (t - from) as f32 / (start - from) as f32))
}

/// The UI's lines. A background line's `group` is the main line it belongs to.
pub fn rows(l: &Lyrics) -> Vec<LyricLine> {
    let mut group = 0;
    l.lines
        .iter()
        .enumerate()
        .map(|(i, x)| {
            if !x.background {
                group = i;
            }
            let text = x.text();
            LyricLine {
                fill: styled(&plain(&text), &text),
                glow: StyledText::default(),
                done: StyledText::default(),
                text: text.into(),
                ms: if l.synced() { x.start as i32 } else { -1 },
                group: group as i32,
                agent: x.agent as i32,
                background: x.background,
            }
        })
        .collect()
}

fn styled(markup: &str, text: &str) -> StyledText {
    StyledText::from_markdown(markup).unwrap_or_else(|e| {
        log::debug!("lyric markup: {e}");
        StyledText::from_plain_text(text)
    })
}

/// A line as bold text in the default colour.
fn plain(text: &str) -> String {
    if text.is_empty() {
        String::new()
    } else {
        format!("**{}**", escape(text))
    }
}

/// The line at `t`: sung words in `sung`, the word being sung filling a character at a time
/// (the edge character blended in quarter steps), the rest in `todo`. Also its glow: the same
/// line with only what's sung showing.
fn fill(line: &Line, t: u64, sung: Color, todo: Color) -> (String, String) {
    let glow = |c: Color| {
        let f = if c == sung {
            1.0
        } else if c == todo {
            0.0
        } else {
            // The blended edge: as far along as its colour is.
            let (c, a, b) = (c.to_argb_u8(), todo.to_argb_u8(), sung.to_argb_u8());
            (c.red as f32 - a.red as f32) / (b.red as f32 - a.red as f32).max(1.0)
        };
        sung.with_alpha(f.clamp(0.0, 1.0))
    };
    let mut runs: Vec<(Color, String)> = Vec::new();
    let mut push = |c: Color, s: &str| {
        if s.is_empty() {
            return;
        }
        match runs.last_mut() {
            Some(last) if last.0 == c => last.1 += s,
            _ => runs.push((c, s.to_string())),
        }
    };
    for w in &line.words {
        if t >= w.end {
            push(sung, &w.text);
        } else if t < w.start {
            push(todo, &w.text);
        } else {
            let body = w.text.trim_end();
            let chars: Vec<char> = body.chars().collect();
            let n = chars.len();
            let k = (t - w.start) as f32 / (w.end - w.start).max(1) as f32 * n as f32;
            let full = (k as usize).min(n);
            push(sung, &chars[..full].iter().collect::<String>());
            if full < n {
                let edge = ((k - full as f32) * 4.0).round() / 4.0;
                push(blend(todo, sung, edge), &chars[full].to_string());
                push(todo, &chars[full + 1..].iter().collect::<String>());
            }
            push(todo, &w.text[body.len()..]);
        }
    }
    if runs.is_empty() {
        return (String::new(), String::new());
    }
    let markup = |runs: &mut dyn Iterator<Item = (Color, &str)>| {
        let mut out = String::from("**");
        for (c, s) in runs {
            let c = c.to_argb_u8();
            out += &format!(
                "<font color=\"#{:02x}{:02x}{:02x}{:02x}\">{}</font>",
                c.red,
                c.green,
                c.blue,
                c.alpha,
                escape(s)
            );
        }
        out + "**"
    };
    (
        markup(&mut runs.iter().map(|(c, s)| (*c, s.as_str()))),
        markup(&mut runs.iter().map(|(c, s)| (glow(*c), s.as_str()))),
    )
}

/// `a` moving towards `b` by `f` (0..1).
fn blend(a: Color, b: Color, f: f32) -> Color {
    let (a, b) = (a.to_argb_u8(), b.to_argb_u8());
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * f).round() as u8;
    Color::from_argb_u8(
        mix(a.alpha, b.alpha),
        mix(a.red, b.red),
        mix(a.green, b.green),
        mix(a.blue, b.blue),
    )
}

/// Lyrics as literal markdown text: every ASCII punctuation mark escaped.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if c.is_ascii_punctuation() {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use music_core::lyrics::Word;

    fn word(start: u64, end: u64, text: &str) -> Word {
        Word {
            start,
            end,
            text: text.into(),
        }
    }

    #[test]
    fn long_gaps_wait() {
        let l = |start, end| Line {
            start,
            end,
            ..Default::default()
        };
        let lines = [l(8000, 10000), l(10000, 12000), l(20000, 22000)];
        assert_eq!(pause_at(&lines, -1, 4000).map(|p| p.0), Some(0));
        assert_eq!(pause_at(&lines, 0, 11000), None);
        assert_eq!(pause_at(&lines, 1, 12100), None);
        assert_eq!(pause_at(&lines, 1, 16000).map(|p| p.0), Some(2));
    }

    #[test]
    fn markup_parses() {
        for s in ["1. Not a list", "# not a heading", "- dash <b> & `x` *y* [z](w)", "♪"] {
            assert!(StyledText::from_markdown(&plain(s)).is_ok(), "{s}");
        }
        let line = Line {
            start: 0,
            end: 2000,
            words: vec![
                word(0, 500, "That's "),
                word(500, 1000, "es"),
                word(1000, 1500, "press"),
                word(1500, 2000, "o!"),
            ],
            ..Default::default()
        };
        let (a, b) = (Color::from_rgb_u8(233, 161, 95), Color::from_argb_u8(140, 241, 229, 210));
        for t in [0, 400, 1250, 1999, 5000] {
            let (m, g) = fill(&line, t, a, b);
            assert!(StyledText::from_markdown(&m).is_ok(), "{m}");
            assert!(StyledText::from_markdown(&g).is_ok(), "{g}");
        }
        // Halfway through "press": "pr" sung, "e" blended, "ss" and "o!" not yet.
        let (m, g) = fill(&line, 1250, a, b);
        assert!(g.contains("e9a15f00\">sso"), "{g}");
        assert!(m.contains(">That\\'s espr</font>"), "{m}");
        assert!(m.contains(">sso\\!</font>**"), "{m}");
    }
}

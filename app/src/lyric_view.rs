//! The lyrics view: the UI's lines, and the sung line filling word by word.
//!
//! Every line is drawn as styled text. Lines not being sung are plain bold text, dimmed by the
//! view. The sung line (and its background vocals) is rebuilt from the playback position about
//! 30 times a second while the lyrics are on screen: words already sung in the accent, the word
//! being sung filling left to right a character at a time, the rest in faded ivory.

use crate::{LyricLine, StageLine, StageSyl, StageWord};
use music_core::lyrics::{self, Line, Lyrics};
use slint::{Color, Model, ModelRc, StyledText, VecModel};
use std::rc::Rc;

/// Lines and words start this much early, so they land with the voice.
pub(crate) const LEAD_MS: u64 = 150;

/// The playing song's lyrics as shown, and where the singing is.
pub struct Follow {
    pub id: String,
    pub lyrics: Lyrics,
    pub model: Rc<VecModel<LyricLine>>,
    /// The same lines for the stage, word by word.
    pub stage: Rc<VecModel<StageLine>>,
    /// The main line being sung (its row), -1 before the first or when not synced.
    pub line: i32,
    /// Rows drawn with a live fill, and the markup each has now.
    filled: Vec<(usize, String)>,
    /// A long instrumental gap: the row of the line after it, and how far through it (0..1).
    pub pause: Option<(usize, f32)>,
    /// Whether the last `at` changed any row (so the window redraws this tick anyway).
    pub drew: bool,
}

impl Follow {
    pub fn new(id: String, lyrics: Lyrics) -> Self {
        let model = Rc::new(VecModel::from(rows(&lyrics)));
        let stage = Rc::new(VecModel::from(stage_rows(&lyrics)));
        Follow {
            id,
            lyrics,
            model,
            stage,
            line: -1,
            filled: Vec::new(),
            pause: None,
            drew: false,
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
        self.drew = false;
        if changed || paused {
            // Back to plain; the view fades the line from all lit (`done`) into that.
            for (i, _) in std::mem::take(&mut self.filled) {
                let (done, _) = fill(&lines[i], u64::MAX, sung, todo);
                if let Some(mut r) = self.model.row_data(i) {
                    r.done = styled(&done, &r.text);
                    self.model.set_row_data(i, r);
                }
                self.set_fill(i, &plain(&lines[i].text()), "");
                self.drew = true;
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
                self.drew = true;
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
    // When the singing before a main line ends: the previous group's end (0 before the first,
    // as `pause_at` counts it).
    let mut sung_until = 0;
    let mut seen_main = false;
    l.lines
        .iter()
        .enumerate()
        .map(|(i, x)| {
            let mut gap = 0;
            if !x.background {
                gap = x.start.saturating_sub(sung_until);
                group = i;
                sung_until = x.end;
                seen_main = true;
            } else if seen_main {
                sung_until = sung_until.max(x.end);
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
                gap: gap.min(i32::MAX as u64) as i32,
            }
        })
        .collect()
}

/// The stage's lines. Syllables are joined into words (a line wraps only between words);
/// Chinese and Japanese pieces stay apart, with no gap after them, as they run on and wrap
/// anywhere. Unsynced lyrics are a line of text each, untimed.
pub fn stage_rows(l: &Lyrics) -> Vec<StageLine> {
    let mut group = 0;
    l.lines
        .iter()
        .enumerate()
        .map(|(i, x)| {
            if !x.background {
                group = i;
            }
            let words: Vec<StageWord> = if l.synced() {
                stage_words(x)
            } else {
                vec![StageWord {
                    text: x.text().into(),
                    start: -1,
                    end: -1,
                    space: false,
                    syls: ModelRc::new(VecModel::from(vec![StageSyl {
                        text: x.text().into(),
                        start: -1,
                        end: -1,
                    }])),
                    held: false,
                }]
            };
            StageLine {
                words: ModelRc::new(VecModel::from(words)),
                start: if l.synced() { x.start as i32 } else { -1 },
                end: if l.synced() { x.end as i32 } else { -1 },
                group: group as i32,
                agent: x.agent as i32,
                background: x.background,
                translation: x.translation.as_str().into(),
                roman: x.roman.as_str().into(),
            }
        })
        .collect()
}

/// A word held at least this long (ms) swells as it's sung, its letters lifting one by one.
const HELD_MS: u64 = 1000;

fn stage_words(line: &Line) -> Vec<StageWord> {
    let ms = |t: u64| t.min(i32::MAX as u64) as i32;
    let mut out: Vec<StageWord> = Vec::new();
    // The word being joined: its syllables (text, start, end).
    let mut open: Vec<(String, u64, u64)> = Vec::new();
    for (k, w) in line.words.iter().enumerate() {
        open.push((w.text.clone(), w.start, w.end.max(w.start)));
        let last = k + 1 == line.words.len();
        let cjk = w.text.chars().any(runs_on);
        if !(w.text.ends_with(char::is_whitespace) || cjk || last) {
            continue;
        }
        let syls = std::mem::take(&mut open);
        let text: String = syls.iter().map(|s| s.0.as_str()).collect();
        let word = text.trim();
        if word.is_empty() {
            continue;
        }
        let start = syls[0].1;
        let end = syls.iter().map(|s| s.2).max().unwrap_or(start);
        let held = !cjk && end - start >= HELD_MS && word.chars().count() <= 14;
        // Trimmed like the word: the first piece's leading space, the last's trailing one.
        let n = syls.len();
        let mut pieces: Vec<StageSyl> = Vec::new();
        for (i, (t, s, e)) in syls.into_iter().enumerate() {
            let mut t = t.as_str();
            if i == 0 {
                t = t.trim_start();
            }
            if i + 1 == n {
                t = t.trim_end();
            }
            if t.is_empty() {
                continue;
            }
            if held {
                // Letters, sharing the syllable's time, so a held vowel ripples through.
                let c = t.chars().count() as u64;
                for (j, ch) in t.chars().enumerate() {
                    let j = j as u64;
                    pieces.push(StageSyl {
                        text: ch.to_string().into(),
                        start: ms(s + (e - s) * j / c),
                        end: ms(s + (e - s) * (j + 1) / c),
                    });
                }
            } else {
                pieces.push(StageSyl {
                    text: t.into(),
                    start: ms(s),
                    end: ms(e),
                });
            }
        }
        out.push(StageWord {
            text: word.into(),
            start: ms(start),
            end: ms(end),
            space: !cjk || text.ends_with(char::is_whitespace),
            syls: ModelRc::new(VecModel::from(pieces)),
            held,
        });
    }
    out
}

/// Writing that runs on without spaces between words (Chinese, Japanese).
fn runs_on(c: char) -> bool {
    matches!(c as u32, 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF)
}

/// The stage fills the words one by one: as the source timed them, or as estimated from the
/// line's time (as the panel does).
pub fn word_timed(l: &Lyrics) -> bool {
    l.synced()
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
    fn stage_joins_syllables_into_words() {
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
        let words = stage_words(&line);
        let w: Vec<_> = words
            .iter()
            .map(|w| (w.text.to_string(), w.start, w.end))
            .collect();
        assert_eq!(
            w,
            [("That's".into(), 0, 500), ("espresso!".into(), 500, 2000)]
        );
        // A short word keeps its one piece; the held one comes apart into timed letters.
        assert!(!words[0].held);
        assert_eq!(words[0].syls.row_count(), 1);
        assert_eq!(words[0].syls.row_data(0).unwrap().text, "That's");
        assert!(words[1].held);
        let letters: Vec<_> = words[1]
            .syls
            .iter()
            .map(|s| (s.text.to_string(), s.start))
            .collect();
        assert_eq!(letters.len(), 9);
        assert_eq!(letters[0], ("e".into(), 500));
        assert_eq!(letters[1], ("s".into(), 750));
        assert_eq!(letters[2], ("p".into(), 1000));
        let jp = Line {
            words: vec![
                word(0, 300, "夜"),
                word(300, 600, "空"),
                word(600, 900, "go"),
            ],
            ..Default::default()
        };
        let w = stage_words(&jp);
        assert_eq!(w.len(), 3);
        assert!(!w[0].space);
    }

    #[test]
    fn markup_parses() {
        for s in [
            "1. Not a list",
            "# not a heading",
            "- dash <b> & `x` *y* [z](w)",
            "♪",
        ] {
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
        let (a, b) = (
            Color::from_rgb_u8(233, 161, 95),
            Color::from_argb_u8(140, 241, 229, 210),
        );
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

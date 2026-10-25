//! YouTube uploads: telling songs from other videos, and links pasted into search.
//!
//! An upload carries no "this is music" flag, so its title, length and channel are scored:
//! words that name a track (`unreleased`, `prod.`, `(Extended Mix)`) count for a song, words
//! that name something else (`reaction`, `full album`, `DJ mix`) count against, and the length
//! settles the rest (a Short or an hour-long set is never a song). "Mix" goes either way: a
//! version of a track counts for, a set or a compilation against, and a bare "mix" leaves it
//! to the length.

use super::model::{Named, Track};

/// What an upload shows about itself.
#[derive(Debug, Clone, Copy, Default)]
pub struct Clues<'a> {
    pub title: &'a str,
    pub channel: &'a str,
    pub duration_ms: Option<u64>,
    /// The channel's verified-artist badge.
    pub verified: bool,
    /// Its place in the search results, from 0, when it came from one.
    pub rank: Option<usize>,
}

/// A score at or above this is a song.
const SONG: i32 = 3;

/// Words that name a track, or a copy of one.
const SONG_WORDS: &[&str] = &[
    "unreleased",
    "leak",
    "leaked",
    "snippet",
    "prod",
    "feat",
    "ft",
    "audio",
    "slowed",
    "sped",
    "reverb",
    "remix",
    "cdq",
    "hq",
    "lq",
    "og",
    "instrumental",
    "acapella",
    "vocals",
    "demo",
    "extended",
    "version",
    "mastered",
    "remastered",
    "open verse",
    "alt",
    "alternate",
    "v1",
    "v2",
    "v3",
    "v4",
    "v5",
];
/// Weaker: a lyric video or a visualizer is still the song.
const SONG_HINTS: &[&str] = &["lyrics", "lyric", "visualizer"];
/// Phrases that name something other than a song.
const VIDEO_WORDS: &[&str] = &[
    "reaction",
    "reacts",
    "reacting",
    "review",
    "interview",
    "podcast",
    "vlog",
    "tutorial",
    "how to",
    "breakdown",
    "explained",
    "explains",
    "live stream",
    "livestream",
    "full album",
    "compilation",
    "type beat",
    "playlist",
    "hours of",
    "minutes of",
    "#shorts",
    "ranking",
    "ranked",
    "tier list",
    "documentary",
    "trailer",
    "unboxing",
    "behind the scenes",
    "story of",
    "what happened",
    "full concert",
    "full set",
];
/// A music video: shown with the videos, as YouTube Music does.
const MUSIC_VIDEO: &[&str] = &[
    "music video",
    "official video",
    "(video)",
    "[video]",
    "official mv",
    "(mv)",
];
/// "Mix" as a DJ set or a compilation.
const SET_MIX: &[&str] = &[
    "dj mix",
    "dj set",
    "hour mix",
    "hr mix",
    "minute mix",
    "min mix",
    "nonstop",
    "non-stop",
    "continuous mix",
    "mixed by",
    "songs mix",
    "best of",
    "megamix",
];

/// Whether an upload is a song.
pub fn is_song(c: &Clues) -> bool {
    score(c) >= SONG
}

pub fn score(c: &Clues) -> i32 {
    let t = c.title.to_lowercase();
    let mut s = match c.duration_ms.map(|ms| ms / 1000) {
        None => -5, // live, or a premiere
        Some(..60) => -5,
        Some(60..=480) => 2,
        Some(481..=900) => -1,
        Some(_) => -5,
    };
    // "Artist - Title", however it's spaced.
    if dash(&t).is_some() {
        s += 1;
    }
    let words = SONG_WORDS.iter().filter(|w| has_word(&t, w)).count() as i32 * 2
        + SONG_HINTS.iter().filter(|w| has_word(&t, w)).count() as i32;
    s += words.min(4);
    s += version_mix(&t) * 2;
    s -= VIDEO_WORDS.iter().filter(|w| has_word(&t, w)).count() as i32 * 4;
    s -= set_mix(&t) * 4;
    if top_list(&t) {
        s -= 4;
    }
    if MUSIC_VIDEO.iter().any(|w| t.contains(w)) {
        s -= 6;
    }
    let ch = c.channel.to_lowercase();
    if ch.ends_with(" - topic") {
        s += 3;
    }
    if c.verified {
        s += 1;
    }
    // A search's first few results lean its way.
    if c.rank.is_some_and(|r| r < 5) {
        s += 1;
    }
    s
}

/// `w` in `t` with no letter or digit either side.
fn has_word(t: &str, w: &str) -> bool {
    let edge = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric());
    t.match_indices(w)
        .any(|(i, _)| edge(t[..i].chars().next_back()) && edge(t[i + w.len()..].chars().next()))
}

/// 1 for "(Extended Mix)", "[VIP Mix]", "- Radio Mix": a version of a track. A short phrase
/// ending in "mix", in brackets or after a dash, that isn't a set.
fn version_mix(t: &str) -> i32 {
    let short = |p: &str| {
        let p = p.trim();
        p.ends_with(" mix") && p.split_whitespace().count() <= 3 && set_mix(p) == 0
    };
    let bracketed = t
        .split(['(', '['])
        .skip(1)
        .filter_map(|p| p.split([')', ']']).next())
        .any(short);
    let dashed = t.rsplit_once(" - ").is_some_and(|(_, p)| short(p));
    (bracketed || dashed) as i32
}

/// 1 for "Mix" as a set: the phrases, or a year after it ("summer mix 2025").
fn set_mix(t: &str) -> i32 {
    let year = t.match_indices("mix ").any(|(i, _)| {
        let rest = &t[i + 4..];
        rest.len() >= 4
            && rest.as_bytes()[..4].iter().all(u8::is_ascii_digit)
            && rest.starts_with("20")
    });
    (year || SET_MIX.iter().any(|w| has_word(t, w))) as i32
}

/// "Top 10 …", "top 5 …".
fn top_list(t: &str) -> bool {
    t.match_indices("top ")
        .any(|(i, _)| t[i + 4..].starts_with(|c: char| c.is_ascii_digit()))
}

/// Where an upload's title splits into artist and song: the first `-`, `–` or `—` with a space
/// on at least one side ("Artist - Song", "Song -Artist"), so "Jay-Z" stays whole. Its byte
/// range, and whether it's spaced on both sides (the usual "Artist - Song").
fn dash(t: &str) -> Option<(usize, usize, bool)> {
    t.char_indices().find_map(|(i, c)| {
        if !matches!(c, '-' | '–' | '—') {
            return None;
        }
        let end = i + c.len_utf8();
        let before = t[..i].ends_with(' ');
        let after = t[end..].starts_with(' ');
        let sides = !t[..i].trim().is_empty() && !t[end..].trim().is_empty();
        ((before || after) && sides).then_some((i, end, before && after))
    })
}

/// Lowercase letters and digits, single-spaced: for comparing names.
fn norm(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// An upload's title as song and artists, when it shows them: split at its dash, the artist on
/// whichever side names one of `known` (the artists a search found) or the channel, else on the
/// left when the dash is spaced the usual way. Brackets on the artist's side belong to the song
/// ("Song -Artist (Clear Vocals)"). `None` when it can't tell: the channel stays the artist.
pub fn split_title(title: &str, channel: &str, known: &[String]) -> Option<(String, Vec<Named>)> {
    let (i, end, spaced) = dash(title)?;
    let (left, right) = (title[..i].trim(), title[end..].trim());
    let channel = channel.trim_end_matches(" - Topic");
    let names: Vec<String> = known
        .iter()
        .map(String::as_str)
        .chain([channel])
        .map(norm)
        .filter(|n| n.len() >= 2)
        .collect();
    let bare = |side: &str| {
        side.split(['(', '['])
            .next()
            .unwrap_or(side)
            .trim()
            .to_string()
    };
    let names_one = |side: &str| {
        let side = norm(&bare(side));
        names.iter().any(|n| has_word(&side, n))
    };
    // A known artist elsewhere in the title ("Song (feat. Artist) - Unreleased") says the left
    // side isn't simply the artist.
    let elsewhere = || {
        let t = norm(title);
        names.iter().any(|n| has_word(&t, n))
    };
    let artist_left = match (names_one(left), names_one(right)) {
        (false, true) => false,
        (true, _) => true,
        _ if spaced && !elsewhere() => true,
        _ => return None,
    };
    let (artist, song) = if artist_left {
        (left, right)
    } else {
        (right, left)
    };
    let who = bare(artist);
    // What follows the artist's name ("(Clear Vocals)") goes with the song.
    let extra = artist[who.len()..].trim();
    let song = if extra.is_empty() {
        song.to_string()
    } else {
        format!("{song} {extra}")
    };
    let mut parts = vec![who];
    for sep in [
        ",", "&", " x ", " X ", " ft. ", " Ft. ", " ft ", " feat. ", " Feat. ", " feat ",
    ] {
        parts = parts
            .iter()
            .flat_map(|a| a.split(sep).map(str::to_string).collect::<Vec<_>>())
            .collect();
    }
    let artists: Vec<Named> = parts
        .iter()
        .map(|a| a.trim())
        .filter(|a| !a.is_empty())
        .map(|a| Named {
            name: a.to_string(),
            id: None,
        })
        .collect();
    (!artists.is_empty() && !song.is_empty()).then_some((song, artists))
}

/// A song upload's title and artists from its title, when it shows them (see [`split_title`]).
pub fn tidy(t: &mut Track, known: &[String]) {
    let channel = t
        .artists
        .first()
        .map(|a| a.name.clone())
        .unwrap_or_default();
    if let Some((title, artists)) = split_title(&t.title, &channel, known) {
        (t.title, t.artists) = (title, artists);
    }
}

/// A YouTube or YouTube Music link pasted into search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pasted {
    Video(String),
    /// Its browse id ("VL…").
    Playlist(String),
}

/// The video or playlist a link points to. A watch link inside a playlist is the video.
pub fn pasted(q: &str) -> Option<Pasted> {
    let q = q.trim();
    let rest = q
        .strip_prefix("https://")
        .or_else(|| q.strip_prefix("http://"))
        .unwrap_or(q);
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    let rest = rest.strip_prefix("m.").unwrap_or(rest);
    let (host, path) = rest.split_once('/')?;
    let param = |name: &str| {
        let query = path.split_once('?')?.1;
        query
            .split('&')
            .find_map(|kv| kv.strip_prefix(name)?.strip_prefix('='))
            .map(|v| v.split('#').next().unwrap_or(v).to_string())
    };
    let id = |s: &str| {
        let s: String = s
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        (s.len() == 11).then_some(s)
    };
    match host {
        "youtu.be" => id(path).map(Pasted::Video),
        "youtube.com" | "music.youtube.com" => {
            if path.starts_with("watch") {
                param("v").and_then(|v| id(&v)).map(Pasted::Video)
            } else if let Some(s) = path
                .strip_prefix("shorts/")
                .or_else(|| path.strip_prefix("live/"))
                .or_else(|| path.strip_prefix("embed/"))
            {
                id(s).map(Pasted::Video)
            } else if path.starts_with("playlist") {
                param("list")
                    .filter(|l| !l.is_empty())
                    .map(|l| Pasted::Playlist(format!("VL{l}")))
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(title: &str, secs: u64) -> bool {
        is_song(&Clues {
            title,
            channel: "someone",
            duration_ms: Some(secs * 1000),
            ..Default::default()
        })
    }

    #[test]
    fn songs() {
        assert!(song("Playboi Carti - Skeleton (Unreleased)", 184));
        assert!(song(
            "playboi carti - SURVIVOR (PROD. F1LTHY) [UNRELEASED]",
            155
        ));
        assert!(song("Playboi Carti - Play This (Different Type Hoes)", 86));
        assert!(song("Playboi Carti UNRELEASED-Molly(lyrics)", 143));
        assert!(song("bando (prod.ta2cute x yugen)", 186));
        assert!(song(
            "Playboi Carti - Locked In/Diamonds (Full LQ Audio) (4K)",
            161
        ));
        assert!(song("Artist - Track (Extended Mix)", 400));
        assert!(song("Artist - Track [VIP Mix]", 240));
        assert!(song("Artist - Track - Radio Mix", 200));
        assert!(!song("Some Song", 200));
        assert!(song("Never See Me Again -Kanye West (Clear Vocals)", 260));
        assert!(song("Never See Me Again (Final Version)", 260));
        assert!(song("Artist - Song v2", 200));
    }

    #[test]
    fn videos() {
        assert!(!song("Playboi Carti - WITHDRAWALS (HQ Music Video)", 241));
        assert!(!song("Playboi Carti - Cancun (Official Video)", 140));
        assert!(!song("playboi carti's hardest songs mix v1", 2479));
        assert!(!song(
            "12 minutes of mix playboi carti, that will let you float",
            777
        ));
        assert!(!song("Artist - Song REACTION", 300));
        assert!(!song("My Top 10 Unreleased Songs", 300));
        assert!(!song("Artist - Song #shorts", 30));
        assert!(!song("Summer Mix 2025 - Artist", 300));
        assert!(!song("Artist - Live DJ Set", 420));
    }

    #[test]
    fn a_bare_mix_goes_by_its_length() {
        assert!(song("Artist - Track (mix)", 220));
        assert!(!song("Artist - Track mix", 3000));
    }

    #[test]
    fn topic_channels_and_the_top_results_lean_song() {
        let c = Clues {
            title: "Some Song",
            channel: "Artist - Topic",
            duration_ms: Some(200_000),
            ..Default::default()
        };
        assert!(is_song(&c));
        assert!(is_song(&Clues {
            channel: "x",
            rank: Some(0),
            ..c
        }));
    }

    #[test]
    fn words_need_their_edges() {
        assert!(has_word("x (prod. y)", "prod"));
        assert!(!has_word("production", "prod"));
        assert!(!has_word("often", "ft"));
    }

    fn split(title: &str, channel: &str, known: &[&str]) -> Option<(String, Vec<String>)> {
        let known: Vec<String> = known.iter().map(|k| k.to_string()).collect();
        split_title(title, channel, &known)
            .map(|(t, a)| (t, a.into_iter().map(|n| n.name).collect()))
    }

    #[test]
    fn titles_split_at_the_dash() {
        let (t, a) = split("Playboi Carti - Skeleton (Unreleased)", "zeuven", &[]).unwrap();
        assert_eq!(
            (t.as_str(), a[0].as_str()),
            ("Skeleton (Unreleased)", "Playboi Carti")
        );
        let (_, a) = split("A x B & C - Song", "ch", &[]).unwrap();
        assert_eq!(a, ["A", "B", "C"]);
        let (_, a) = split("Kanye West ft. Jay-Z - Song", "ch", &[]).unwrap();
        assert_eq!(a, ["Kanye West", "Jay-Z"]);
        // No dash: nothing to go on.
        assert_eq!(split("bando (prod. x)", "Artist - Topic", &[]), None);
        // A hyphen inside a word isn't a dash.
        assert_eq!(
            split("Playboi Carti UNRELEASED-Molly(lyrics)", "ch", &[]),
            None
        );
        assert_eq!(split("Jay-Z - Song", "x", &[]).unwrap().1, ["Jay-Z"]);
    }

    #[test]
    fn messy_titles_find_the_artist() {
        let want = Some((
            "Never See Me Again (Clear Vocals)".to_string(),
            vec!["Kanye West".to_string()],
        ));
        let t = "Never See Me Again -Kanye West (Clear Vocals)";
        // A search for him found him: he's on the right, whatever the spacing.
        assert_eq!(split(t, "uploader", &["Kanye West"]), want);
        assert_eq!(
            split(
                "Never See Me Again - Kanye West (Clear Vocals)",
                "x",
                &["Kanye West"]
            ),
            want
        );
        // The channel is the artist.
        assert_eq!(split(t, "Kanye West - Topic", &[]), want);
        // Known on the left, any spacing.
        let (s, a) = split("Kanye West- Never See Me Again", "x", &["kanye west"]).unwrap();
        assert_eq!(
            (s.as_str(), a[0].as_str()),
            ("Never See Me Again", "Kanye West")
        );
        assert_eq!(
            split("Song — Artist", "x", &["Artist"]).unwrap().1,
            ["Artist"]
        );
        // Badly spaced and nothing known: left alone rather than guessed.
        assert_eq!(split(t, "uploader", &[]), None);
        // The artist only in a "feat.": the left side is the song, not the artist.
        assert_eq!(
            split(
                "We Are Young (feat. Kanye West & JAY-Z) - Unreleased Watch The Throne II",
                "x",
                &["Kanye West"]
            ),
            None
        );
    }

    #[test]
    fn links() {
        let v = |s: &str| Some(Pasted::Video(s.into()));
        assert_eq!(
            pasted("https://www.youtube.com/watch?v=r6Vgu5ViOMY"),
            v("r6Vgu5ViOMY")
        );
        assert_eq!(
            pasted("https://youtu.be/r6Vgu5ViOMY?si=abc"),
            v("r6Vgu5ViOMY")
        );
        assert_eq!(
            pasted("music.youtube.com/watch?v=r6Vgu5ViOMY&list=RDAMVM"),
            v("r6Vgu5ViOMY")
        );
        assert_eq!(
            pasted("https://youtube.com/shorts/r6Vgu5ViOMY"),
            v("r6Vgu5ViOMY")
        );
        assert_eq!(
            pasted("https://www.youtube.com/playlist?list=PLabc"),
            Some(Pasted::Playlist("VLPLabc".into()))
        );
        assert_eq!(pasted("playboi carti"), None);
        assert_eq!(pasted("https://example.com/watch?v=r6Vgu5ViOMY"), None);
    }
}

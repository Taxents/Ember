//! Station logos. Radio Garden has none, so they come from radio-browser.info (a free,
//! community-kept station list), matched by name and country.
//!
//! What a lookup found is kept on disk: the logo's bytes as `<channel>.img`, or an empty
//! `<channel>.none` when the list answered and has none, tried again after a fortnight. A
//! lookup that couldn't ask (offline, the list down) remembers nothing and is tried next time.

use super::Station;
use std::path::PathBuf;
use std::time::Duration;

const API: &str = "https://all.api.radio-browser.info/json/stations/search";
const RETRY: Duration = Duration::from_secs(14 * 24 * 3600);

fn dir() -> PathBuf {
    let d = crate::paths::cache_dir().join("radio-logos");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// The station's logo as image bytes (PNG, JPEG or WebP), if one can be found. Blocks.
pub fn logo(st: &Station) -> Option<Vec<u8>> {
    if !super::valid_id(&st.id) {
        return None;
    }
    let img = dir().join(format!("{}.img", st.id));
    let none = dir().join(format!("{}.none", st.id));
    if let Ok(b) = std::fs::read(&img) {
        return Some(b);
    }
    let tried_lately = std::fs::metadata(&none)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < RETRY);
    if tried_lately {
        return None;
    }
    match find(st) {
        Found::Logo(b) => {
            let _ = crate::paths::write_atomic(&img, &b);
            let _ = std::fs::remove_file(&none);
            Some(b)
        }
        Found::None => {
            let _ = std::fs::write(&none, b"");
            None
        }
        Found::Unknown => None,
    }
}

/// What a lookup learned.
#[derive(Debug, PartialEq)]
enum Found {
    Logo(Vec<u8>),
    /// The list answered: it has no logo we can use.
    None,
    /// It couldn't be asked (or a logo couldn't be fetched): try again another time.
    Unknown,
}

/// What one answer of the list (or its failure) and the fetched favicons add up to.
fn outcome(answers: &[Option<Vec<Fetched>>]) -> Found {
    let mut unsure = false;
    for a in answers {
        match a {
            None => unsure = true,
            Some(fetched) => {
                for f in fetched {
                    match f {
                        Fetched::Image(b) => return Found::Logo(b.clone()),
                        Fetched::Failed => unsure = true,
                        Fetched::Unusable => {}
                    }
                }
            }
        }
    }
    if unsure { Found::Unknown } else { Found::None }
}

fn find(st: &Station) -> Found {
    // Its full name first, then without the frequency ("Urgent FM 105.3" is "Urgent" there).
    let mut names = vec![st.title.clone()];
    let bare = bare(&st.title);
    if !bare.is_empty() && bare != st.title {
        names.push(bare);
    }
    let mut answers = Vec::new();
    for name in &names {
        let url = format!(
            "{API}?name={}&country={}&order=votes&reverse=true&hidebroken=true&limit=20",
            enc(name),
            enc(&st.country)
        );
        let Some(list) = crate::net::agent()
            .get(&url)
            .header("User-Agent", "Ember/1.0")
            .call()
            .ok()
            .and_then(|mut r| r.body_mut().read_json::<serde_json::Value>().ok())
        else {
            answers.push(None);
            continue;
        };
        let want = squash(name);
        let mut favicons: Vec<&str> = Vec::new();
        for exact in [true, false] {
            for s in list.as_array().into_iter().flatten() {
                let got = squash(s["name"].as_str().unwrap_or_default());
                let fits = if exact {
                    got == want
                } else {
                    got.starts_with(&want) || want.starts_with(&got) && got.len() >= 4
                };
                if let Some(f) = s["favicon"].as_str().filter(|f| fits && !f.is_empty())
                    && !favicons.contains(&f)
                {
                    favicons.push(f);
                }
            }
        }
        let mut fetched = Vec::new();
        for f in favicons.iter().take(3) {
            let got = fetch(f);
            let done = matches!(got, Fetched::Image(_));
            fetched.push(got);
            if done {
                break;
            }
        }
        answers.push(Some(fetched));
        if let Found::Logo(b) = outcome(&answers) {
            return Found::Logo(b);
        }
    }
    outcome(&answers)
}

/// One favicon fetch.
#[derive(Debug, PartialEq)]
enum Fetched {
    Image(Vec<u8>),
    /// Answered, but not with an image we can draw (or not at all: 404 and the like).
    Unusable,
    /// Couldn't be fetched (network).
    Failed,
}

/// A station's name without its frequency and band: "Radio Plus Gent 107.7 FM" → "Radio Plus Gent".
fn bare(name: &str) -> String {
    name.split_whitespace()
        .filter(|w| {
            w.parse::<f32>().is_err()
                && !["fm", "am", "dab", "dab+", "mhz"].contains(&w.to_ascii_lowercase().as_str())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The image at `url`, if it's one we can draw (Wikimedia SVGs come as PNG thumbnails).
fn fetch(url: &str) -> Fetched {
    let url = wikimedia_png(url).unwrap_or_else(|| url.to_string());
    let mut r = match crate::net::agent()
        .get(&url)
        .header("User-Agent", "Ember/1.0")
        .call()
    {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(_)) => return Fetched::Unusable,
        Err(_) => return Fetched::Failed,
    };
    if r.status().as_u16() != 200 {
        return Fetched::Unusable;
    }
    let Ok(b) = r
        .body_mut()
        .with_config()
        .limit(4 * 1024 * 1024)
        .read_to_vec()
    else {
        return Fetched::Failed;
    };
    let png = b.starts_with(b"\x89PNG");
    let jpeg = b.starts_with(b"\xFF\xD8");
    let webp = b.len() > 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP";
    if png || jpeg || webp {
        Fetched::Image(b)
    } else {
        Fetched::Unusable
    }
}

/// upload.wikimedia.org/…/commons/0/08/X.svg → the 512px PNG Wikimedia renders of it.
fn wikimedia_png(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://upload.wikimedia.org/wikipedia/")?;
    let (project, path) = rest.split_once('/')?;
    let name = path.rsplit('/').next()?;
    name.to_ascii_lowercase().ends_with(".svg").then(|| {
        format!("https://upload.wikimedia.org/wikipedia/{project}/thumb/{path}/512px-{name}.png")
    })
}

/// Lower case letters and digits only: "Radio 1 (VRT)" and "radio1 vrt" match.
fn squash(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_urls() {
        assert_eq!(squash("VRT Radio 1"), "vrtradio1");
        assert_eq!(enc("Radio Nova"), "Radio%20Nova");
        assert_eq!(bare("Radio Plus Gent 107.7 FM"), "Radio Plus Gent");
        assert_eq!(bare("Urgent FM 105.3"), "Urgent");
        assert_eq!(
            wikimedia_png(
                "https://upload.wikimedia.org/wikipedia/commons/0/08/VRT_Radio_1_logo.svg"
            )
            .as_deref(),
            Some(
                "https://upload.wikimedia.org/wikipedia/commons/thumb/0/08/VRT_Radio_1_logo.svg/512px-VRT_Radio_1_logo.svg.png"
            )
        );
        assert_eq!(wikimedia_png("https://example.com/a.png"), None);
    }

    #[test]
    fn none_is_remembered_only_when_the_list_answered() {
        use Fetched::*;
        let logo = || Image(b"\x89PNG".to_vec());
        assert_eq!(outcome(&[Some(vec![])]), Found::None, "no match");
        assert_eq!(
            outcome(&[Some(vec![Unusable])]),
            Found::None,
            "a broken favicon"
        );
        assert_eq!(outcome(&[None]), Found::Unknown, "couldn't ask");
        assert_eq!(outcome(&[Some(vec![]), None]), Found::Unknown);
        assert_eq!(
            outcome(&[Some(vec![Failed])]),
            Found::Unknown,
            "offline mid-way"
        );
        assert_eq!(
            outcome(&[None, Some(vec![Failed, logo()])]),
            Found::Logo(b"\x89PNG".to_vec())
        );
    }
}

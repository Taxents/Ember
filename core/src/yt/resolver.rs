//! Stream resolver: videoId → a plain audio URL.
//!
//! Uses the signed-out `VISIONOS` InnerTube client with a cached `visitorData`. Its `/player`
//! response carries plain (unciphered) URLs, so no JavaScript and no PO token are needed.
//! Measured in spike B: 165–301 ms to the first byte, URLs valid ~6 h.
//!
//! The same response lists the video-only formats too: the cheap-to-decode ones (H.264 MP4, at
//! most 720p) are kept alongside, for showing a music video's picture.

use crate::paths::{cache_dir, unix_now, write_atomic};
use crate::{Error, Result, net};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;

const PLAYER_URL: &str = "https://www.youtube.com/youtubei/v1/player?prettyPrint=false";
const VISITOR_URL: &str = "https://music.youtube.com/youtubei/v1/visitor_id?prettyPrint=false";
const CLIENT_NAME_ID: &str = "101";
const CLIENT_VERSION: &str = "1.02";
const CLIENT_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) AppleWebKit/605.1.15 \
     (KHTML, like Gecko) Version/26.0 Safari/605.1.15";
/// visitorData is refreshed daily, or immediately when YouTube rejects a request.
const VISITOR_TTL_S: u64 = 24 * 3600;
/// A cached URL is used only while it has at least this long left.
const URL_MARGIN_S: u64 = 30 * 60;
/// Preferred audio formats, best first: Opus ~160k, Opus ~70k, AAC 128k.
const ITAGS: [u32; 3] = [251, 250, 140];

/// Video-only formats are taken up to this height (H.264 MP4 only: Windows decodes it in
/// hardware everywhere).
const VIDEO_MAX_H: u64 = 720;

/// One playable audio stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamInfo {
    pub video_id: String,
    pub url: String,
    pub itag: u32,
    /// e.g. `audio/webm; codecs="opus"`.
    pub mime: String,
    pub content_length: Option<u64>,
    pub duration_ms: Option<u64>,
    pub sample_rate: Option<u32>,
    /// Unix seconds.
    pub expires_at: u64,
}

impl StreamInfo {
    pub fn is_webm(&self) -> bool {
        self.mime.starts_with("audio/webm")
    }
}

/// A video-only (picture, no sound) stream: H.264 in MP4.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoInfo {
    pub video_id: String,
    pub url: String,
    pub itag: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub content_length: Option<u64>,
    /// Unix seconds.
    pub expires_at: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct Disk {
    visitor: Option<(String, u64)>,
    streams: HashMap<String, StreamInfo>,
    /// Video formats from the same responses (empty: none we can use).
    #[serde(default)]
    video_formats: HashMap<String, Vec<VideoInfo>>,
}

pub struct Resolver {
    state: Mutex<Disk>,
    path: std::path::PathBuf,
    itags: Vec<u32>,
}

impl Resolver {
    /// Loads the cached visitorData and URLs from disk (dropping expired ones).
    pub fn new() -> Self {
        let path = cache_dir().join("streams.json");
        let mut disk: Disk = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let now = unix_now();
        disk.streams
            .retain(|_, s| s.expires_at > now + URL_MARGIN_S);
        // "No usable video" is remembered as long as the audio it came with.
        let audio = &disk.streams;
        disk.video_formats.retain(|id, v| match v.first() {
            Some(v) => v.expires_at > now + URL_MARGIN_S,
            None => audio.contains_key(id),
        });
        Resolver {
            state: Mutex::new(disk),
            path,
            itags: ITAGS.to_vec(),
        }
    }

    /// Restrict and order the formats to pick from (testing the AAC fallback, for one).
    pub fn with_itags(mut self, itags: &[u32]) -> Self {
        self.itags = itags.to_vec();
        self
    }

    /// A playable stream for `video_id`, from cache when still fresh.
    pub fn resolve(&self, video_id: &str) -> Result<StreamInfo> {
        if let Some(s) = self.cached(video_id) {
            return Ok(s);
        }
        self.refresh(video_id)
    }

    /// Always asks YouTube (used when a cached URL turned out to be dead).
    pub fn refresh(&self, video_id: &str) -> Result<StreamInfo> {
        self.fetch(video_id).map(|(a, _)| a)
    }

    /// The picture of a music video: a video-only stream at least `height` pixels tall when
    /// there is one (else the tallest), from cache when still fresh. It comes with the audio's
    /// `/player` response, so usually no request is needed.
    /// `Ok(None)`: the video has no format we can decode.
    pub fn resolve_video(&self, video_id: &str, height: u32) -> Result<Option<VideoInfo>> {
        let cached = {
            let st = self.state.lock().unwrap();
            st.video_formats
                .get(video_id)
                .filter(|v| v.first().is_none_or(|v| v.expires_at > unix_now() + URL_MARGIN_S))
                .cloned()
        };
        let formats = match cached {
            Some(f) => f,
            None => self.fetch(video_id)?.1,
        };
        Ok(pick_video(&formats, height))
    }

    /// Forget a cached video URL (after it was refused).
    pub fn invalidate_video(&self, video_id: &str) {
        let mut st = self.state.lock().unwrap();
        if st.video_formats.remove(video_id).is_some() {
            self.save(&mut st);
        }
    }

    /// Asks YouTube for the streams and caches them.
    fn fetch(&self, video_id: &str) -> Result<(StreamInfo, Vec<VideoInfo>)> {
        let visitor = self.visitor(false)?;
        let (info, video) = match self.player(video_id, &visitor) {
            Ok(i) => i,
            // A stale visitorData is the usual reason for a refusal: retry once with a fresh one.
            Err(Error::Net(_)) | Err(Error::Unavailable(_)) => {
                let visitor = self.visitor(true)?;
                self.player(video_id, &visitor)?
            }
            Err(e) => return Err(e),
        };
        // A resolver restricted to other formats (testing) doesn't touch the shared cache.
        if self.itags == ITAGS {
            let mut st = self.state.lock().unwrap();
            st.streams.insert(video_id.to_string(), info.clone());
            st.video_formats.insert(video_id.to_string(), video.clone());
            self.save(&mut st);
        }
        Ok((info, video))
    }

    /// Forget a cached URL (e.g. after a 403).
    pub fn invalidate(&self, video_id: &str) {
        let mut st = self.state.lock().unwrap();
        if st.streams.remove(video_id).is_some() {
            self.save(&mut st);
        }
    }

    fn cached(&self, video_id: &str) -> Option<StreamInfo> {
        let st = self.state.lock().unwrap();
        st.streams
            .get(video_id)
            .filter(|s| s.expires_at > unix_now() + URL_MARGIN_S && self.itags.contains(&s.itag))
            .cloned()
    }

    fn visitor(&self, force: bool) -> Result<String> {
        {
            let st = self.state.lock().unwrap();
            if let Some((v, at)) = &st.visitor
                && !force
                && unix_now() < at + VISITOR_TTL_S
            {
                return Ok(v.clone());
            }
        }
        let body = json!({"context": {"client": {
            "clientName": "WEB_REMIX", "clientVersion": "1.20260707.12.00", "hl": "en", "gl": "US"
        }}});
        let v = net::post_json(
            VISITOR_URL,
            &[
                ("User-Agent", net::BROWSER_UA),
                ("Origin", "https://music.youtube.com"),
            ],
            &body,
        )?;
        let data = v["responseContext"]["visitorData"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Net("no visitorData in response".into()))?
            .to_string();
        let mut st = self.state.lock().unwrap();
        st.visitor = Some((data.clone(), unix_now()));
        self.save(&mut st);
        Ok(data)
    }

    fn player(&self, video_id: &str, visitor: &str) -> Result<(StreamInfo, Vec<VideoInfo>)> {
        let body = json!({
            "context": {"client": {
                "clientName": "VISIONOS", "clientVersion": CLIENT_VERSION,
                "deviceMake": "Apple", "deviceModel": "RealityDevice17,1",
                "userAgent": CLIENT_UA, "osName": "visionOS", "osVersion": "26.5.23O471",
                "hl": "en", "gl": "US", "visitorData": visitor,
            }},
            "videoId": video_id,
            "playbackContext": {"contentPlaybackContext": {"html5Preference": "HTML5_PREF_WANTS"}},
            "contentCheckOk": true,
            "racyCheckOk": true,
        });
        let v = net::post_json(
            PLAYER_URL,
            &[
                ("User-Agent", CLIENT_UA),
                ("X-YouTube-Client-Name", CLIENT_NAME_ID),
                ("X-YouTube-Client-Version", CLIENT_VERSION),
                ("Origin", "https://www.youtube.com"),
                ("X-Goog-Visitor-Id", visitor),
            ],
            &body,
        )?;
        let audio = parse_player(video_id, &v, &self.itags)?;
        Ok((audio, parse_video(video_id, &v)))
    }

    fn save(&self, st: &mut Disk) {
        // Written on every resolve, so it only keeps what's still usable.
        let now = unix_now();
        st.streams.retain(|_, s| s.expires_at > now + URL_MARGIN_S);
        let audio = &st.streams;
        st.video_formats.retain(|id, v| match v.first() {
            Some(v) => v.expires_at > now + URL_MARGIN_S,
            None => audio.contains_key(id),
        });
        if let Ok(b) = serde_json::to_vec(&*st) {
            let _ = write_atomic(&self.path, &b);
        }
    }
}

impl Default for Resolver {
    fn default() -> Self {
        Self::new()
    }
}

/// Picks the best audio format out of a `/player` response.
fn parse_player(video_id: &str, v: &Value, itags: &[u32]) -> Result<StreamInfo> {
    let ps = &v["playabilityStatus"];
    let status = ps["status"].as_str().unwrap_or("");
    if status != "OK" {
        let reason = ps["reason"].as_str().unwrap_or(status);
        return Err(Error::Unavailable(format!("{video_id}: {reason}")));
    }
    let sd = &v["streamingData"];
    let expires_in = sd["expiresInSeconds"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(6 * 3600);
    let formats = sd["adaptiveFormats"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let num = |f: &Value, k: &str| f[k].as_str().and_then(|s| s.parse::<u64>().ok());
    for &want in itags {
        let Some(f) = formats
            .iter()
            .find(|f| f["itag"].as_u64() == Some(want as u64))
        else {
            continue;
        };
        let Some(url) = f["url"].as_str() else {
            continue;
        };
        return Ok(StreamInfo {
            video_id: video_id.to_string(),
            url: url.to_string(),
            itag: want,
            mime: f["mimeType"].as_str().unwrap_or("").to_string(),
            content_length: num(f, "contentLength").filter(|&n| n > 0),
            duration_ms: num(f, "approxDurationMs"),
            sample_rate: num(f, "audioSampleRate").map(|r| r as u32),
            expires_at: unix_now() + expires_in,
        });
    }
    Err(Error::Unavailable(format!(
        "{video_id}: no usable audio format"
    )))
}

/// The video-only formats a music video can be shown with: H.264 MP4 (hardware-decoded
/// everywhere) up to 720p.
fn parse_video(video_id: &str, v: &Value) -> Vec<VideoInfo> {
    let sd = &v["streamingData"];
    let expires_in = sd["expiresInSeconds"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(6 * 3600);
    let num = |f: &Value, k: &str| f[k].as_str().and_then(|s| s.parse::<u64>().ok());
    sd["adaptiveFormats"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter(|f| {
            let mime = f["mimeType"].as_str().unwrap_or("");
            mime.starts_with("video/mp4") && mime.contains("avc1") && f["url"].is_string()
        })
        .filter_map(|f| {
            let height = f["height"].as_u64().filter(|&h| h > 0 && h <= VIDEO_MAX_H)? as u32;
            Some(VideoInfo {
                video_id: video_id.to_string(),
                url: f["url"].as_str()?.to_string(),
                itag: f["itag"].as_u64()? as u32,
                width: f["width"].as_u64().unwrap_or(0) as u32,
                height,
                fps: f["fps"].as_u64().unwrap_or(30) as u32,
                content_length: num(f, "contentLength").filter(|&n| n > 0),
                expires_at: unix_now() + expires_in,
            })
        })
        .collect()
}

/// The format to show at `height` pixels: ≤ 30 fps when there is one (half the decode work, and
/// plenty for a panel), the smallest at least `height` tall, else the tallest.
fn pick_video(formats: &[VideoInfo], height: u32) -> Option<VideoInfo> {
    let calm = formats.iter().any(|f| f.fps <= 30);
    let ok: Vec<&VideoInfo> = formats
        .iter()
        .filter(|f| !calm || f.fps <= 30)
        .collect();
    ok.iter()
        .filter(|f| f.height >= height)
        .min_by_key(|f| f.height)
        .or_else(|| ok.iter().max_by_key(|f| f.height))
        .map(|f| (*f).clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_a_video_format() {
        let v = json!({"streamingData": {"expiresInSeconds": "21540", "adaptiveFormats": [
            {"itag": 137, "url": "u137", "mimeType": "video/mp4; codecs=\"avc1.640028\"", "height": 1080, "fps": 30},
            {"itag": 298, "url": "u298", "mimeType": "video/mp4; codecs=\"avc1.4d4020\"", "height": 720, "fps": 60},
            {"itag": 136, "url": "u136", "mimeType": "video/mp4; codecs=\"avc1.4d401f\"", "height": 720, "fps": 30},
            {"itag": 135, "url": "u135", "mimeType": "video/mp4; codecs=\"avc1.4d401e\"", "height": 480, "fps": 30},
            {"itag": 134, "url": "u134", "mimeType": "video/mp4; codecs=\"avc1.4d401e\"", "height": 360, "fps": 30},
            {"itag": 244, "url": "u244", "mimeType": "video/webm; codecs=\"vp9\"", "height": 480, "fps": 30},
            {"itag": 251, "url": "u251", "mimeType": "audio/webm; codecs=\"opus\""}
        ]}});
        let f = parse_video("x", &v);
        assert_eq!(f.iter().map(|f| f.itag).collect::<Vec<_>>(), [298, 136, 135, 134]);
        assert_eq!(pick_video(&f, 300).unwrap().itag, 134);
        assert_eq!(pick_video(&f, 450).unwrap().itag, 135);
        assert_eq!(pick_video(&f, 600).unwrap().itag, 136);
        assert_eq!(pick_video(&f, 2000).unwrap().itag, 136);
        assert!(pick_video(&[], 300).is_none());
    }
}

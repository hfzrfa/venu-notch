//! Lightweight, on-demand lyrics lookup for the active media session.
//!
//! Only a track change performs network work. Results are kept in memory by
//! the media snapshot and are discarded when another track starts.

use serde::Deserialize;
use windows::core::HSTRING;
use windows::Foundation::Uri;
use windows::Web::Http::HttpClient;

#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub at_secs: f64,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Lyrics {
    pub synced: Vec<Line>,
    pub plain: Vec<String>,
}

impl Lyrics {
    pub fn is_empty(&self) -> bool {
        self.synced.is_empty() && self.plain.is_empty()
    }

    /// Index of the line that belongs to the current playback position.
    pub fn current_index(&self, position_secs: f64) -> Option<usize> {
        if self.synced.is_empty() {
            return (!self.plain.is_empty()).then_some(0);
        }
        self.synced
            .partition_point(|line| line.at_secs <= position_secs + 0.08)
            .checked_sub(1)
            .or(Some(0))
    }

    /// Synced lyrics use their timestamps. Plain lyrics have no timing data,
    /// so advance them evenly through the useful part of a finite track. This
    /// is intentionally approximate, but is much more useful than leaving the
    /// first line frozen for the entire song.
    pub fn current_index_with_duration(
        &self,
        position_secs: f64,
        duration_secs: f64,
    ) -> Option<usize> {
        if !self.synced.is_empty() {
            return self.current_index(position_secs);
        }
        if self.plain.is_empty() {
            return None;
        }
        if duration_secs <= 1.0 || !duration_secs.is_finite() {
            return Some(0);
        }
        // Leave a small instrumental lead-in and tail instead of burning the
        // first/last line against the exact media-session endpoints.
        let vocal_start = (duration_secs * 0.06).min(12.0);
        let vocal_end = (duration_secs * 0.94).max(vocal_start + 1.0);
        let progress = ((position_secs - vocal_start) / (vocal_end - vocal_start)).clamp(0.0, 1.0);
        Some(((progress * self.plain.len() as f64) as usize).min(self.plain.len() - 1))
    }

    pub fn line(&self, index: usize) -> Option<&str> {
        if self.synced.is_empty() {
            self.plain.get(index).map(String::as_str)
        } else {
            self.synced.get(index).map(|line| line.text.as_str())
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Response {
    track_name: Option<String>,
    artist_name: Option<String>,
    duration: Option<f64>,
    plain_lyrics: Option<String>,
    synced_lyrics: Option<String>,
}

#[derive(Deserialize)]
struct PlainResponse {
    lyrics: String,
}

/// Fetch a best match from LRCLIB. The endpoint accepts metadata from any
/// Windows media session, so this is not tied to Spotify or a browser.
pub fn fetch(
    title: &str,
    artist: &str,
    album: &str,
    duration_secs: f64,
) -> Result<Option<Lyrics>, String> {
    if title.trim().is_empty() || artist.trim().is_empty() {
        return Ok(None);
    }
    let client = HttpClient::new().map_err(|e| e.message())?;
    if let Ok(headers) = client.DefaultRequestHeaders() {
        if let Ok(user_agent) = headers.UserAgent() {
            let _ =
                user_agent.ParseAdd(&HSTRING::from(concat!("Nova/", env!("CARGO_PKG_VERSION"))));
        }
    }

    let lrclib = fetch_lrclib(&client, title, artist, album, duration_secs);
    match lrclib {
        Ok(Some(lyrics)) => return Ok(Some(lyrics)),
        Ok(None) => {}
        Err(_) => {
            // LRCLIB occasionally answers 429/5xx. A plain-lyrics provider is
            // better than making the whole Now Playing face lose its lyric.
        }
    }

    match fetch_plain_fallback(&client, title, artist) {
        Ok(Some(lyrics)) => Ok(Some(lyrics)),
        Ok(None) => lrclib,
        Err(fallback_error) => match lrclib {
            Err(primary_error) => Err(format!(
                "LRCLIB: {primary_error}; lyrics fallback: {fallback_error}"
            )),
            other => other,
        },
    }
}

fn fetch_lrclib(
    client: &HttpClient,
    title: &str,
    artist: &str,
    album: &str,
    duration_secs: f64,
) -> Result<Option<Lyrics>, String> {
    // Search first. Unlike `/api/get`, this endpoint returns an ordinary 200
    // with an empty list for a miss; WinRT's `GetStringAsync` does not expose
    // HTTP 404 consistently in its error text, which previously skipped the
    // useful fuzzy search for many browser/YouTube tracks.
    let search_url = format!(
        "https://lrclib.net/api/search?track_name={}&artist_name={}",
        encode(title.trim()),
        encode(artist.trim())
    );
    if let Some(body) = get_once(client, &search_url)? {
        let results: Vec<Response> = serde_json::from_str(&body).map_err(|e| e.to_string())?;
        if let Some(lyrics) = results
            .into_iter()
            .filter_map(|candidate| {
                let score = candidate.match_score(title, artist, duration_secs)?;
                let lyrics = candidate.into_lyrics()?;
                Some((score, lyrics))
            })
            .min_by(|left, right| left.0.total_cmp(&right.0))
            .map(|(_, lyrics)| lyrics)
        {
            return Ok(Some(lyrics));
        }
    }

    let relaxed_url = format!(
        "https://lrclib.net/api/get?track_name={}&artist_name={}",
        encode(title.trim()),
        encode(artist.trim())
    );
    let mut exact_url = relaxed_url.clone();
    if !album.trim().is_empty() {
        exact_url.push_str("&album_name=");
        exact_url.push_str(&encode(album.trim()));
    }
    if (1.0..=3600.0).contains(&duration_secs) {
        exact_url.push_str("&duration=");
        exact_url.push_str(&(duration_secs.round() as u32).to_string());
    }

    // Start strict, then relax album/duration. Media sessions often publish
    // those fields one poll later or round duration differently from LRCLIB.
    // One exact request is enough. Falling through directly to fuzzy search
    // avoids issuing a second near-identical `/get` request for every miss,
    // which was a major source of LRCLIB rate limiting during rapid Spotify
    // skips.
    if let Some(body) = get_once(client, &exact_url)? {
        let response: Response = serde_json::from_str(&body).map_err(|e| e.to_string())?;
        // Even /api/get can return a neighbouring catalogue entry when
        // metadata is incomplete. Never put it on screen unless the response
        // itself agrees with the requested song and recording.
        if response.match_score(title, artist, duration_secs).is_some() {
            if let Some(lyrics) = response.into_lyrics() {
                return Ok(Some(lyrics));
            }
        }
    }

    Ok(None)
}

fn fetch_plain_fallback(
    client: &HttpClient,
    title: &str,
    artist: &str,
) -> Result<Option<Lyrics>, String> {
    let url = format!(
        "https://api.lyrics.ovh/v1/{}/{}",
        encode(artist.trim()),
        encode(title.trim())
    );
    let Some(body) = get_once(client, &url)? else {
        return Ok(None);
    };
    let response: PlainResponse = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let plain = plain_lines(&response.lyrics);
    Ok((!plain.is_empty()).then_some(Lyrics {
        synced: Vec::new(),
        plain,
    }))
}

fn plain_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        // Provider transcripts often begin with "[Verse]" or an artist
        // credit. Do not let that become the only line shown in the pill.
        .filter(|line| !(line.starts_with('[') && line.ends_with(']')))
        .take(500)
        .map(str::to_owned)
        .collect()
}

impl Response {
    /// Lower is better. Search results must agree with the normalized title
    /// and artist; duration then selects the correct recording among them.
    fn match_score(&self, title: &str, artist: &str, duration: f64) -> Option<f64> {
        let wanted_title = normalized(title);
        let wanted_artist = normalized(artist);
        let found_title = normalized(self.track_name.as_deref()?);
        let found_artist = normalized(self.artist_name.as_deref()?);
        if !text_matches(&wanted_title, &found_title)
            || !text_matches(&wanted_artist, &found_artist)
        {
            return None;
        }

        let title_penalty = (wanted_title != found_title) as u8 as f64 * 8.0;
        let artist_penalty = (wanted_artist != found_artist) as u8 as f64 * 5.0;
        let duration_penalty = match (duration > 1.0, self.duration) {
            (true, Some(found)) => {
                let difference = (found - duration).abs();
                if difference > duration_tolerance(duration) {
                    return None;
                }
                difference
            }
            (true, None) => 12.0,
            _ => 0.0,
        };
        let unsynced_penalty = self.synced_lyrics.is_none() as u8 as f64 * 3.0;
        Some(title_penalty + artist_penalty + duration_penalty + unsynced_penalty)
    }

    fn into_lyrics(self) -> Option<Lyrics> {
        let synced = self
            .synced_lyrics
            .as_deref()
            .map(parse_lrc)
            .unwrap_or_default();
        let plain = self
            .plain_lyrics
            .as_deref()
            .map(plain_lines)
            .unwrap_or_default();
        let lyrics = Lyrics { synced, plain };
        (!lyrics.is_empty()).then_some(lyrics)
    }
}

fn duration_tolerance(duration: f64) -> f64 {
    // Enough for rounded media-session values, but not enough to silently
    // select a substantially different edit of the song.
    (duration * 0.025).clamp(4.0, 10.0)
}

fn normalized(value: &str) -> String {
    value
        .chars()
        .flat_map(char::to_lowercase)
        .map(|ch| if ch.is_alphanumeric() { ch } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn text_matches(left: &str, right: &str) -> bool {
    !left.is_empty()
        && !right.is_empty()
        && (left == right || left.contains(right) || right.contains(left))
}

fn get_once(client: &HttpClient, url: &str) -> Result<Option<String>, String> {
    let uri = Uri::CreateUri(&HSTRING::from(url)).map_err(|e| e.message())?;
    match client.GetStringAsync(&uri).and_then(|op| op.get()) {
        Ok(body) => Ok(Some(body.to_string_lossy())),
        Err(error) => {
            let message = error.message();
            if message.contains("404") || message.contains("Not Found") {
                Ok(None)
            } else {
                Err(message)
            }
        }
    }
}

fn parse_lrc(text: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    for raw in text.lines().take(1000) {
        let mut rest = raw.trim();
        let mut stamps = Vec::new();
        while let Some(after_open) = rest.strip_prefix('[') {
            let Some(end) = after_open.find(']') else {
                break;
            };
            let tag = &after_open[..end];
            if let Some(at_secs) = parse_stamp(tag) {
                stamps.push(at_secs);
                rest = &after_open[end + 1..];
            } else {
                break;
            }
        }
        let lyric = rest.trim();
        if lyric.is_empty() {
            continue;
        }
        for at_secs in stamps {
            lines.push(Line {
                at_secs,
                text: lyric.to_owned(),
            });
        }
    }
    lines.sort_by(|a, b| a.at_secs.total_cmp(&b.at_secs));
    lines.truncate(500);
    lines
}

fn parse_stamp(tag: &str) -> Option<f64> {
    let (minutes, seconds) = tag.split_once(':')?;
    let minutes: f64 = minutes.parse().ok()?;
    let seconds: f64 = seconds.parse().ok()?;
    (minutes >= 0.0 && (0.0..60.0).contains(&seconds)).then_some(minutes * 60.0 + seconds)
}

/// Percent-encode UTF-8 for query values without adding a dependency.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            use std::fmt::Write;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_selects_synced_lines() {
        let lyrics = Lyrics {
            synced: parse_lrc("[00:01.00]one\n[00:03.50][00:05.00]two"),
            plain: Vec::new(),
        };
        assert_eq!(lyrics.synced.len(), 3);
        assert_eq!(lyrics.current_index(4.0), Some(1));
        assert_eq!(lyrics.line(1), Some("two"));
    }

    #[test]
    fn plain_lines_advance_without_staying_frozen() {
        let lyrics = Lyrics {
            synced: Vec::new(),
            plain: vec!["one".into(), "two".into(), "three".into(), "four".into()],
        };
        assert_eq!(lyrics.current_index_with_duration(0.0, 100.0), Some(0));
        assert_eq!(lyrics.current_index_with_duration(51.0, 100.0), Some(2));
        assert_eq!(lyrics.current_index_with_duration(100.0, 100.0), Some(3));
    }

    #[test]
    fn search_matching_rejects_a_different_recording() {
        let matching = Response {
            track_name: Some("Best Friend".into()),
            artist_name: Some("Rex Orange County".into()),
            duration: Some(252.0),
            plain_lyrics: Some("line".into()),
            synced_lyrics: Some("[00:01.00]line".into()),
        };
        assert!(matching
            .match_score("Best Friend", "Rex Orange County", 250.0)
            .is_some());
        assert!(matching
            .match_score("Best Friend", "Rex Orange County", 220.0)
            .is_none());
    }

    #[test]
    fn encodes_unicode_query_values() {
        assert_eq!(encode("A & B"), "A%20%26%20B");
        assert_eq!(encode("é"), "%C3%A9");
    }

    #[test]
    fn parses_plain_provider_response() {
        let response: PlainResponse =
            serde_json::from_str(r#"{"lyrics":"[Verse]\nFirst line\n\nSecond line"}"#).unwrap();
        assert_eq!(plain_lines(&response.lyrics), ["First line", "Second line"]);
    }
}

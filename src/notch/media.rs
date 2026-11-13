//! Reads the system's "Now Playing" session — whichever app currently owns
//! Windows' System Media Transport Controls — on a dedicated background
//! thread, and publishes a snapshot the painter can read without blocking.
//!
//! The WinRT calls here block on `.get()` rather than awaiting. That is safe
//! only because this thread does nothing else: no message pump, no UI, so a
//! stalled wait starves nothing but itself.

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::notch::lyrics::Lyrics;
use parking_lot::RwLock;
use windows::core::Interface;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSessionManager as SessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus,
};
use windows::Storage::Streams::{DataReader, IInputStream, IRandomAccessStreamReference};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

const POLL_INTERVAL: Duration = Duration::from_millis(900);
const LYRICS_LOOKUP_TIMEOUT: Duration = Duration::from_secs(15);
static NEXT_LYRICS_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
/// A lyric can grow the resting pill up to this width before ellipsis becomes
/// unavoidable. The window reserves this reach without keeping it visible.
pub const MAX_STANDBY_WIDTH: f32 = 620.0;
/// Resting media layout. Sizing and painting both use these values so the
/// pill ends immediately after its content instead of retaining a guessed
/// right-hand allowance.
pub const STANDBY_LEFT_PAD: f32 = 10.0;
pub const STANDBY_RIGHT_PAD: f32 = 8.0;
pub const STANDBY_WAVE_WIDTH: f32 = 14.0;
pub const STANDBY_WAVE_GAP: f32 = 7.0;
// The empty-state minimum must still contain all fixed chrome (51 px).
pub const MIN_STANDBY_WIDTH: f32 = 51.0;
/// DirectWrite's reported advance can be a few pixels narrower than the
/// visible overhang of the final glyph. Keep enough room for that overhang so
/// a complete last word does not turn into an ellipsis.
pub const STANDBY_TEXT_SLACK: f32 = 12.0;
/// Album art is a thumbnail, never anything close to this; a stream this big
/// is a sign to bail rather than block on reading it.
const MAX_ART_BYTES: u64 = 8 * 1024 * 1024;

/// What the painter reads each frame. Cheap to clone — art bytes live behind
/// an `Arc` so an unchanged frame costs nothing to hand back.
#[derive(Clone, Default)]
pub struct NowPlaying {
    pub has_session: bool,
    pub playing: bool,
    pub title: String,
    pub artist: String,
    pub art: Option<Arc<[u8]>>,
    /// Bumped whenever `art` changes, so the painter's decode cache knows to
    /// re-decode without hashing or comparing the byte slice itself.
    pub art_generation: u64,
    /// Two colours sampled from the current artwork. Kept separate from the
    /// configured surface: the painter uses them only as an optional media
    /// overlay, so every glass theme continues to work underneath it.
    pub art_palette: Option<[[f32; 3]; 2]>,
    pub album: String,
    /// The app playing it, in words: "Spotify", "Chrome".
    pub source: String,
    /// Track length in seconds; 0 when the app does not report one (live
    /// streams, some browsers).
    pub duration_secs: f64,
    /// Position in seconds as of `position_at`. The painter runs it forward
    /// from there while playing, so it never needs a poll to tick.
    pub position_secs: f64,
    pub position_at: Option<Instant>,
    pub lyrics: Option<Arc<Lyrics>>,
    pub lyrics_loading: bool,
    /// Keeps a freshly changed track's title visible briefly before synced
    /// lyrics take over the collapsed face.
    track_started_at: Option<Instant>,
    /// Failed requests are retried a few times. These fields are internal
    /// lookup state and deliberately do not affect rendering by themselves.
    lyrics_failures: u8,
    lyrics_retry_at: Option<Instant>,
    /// Distinguishes "not requested yet" from a definitive provider miss.
    /// Spotify often publishes title/artist one poll before its duration;
    /// without this bit that refinement either lost the result or caused the
    /// same unavailable lyric to be requested forever.
    lyrics_queried: bool,
    lyrics_request_id: u64,
    lyrics_request_started: Option<Instant>,
}

impl NowPlaying {
    /// Where playback is now, in seconds, clamped to the track.
    pub fn position_now(&self) -> f64 {
        let mut pos = self.position_secs;
        if self.playing {
            if let Some(at) = self.position_at {
                pos += at.elapsed().as_secs_f64();
            }
        }
        if self.duration_secs > 0.0 {
            pos.clamp(0.0, self.duration_secs)
        } else {
            pos.max(0.0)
        }
    }

    pub fn current_lyric(&self) -> Option<&str> {
        let lyrics = self.lyrics.as_deref()?;
        lyrics.line(lyrics.current_index_with_duration(self.position_now(), self.duration_secs)?)
    }

    /// Exactly the text drawn on the resting media pill. Keeping this in one
    /// place makes sizing and painting agree, including the title/artist
    /// fallback while lyrics are unavailable.
    pub fn standby_text(&self, lyrics_enabled: bool) -> Cow<'_, str> {
        let title_grace = self
            .track_started_at
            .is_some_and(|started| started.elapsed() < Duration::from_secs(3));
        if lyrics_enabled && !title_grace {
            if let Some(line) = self.current_lyric().filter(|line| !line.trim().is_empty()) {
                return Cow::Borrowed(line.trim());
            }
        }
        // The closed pill is intentionally one glanceable line. The opened
        // player already shows artist, album and source; repeating the artist
        // here made browser/YouTube titles needlessly span most of the screen.
        Cow::Borrowed(self.title.trim())
    }
}

/// Width of the closed media pill for an already-measured line. Kept pure so
/// the compact/no-dead-space contract has a regression test independent of
/// DirectWrite and the renderer.
pub fn standby_pill_width(text_width: f32) -> f32 {
    let chrome = STANDBY_LEFT_PAD
        + STANDBY_WAVE_WIDTH
        + STANDBY_WAVE_GAP
        + STANDBY_RIGHT_PAD
        + STANDBY_TEXT_SLACK;
    (text_width.max(0.0) + chrome).clamp(MIN_STANDBY_WIDTH, MAX_STANDBY_WIDTH)
}

/// A media app's id as a name people know.
pub fn source_name(app_id: &str) -> String {
    let lower = app_id.to_ascii_lowercase();
    let known = [
        ("spotify", "Spotify"),
        ("chrome", "Chrome"),
        ("msedge", "Edge"),
        ("firefox", "Firefox"),
        // Firefox registers under a hash of its install path.
        ("308046b0af4a39cb", "Firefox"),
        ("brave", "Brave"),
        ("opera", "Opera"),
        ("vivaldi", "Vivaldi"),
        ("thebrowsercompany", "Arc"),
        ("zunemusic", "Media Player"),
        ("zunevideo", "Films & TV"),
        ("vlc", "VLC"),
        ("applemusic", "Apple Music"),
        ("itunes", "iTunes"),
        ("tidal", "TIDAL"),
        ("deezer", "Deezer"),
        ("youtube", "YouTube"),
    ];
    if let Some((_, name)) = known.iter().find(|(key, _)| lower.contains(key)) {
        return (*name).to_owned();
    }
    // "Publisher.App_hash!App" or "app.exe": keep the readable part.
    let tail = app_id.rsplit('!').next().unwrap_or(app_id);
    let tail = tail.rsplit(['\\', '/']).next().unwrap_or(tail);
    let tail = tail.strip_suffix(".exe").unwrap_or(tail);
    let tail = tail.rsplit('.').next().unwrap_or(tail);
    let mut chars = tail.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

enum Command {
    PlayPause,
    Next,
    Previous,
    Seek(u64),
}

/// Owns the background poller. Dropping this stops the thread — the notch
/// feature can be toggled off and on freely without leaking one.
pub struct MediaWatcher {
    snapshot: Arc<RwLock<NowPlaying>>,
    /// Bumped only when a poll actually found something different. The notch
    /// reads it every tick to decide whether the frame on screen is still
    /// current, so a poll that finds the same track playing must not look
    /// like a change.
    revision: Arc<AtomicU64>,
    tx: Sender<Command>,
    running: Arc<AtomicBool>,
    lyrics_enabled: Arc<AtomicBool>,
}

impl MediaWatcher {
    pub fn spawn() -> Self {
        let snapshot = Arc::new(RwLock::new(NowPlaying::default()));
        let revision = Arc::new(AtomicU64::new(0));
        let running = Arc::new(AtomicBool::new(true));
        let lyrics_enabled = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();

        let worker_snapshot = snapshot.clone();
        let worker_revision = revision.clone();
        let worker_running = running.clone();
        let worker_lyrics_enabled = lyrics_enabled.clone();
        std::thread::spawn(move || {
            run(
                worker_snapshot,
                worker_revision,
                worker_running,
                worker_lyrics_enabled,
                rx,
            )
        });

        Self {
            snapshot,
            revision,
            tx,
            running,
            lyrics_enabled,
        }
    }

    pub fn snapshot(&self) -> NowPlaying {
        self.snapshot.read().clone()
    }

    /// Whether any app owns the media session, without cloning the snapshot.
    pub fn has_session(&self) -> bool {
        self.snapshot.read().has_session
    }

    /// Whether something is playing, without cloning the snapshot: read on
    /// every frame for the music wave.
    pub fn is_playing(&self) -> bool {
        self.snapshot.read().playing
    }

    /// Monotonic counter over every change to what is playing. Equal
    /// revisions mean the notch would draw the same Now Playing it drew last
    /// time, without having to clone and compare the snapshot.
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    /// The playback position to the second, for the open player's frame
    /// key: it has to repaint once a second while the time counts up, and
    /// not otherwise.
    pub fn position_key(&self) -> u64 {
        let now = self.snapshot.read();
        if !now.has_session {
            return 0;
        }
        let title_grace = now
            .track_started_at
            .and_then(|started| Duration::from_secs(3).checked_sub(started.elapsed()))
            .map(|left| left.as_millis().div_ceil(250) as u64 + 1)
            .unwrap_or(0);
        let position = if now.duration_secs > 0.0 {
            now.position_now() as u64 + 1
        } else {
            0
        };
        position | (title_grace << 56)
    }

    /// Enable/disable online lyrics. Enabling after a track has already
    /// started triggers one lookup; disabling never affects media playback.
    pub fn set_lyrics_enabled(&self, enabled: bool) {
        let was = self.lyrics_enabled.swap(enabled, Ordering::AcqRel);
        if !enabled || was {
            return;
        }
        let request = {
            let mut now = self.snapshot.write();
            let query = lyrics_query(&now.title, &now.artist);
            if !now.has_session || now.lyrics.is_some() || now.lyrics_loading || query.is_none() {
                None
            } else {
                let (title, artist) = query.expect("lyrics query checked above");
                let request_id = NEXT_LYRICS_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
                now.lyrics_loading = true;
                now.lyrics_queried = true;
                now.lyrics_request_id = request_id;
                now.lyrics_request_started = Some(Instant::now());
                Some((
                    request_id,
                    track_key(&now.title, &now.artist, &now.album, now.duration_secs),
                    title,
                    artist,
                    now.album.clone(),
                    now.duration_secs,
                ))
            }
        };
        if let Some((request_id, key, title, artist, album, duration)) = request {
            self.revision.fetch_add(1, Ordering::Release);
            spawn_lyrics_lookup(
                self.snapshot.clone(),
                self.revision.clone(),
                request_id,
                key,
                title,
                artist,
                album,
                duration,
            );
        }
    }

    pub fn play_pause(&self) {
        let _ = self.tx.send(Command::PlayPause);
    }

    pub fn next(&self) {
        let _ = self.tx.send(Command::Next);
    }

    pub fn previous(&self) {
        let _ = self.tx.send(Command::Previous);
    }

    pub fn seek_to(&self, seconds: f64) {
        let duration = self.snapshot.read().duration_secs;
        if duration <= 0.0 {
            return;
        }
        let seconds = seconds.clamp(0.0, duration);
        {
            // Optimistic feedback keeps the thumb attached to the pointer;
            // the next media-session poll corrects it if an app rejects seek.
            let mut now = self.snapshot.write();
            now.position_secs = seconds;
            now.position_at = Some(Instant::now());
        }
        self.revision.fetch_add(1, Ordering::Release);
        let _ = self
            .tx
            .send(Command::Seek((seconds * 1000.0).round() as u64));
    }
}

impl Drop for MediaWatcher {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

fn run(
    snapshot: Arc<RwLock<NowPlaying>>,
    revision: Arc<AtomicU64>,
    running: Arc<AtomicBool>,
    lyrics_enabled: Arc<AtomicBool>,
    rx: Receiver<Command>,
) {
    // COM apartment for this thread only; the overlay's own STA thread is
    // initialized separately and neither knows about the other.
    if unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_err() {
        return;
    }

    let mut art_generation: u64 = 0;
    let mut last_track_key: Option<String> = None;
    let mut rapid_refresh_until: Option<Instant> = None;

    let mut next_poll = Instant::now();
    while running.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now >= next_poll {
            poll_once(
                &snapshot,
                &revision,
                &mut art_generation,
                &mut last_track_key,
                &lyrics_enabled,
            );
            let rapid = rapid_refresh_until.is_some_and(|until| Instant::now() < until);
            next_poll = Instant::now()
                + if rapid {
                    Duration::from_millis(140)
                } else {
                    POLL_INTERVAL
                };
        }
        let wait = next_poll.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(cmd) => {
                let changes_track = matches!(&cmd, Command::Next | Command::Previous);
                handle_command(cmd);
                if changes_track {
                    // Media apps publish their new metadata slightly after
                    // accepting skip. Poll briefly at a higher cadence so a
                    // skip made from the notch starts the new lyric lookup as
                    // reliably as a skip made inside Spotify/YouTube itself.
                    rapid_refresh_until = Some(Instant::now() + Duration::from_secs(3));
                    next_poll = Instant::now() + Duration::from_millis(100);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    unsafe { CoUninitialize() };
}

fn current_session() -> Option<windows::Media::Control::GlobalSystemMediaTransportControlsSession> {
    let manager = SessionManager::RequestAsync().ok()?.get().ok()?;
    manager.GetCurrentSession().ok()
}

fn handle_command(cmd: Command) {
    let Some(session) = current_session() else {
        return;
    };
    let _ = match cmd {
        Command::PlayPause => session.TryTogglePlayPauseAsync().and_then(|op| op.get()),
        Command::Next => session.TrySkipNextAsync().and_then(|op| op.get()),
        Command::Previous => session.TrySkipPreviousAsync().and_then(|op| op.get()),
        Command::Seek(milliseconds) => session
            .TryChangePlaybackPositionAsync((milliseconds.saturating_mul(10_000)) as i64)
            .and_then(|op| op.get()),
    };
}

/// Publish `next` and bump the revision, but only if it differs from what is
/// already there. Most polls find the same track in the same state, and the
/// notch treats a bumped revision as a reason to repaint.
fn publish(
    snapshot: &Arc<RwLock<NowPlaying>>,
    revision: &Arc<AtomicU64>,
    mut next: NowPlaying,
    preserve_lyrics: bool,
) {
    // Keep comparison and replacement under one lock. Otherwise a completed
    // lyric lookup can be erased by a stale media poll between its read/write.
    let mut current = snapshot.write();
    if preserve_lyrics {
        next.lyrics = current.lyrics.clone();
        next.lyrics_loading = current.lyrics_loading;
        next.lyrics_failures = current.lyrics_failures;
        next.lyrics_retry_at = current.lyrics_retry_at;
        next.lyrics_queried = current.lyrics_queried;
        next.lyrics_request_id = current.lyrics_request_id;
        next.lyrics_request_started = current.lyrics_request_started;
        next.track_started_at = current.track_started_at;
    }
    // `art` is compared through `art_generation`, which the poller bumps
    // whenever it reads new bytes — cheaper than comparing the bytes.
    let same = current.has_session == next.has_session
        && current.playing == next.playing
        && current.title == next.title
        && current.artist == next.artist
        && current.art_generation == next.art_generation
        && current.art_palette == next.art_palette
        && current.album == next.album
        && current.source == next.source
        && current.lyrics == next.lyrics
        && current.lyrics_loading == next.lyrics_loading
        && current.lyrics_queried == next.lyrics_queried
        && (current.duration_secs - next.duration_secs).abs() < 0.5;
    // Position moves on its own while playing; only a jump (a seek)
    // is news.
    let seeked = (current.position_now() - next.position_now()).abs() > 2.0;
    if same && !seeked {
        // Keep the anchor fresh without calling it a change.
        current.position_secs = next.position_secs;
        current.position_at = next.position_at;
        return;
    }

    *current = next;
    drop(current);
    revision.fetch_add(1, Ordering::Release);
}

fn poll_once(
    snapshot: &Arc<RwLock<NowPlaying>>,
    revision: &Arc<AtomicU64>,
    art_generation: &mut u64,
    last_track_key: &mut Option<String>,
    lyrics_enabled: &Arc<AtomicBool>,
) {
    let Some(session) = current_session() else {
        *last_track_key = None;
        publish(snapshot, revision, NowPlaying::default(), false);
        return;
    };

    let playing = session
        .GetPlaybackInfo()
        .and_then(|info| info.PlaybackStatus())
        .map(|s| s == PlaybackStatus::Playing)
        .unwrap_or(false);

    let props = session.TryGetMediaPropertiesAsync().and_then(|op| op.get());
    let (title, artist, album, thumb_ref) = match &props {
        Ok(p) => (
            p.Title().map(|h| h.to_string_lossy()).unwrap_or_default(),
            p.Artist().map(|h| h.to_string_lossy()).unwrap_or_default(),
            p.AlbumTitle()
                .map(|h| h.to_string_lossy())
                .unwrap_or_default(),
            p.Thumbnail().ok(),
        ),
        Err(_) => (String::new(), String::new(), String::new(), None),
    };
    let source = session
        .SourceAppUserModelId()
        .map(|id| source_name(&id.to_string_lossy()))
        .unwrap_or_default();

    // Timeline: where the app says it is, as of when it said so. Ticks are
    // 100 ns; the two times are in the same clock, so their difference is
    // how long ago the app last reported.
    let (duration_secs, position_secs) = session
        .GetTimelineProperties()
        .ok()
        .map(|tl| {
            let ticks = |span: windows::core::Result<windows::Foundation::TimeSpan>| {
                span.map(|s| s.Duration as f64 / 1e7).unwrap_or(0.0)
            };
            let start = ticks(tl.StartTime());
            let end = ticks(tl.EndTime());
            let mut position = ticks(tl.Position()) - start;
            if playing {
                if let Ok(updated) = tl.LastUpdatedTime() {
                    let now = now_universal_ticks();
                    if now > updated.UniversalTime {
                        let since = (now - updated.UniversalTime) as f64 / 1e7;
                        // A stale report is better ignored than trusted.
                        if since < 600.0 {
                            position += since;
                        }
                    }
                }
            }
            ((end - start).max(0.0), position.max(0.0))
        })
        .unwrap_or((0.0, 0.0));

    // The thumbnail read is the expensive part of a poll; only redo it when
    // the track has actually changed.
    // Album and duration routinely arrive one poll after Spotify's title and
    // artist. They are lookup hints, not track identity: including them here
    // made the same song look new and cleared a lyric that had just arrived.
    let key = track_key(&title, &artist, &album, duration_secs);
    let track_changed = Some(&key) != last_track_key.as_ref();
    *last_track_key = Some(key.clone());

    let art = if track_changed {
        let bytes = thumb_ref.and_then(|r| read_thumbnail(&r));
        if bytes.is_some() {
            *art_generation += 1;
        }
        bytes.map(Arc::from)
    } else {
        snapshot.read().art.clone()
    };
    let art_palette = if track_changed {
        art.as_deref().and_then(art_palette)
    } else {
        snapshot.read().art_palette
    };
    let lyric_query = lyrics_query(&title, &artist);
    // A WinRT HTTP operation can occasionally remain pending forever after a
    // network/app transition. Release the visible loading state and schedule
    // a fresh request; request ids keep a late completion from overwriting it.
    {
        let mut current = snapshot.write();
        if current.lyrics_loading
            && current
                .lyrics_request_started
                .is_some_and(|started| started.elapsed() >= LYRICS_LOOKUP_TIMEOUT)
        {
            current.lyrics_loading = false;
            current.lyrics_request_started = None;
            current.lyrics_failures = current.lyrics_failures.saturating_add(1);
            current.lyrics_retry_at = Some(Instant::now() + Duration::from_secs(2));
            revision.fetch_add(1, Ordering::Release);
        }
    }
    let current_lookup = snapshot.read();
    let retry_due = !track_changed
        && current_lookup.lyrics.is_none()
        && !current_lookup.lyrics_loading
        && current_lookup
            .lyrics_retry_at
            .is_some_and(|at| Instant::now() >= at);
    let never_queried = track_changed || !current_lookup.lyrics_queried;
    drop(current_lookup);
    let should_fetch_lyrics = lyrics_enabled.load(Ordering::Acquire)
        && lyric_query.is_some()
        && (never_queried || retry_due);
    let (
        lyrics,
        lyrics_loading,
        lyrics_failures,
        lyrics_retry_at,
        lyrics_queried,
        lyrics_request_id,
        lyrics_request_started,
    ) = if track_changed {
        (None, false, 0, None, false, 0, None)
    } else {
        let current = snapshot.read();
        (
            current.lyrics.clone(),
            current.lyrics_loading,
            current.lyrics_failures,
            current.lyrics_retry_at,
            current.lyrics_queried,
            current.lyrics_request_id,
            current.lyrics_request_started,
        )
    };
    let track_started_at = if track_changed {
        Some(Instant::now())
    } else {
        snapshot.read().track_started_at
    };

    publish(
        snapshot,
        revision,
        NowPlaying {
            has_session: true,
            playing,
            title: title.clone(),
            artist: artist.clone(),
            art,
            art_generation: *art_generation,
            art_palette,
            album: album.clone(),
            source,
            duration_secs,
            position_secs,
            position_at: Some(Instant::now()),
            lyrics,
            lyrics_loading,
            lyrics_failures,
            lyrics_retry_at,
            lyrics_queried,
            lyrics_request_id,
            lyrics_request_started,
            track_started_at,
        },
        !track_changed,
    );

    // Claim a request only after the latest metadata is published. The lookup
    // completion uses the same lock, preventing duplicate requests and stale
    // poll data from erasing a result.
    let start_lookup = if should_fetch_lyrics {
        let mut current = snapshot.write();
        let same_track = track_key(
            &current.title,
            &current.artist,
            &current.album,
            current.duration_secs,
        ) == key;
        if same_track && current.lyrics.is_none() && !current.lyrics_loading {
            let request_id = NEXT_LYRICS_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
            current.lyrics_loading = true;
            current.lyrics_queried = true;
            current.lyrics_request_id = request_id;
            current.lyrics_request_started = Some(Instant::now());
            Some(request_id)
        } else {
            None
        }
    } else {
        None
    };
    if let Some(request_id) = start_lookup {
        revision.fetch_add(1, Ordering::Release);
        let (lyric_title, lyric_artist) = lyric_query.expect("lyrics query checked above");
        spawn_lyrics_lookup(
            snapshot.clone(),
            revision.clone(),
            request_id,
            key,
            lyric_title,
            lyric_artist,
            album,
            duration_secs,
        );
    }
}

fn track_key(title: &str, artist: &str, album: &str, duration_secs: f64) -> String {
    // Album/duration are intentionally ignored: Spotify refines those fields
    // after exposing the new title. Normalising case/punctuation also avoids
    // a cosmetic metadata update invalidating an in-flight lookup.
    let _ = (album, duration_secs);
    format!(
        "{}\u{0}{}",
        normalized_metadata(title),
        normalized_metadata(artist)
    )
}

/// Browser media sessions (especially YouTube) sometimes leave `artist`
/// empty even though the conventional "Artist - Song" title contains both.
fn lyrics_query(title: &str, artist: &str) -> Option<(String, String)> {
    let artist = clean_lookup_artist(artist);
    let mut raw_title = title.trim().to_owned();
    // Chromium exposes a YouTube video's channel as the media artist. Some
    // integrations also append that same channel to the document title.
    // Remove that duplicate before interpreting the conventional
    // "Artist - Song" prefix.
    if !artist.is_empty() {
        for separator in [" \u{2014} ", " \u{2013} ", " | "] {
            if let Some((head, tail)) = raw_title.rsplit_once(separator) {
                if normalized_metadata(tail) == normalized_metadata(&artist) {
                    raw_title = head.trim().to_owned();
                    break;
                }
            }
        }
    }
    let mut title = clean_lookup_title(&raw_title);
    if title.is_empty() {
        return None;
    }
    let embedded = split_embedded_artist(&title);
    if !artist.is_empty() && !is_generic_artist(&artist) && !looks_like_channel(&artist) {
        // Browsers sometimes duplicate the artist in both fields:
        // title="Artist - Song", artist="Artist". LRCLIB expects only the
        // track name, so remove that prefix when it is unambiguous.
        if let Some((prefix, song)) = title.split_once(" - ") {
            if normalized_metadata(prefix) == normalized_metadata(&artist)
                && !song.trim().is_empty()
            {
                title = song.trim().to_owned();
            }
        }
        return Some((title, artist));
    }
    let (embedded_artist, embedded_title) = embedded?;
    let song = clean_lookup_title(embedded_title);
    (!embedded_artist.trim().is_empty() && !song.is_empty())
        .then(|| (song, clean_lookup_artist(embedded_artist)))
}

fn split_embedded_artist(title: &str) -> Option<(&str, &str)> {
    [" - ", " \u{2013} ", " \u{2014} "]
        .into_iter()
        .find_map(|separator| title.split_once(separator))
}

fn normalized_metadata(value: &str) -> String {
    value
        .chars()
        .flat_map(char::to_lowercase)
        .map(|ch| if ch.is_alphanumeric() { ch } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn clean_lookup_title(value: &str) -> String {
    let mut text = value
        .trim()
        .trim_start_matches(|ch: char| !ch.is_alphanumeric())
        .to_owned();
    for (open, close) in [('(', ')'), ('[', ']')] {
        loop {
            let Some(start) = text.rfind(open) else { break };
            let Some(relative_end) = text[start + 1..].find(close) else {
                break;
            };
            let end = start + 1 + relative_end;
            let tag = text[start + 1..end].to_ascii_lowercase();
            let decoration = [
                "official",
                "music video",
                "lyric",
                "audio",
                "visualizer",
                "remaster",
                "4k",
                "hd",
            ]
            .iter()
            .any(|word| tag.contains(word));
            if !decoration {
                break;
            }
            text.replace_range(start..=end, "");
            text = text.trim().to_owned();
        }
    }
    // YouTube frequently leaves these unbracketed after the actual song.
    // Catalogue providers store the song title without the video descriptor.
    let lower = text.to_ascii_lowercase();
    let cut = [
        " official music video",
        " official video",
        " official mv",
        " official audio",
        " lyric video",
        " lyrics video",
        " visualizer",
    ]
    .into_iter()
    .filter_map(|marker| lower.find(marker))
    .min();
    if let Some(cut) = cut {
        text.truncate(cut);
    }
    text.trim()
        .trim_matches(['\'', '"', '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}'])
        .trim()
        .to_owned()
}

fn clean_lookup_artist(value: &str) -> String {
    let mut artist = value.trim().to_owned();
    for suffix in [" - Topic", " – Topic", " — Topic"] {
        if artist
            .to_ascii_lowercase()
            .ends_with(&suffix.to_ascii_lowercase())
        {
            artist.truncate(artist.len() - suffix.len());
            break;
        }
    }
    artist.trim().to_owned()
}

fn is_generic_artist(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "youtube" | "youtube music" | "spotify" | "music" | "unknown artist"
    )
}

fn looks_like_channel(value: &str) -> bool {
    let compact = normalized_metadata(value).replace(' ', "");
    compact.ends_with("official")
        || compact.ends_with("records")
        || compact.ends_with("entertainment")
        || compact.ends_with("musicchannel")
}

fn lyrics_retry_delay(failures: u8) -> Option<Duration> {
    match failures {
        1 => Some(Duration::from_secs(8)),
        2 => Some(Duration::from_secs(30)),
        3 => Some(Duration::from_secs(120)),
        _ => None,
    }
}

fn spawn_lyrics_lookup(
    snapshot: Arc<RwLock<NowPlaying>>,
    revision: Arc<AtomicU64>,
    request_id: u64,
    key: String,
    title: String,
    artist: String,
    album: String,
    duration_secs: f64,
) {
    let _ = std::thread::Builder::new()
        .name("Nova lyrics".to_owned())
        .spawn(move || {
            let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
            let lookup = crate::notch::lyrics::fetch(&title, &artist, &album, duration_secs);
            let transient_failure = lookup.is_err();
            let result = lookup.ok().flatten().map(Arc::new);
            let mut now = snapshot.write();
            if track_key(&now.title, &now.artist, &now.album, now.duration_secs) == key
                && now.lyrics_request_id == request_id
            {
                now.lyrics = result;
                now.lyrics_loading = false;
                now.lyrics_request_started = None;
                if now.lyrics.is_some() {
                    now.lyrics_failures = 0;
                    now.lyrics_retry_at = None;
                } else if transient_failure {
                    now.lyrics_failures = now.lyrics_failures.saturating_add(1);
                    now.lyrics_retry_at =
                        lyrics_retry_delay(now.lyrics_failures).map(|delay| Instant::now() + delay);
                } else {
                    // Both providers answered and neither has this song. Do
                    // not hammer them every few seconds for a definitive miss.
                    now.lyrics_retry_at = None;
                }
                revision.fetch_add(1, Ordering::Release);
            }
            drop(now);
            if com {
                unsafe { CoUninitialize() };
            }
        });
}

/// Two low-cost representative colours from a 32px thumbnail. This runs only
/// when artwork changes and retains no decoded image memory.
fn art_palette(bytes: &[u8]) -> Option<[[f32; 3]; 2]> {
    let image = image::load_from_memory(bytes)
        .ok()?
        .thumbnail(32, 32)
        .to_rgb8();
    let mut dark = [0.0f64; 3];
    let mut light = [0.0f64; 3];
    let (mut dark_n, mut light_n) = (0u32, 0u32);
    for pixel in image.pixels() {
        let rgb = [
            pixel[0] as f64 / 255.0,
            pixel[1] as f64 / 255.0,
            pixel[2] as f64 / 255.0,
        ];
        let luma = rgb[0] * 0.2126 + rgb[1] * 0.7152 + rgb[2] * 0.0722;
        let (sum, n) = if luma < 0.45 {
            (&mut dark, &mut dark_n)
        } else {
            (&mut light, &mut light_n)
        };
        for i in 0..3 {
            sum[i] += rgb[i];
        }
        *n += 1;
    }
    if dark_n == 0 && light_n == 0 {
        return None;
    }
    let average = |sum: [f64; 3], n: u32| -> [f32; 3] {
        if n == 0 {
            return [0.12, 0.12, 0.14];
        }
        [
            (sum[0] / n as f64) as f32,
            (sum[1] / n as f64) as f32,
            (sum[2] / n as f64) as f32,
        ]
    };
    let a = average(dark, dark_n);
    let b = if light_n == 0 {
        a
    } else {
        average(light, light_n)
    };
    Some([a, b])
}

/// Now, in the 100 ns ticks since 1601 that WinRT's `DateTime` uses.
fn now_universal_ticks() -> i64 {
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (unix.as_nanos() / 100) as i64 + 116_444_736_000_000_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_apps_get_names_people_know() {
        assert_eq!(source_name("Spotify.exe"), "Spotify");
        assert_eq!(source_name("Chrome"), "Chrome");
        assert_eq!(source_name("MSEdge"), "Edge");
        assert_eq!(source_name("308046B0AF4A39CB"), "Firefox");
        assert_eq!(
            source_name("Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic"),
            "Media Player"
        );
        assert_eq!(source_name("foobar2000.exe"), "Foobar2000");
    }

    #[test]
    fn position_runs_forward_only_while_playing() {
        let mut now = NowPlaying {
            duration_secs: 100.0,
            position_secs: 10.0,
            position_at: Some(Instant::now() - Duration::from_secs(5)),
            ..Default::default()
        };
        assert!((now.position_now() - 10.0).abs() < 0.01);
        now.playing = true;
        assert!((now.position_now() - 15.0).abs() < 0.2);
        now.position_secs = 99.0;
        assert_eq!(now.position_now(), 100.0);
    }

    #[test]
    fn standby_fallback_shows_only_the_title() {
        let now = NowPlaying {
            title: "A Very Long Song Title".into(),
            artist: "A Very Long Artist Name".into(),
            ..Default::default()
        };
        assert_eq!(now.standby_text(true), "A Very Long Song Title");
    }

    #[test]
    fn a_new_track_shows_its_title_before_lyrics_take_over() {
        let lyrics = Arc::new(Lyrics {
            synced: vec![crate::notch::lyrics::Line {
                at_secs: 0.0,
                text: "first lyric".to_owned(),
            }],
            plain: Vec::new(),
        });
        let mut now = NowPlaying {
            title: "New title".to_owned(),
            lyrics: Some(lyrics),
            track_started_at: Some(Instant::now()),
            ..Default::default()
        };
        assert_eq!(now.standby_text(true), "New title");
        now.track_started_at = Some(Instant::now() - Duration::from_secs(4));
        assert_eq!(now.standby_text(true), "first lyric");
    }

    #[test]
    fn lyrics_query_uses_browser_title_when_artist_is_missing() {
        assert_eq!(
            lyrics_query("Rex Orange County - Best Friend", ""),
            Some(("Best Friend".to_owned(), "Rex Orange County".to_owned()))
        );
        assert_eq!(lyrics_query("Best Friend", ""), None);
    }

    #[test]
    fn lyrics_query_cleans_youtube_decorations_and_generic_artist() {
        assert_eq!(
            lyrics_query(
                "Rex Orange County - Best Friend (Official Video)",
                "YouTube"
            ),
            Some(("Best Friend".to_owned(), "Rex Orange County".to_owned()))
        );
        assert_eq!(
            lyrics_query("Best Friend [Official Lyrics]", "Rex Orange County - Topic"),
            Some(("Best Friend".to_owned(), "Rex Orange County".to_owned()))
        );
        assert_eq!(
            lyrics_query("Rex Orange County - Best Friend", "Rex Orange County"),
            Some(("Best Friend".to_owned(), "Rex Orange County".to_owned()))
        );
        assert_eq!(
            lyrics_query(
                "\u{1f7e0} \u{c774}\u{d558}\u{c774} (LeeHi) - 'ONLY' Official MV (ENG/CHN) \u{2014} AOMGOFFICIAL",
                "AOMGOFFICIAL"
            ),
            Some((
                "ONLY".to_owned(),
                "\u{c774}\u{d558}\u{c774} (LeeHi)".to_owned()
            ))
        );
    }

    #[test]
    fn lyric_identity_survives_delayed_spotify_metadata() {
        assert_eq!(
            track_key("Song", "Artist", "Studio", 180.0),
            track_key("Song", "Artist", "Live", 213.0)
        );
        assert_eq!(
            track_key("SONG!", "Artist", "", 0.0),
            track_key("Song", "artist", "Album", 180.0)
        );
    }

    #[test]
    fn standby_width_has_no_fixed_empty_tail_and_caps_long_text() {
        assert_eq!(standby_pill_width(100.0), 151.0);
        assert_eq!(standby_pill_width(0.0), MIN_STANDBY_WIDTH);
        assert_eq!(standby_pill_width(2_000.0), MAX_STANDBY_WIDTH);
    }

    #[test]
    fn ordinary_poll_cannot_overwrite_completed_lyrics() {
        let lyrics = Arc::new(Lyrics {
            synced: vec![crate::notch::lyrics::Line {
                at_secs: 0.0,
                text: "kept".to_owned(),
            }],
            plain: Vec::new(),
        });
        let snapshot = Arc::new(RwLock::new(NowPlaying {
            has_session: true,
            title: "Song".to_owned(),
            artist: "Artist".to_owned(),
            lyrics: Some(lyrics.clone()),
            ..Default::default()
        }));
        let revision = Arc::new(AtomicU64::new(0));
        publish(
            &snapshot,
            &revision,
            NowPlaying {
                has_session: true,
                title: "Song".to_owned(),
                artist: "Artist".to_owned(),
                position_secs: 1.0,
                ..Default::default()
            },
            true,
        );
        assert_eq!(snapshot.read().lyrics.as_deref(), Some(lyrics.as_ref()));
    }
}

fn read_thumbnail(reference: &IRandomAccessStreamReference) -> Option<Vec<u8>> {
    let stream = reference.OpenReadAsync().ok()?.get().ok()?;
    let size = stream.Size().ok()?;
    if size == 0 || size > MAX_ART_BYTES {
        return None;
    }

    let input: IInputStream = stream.cast().ok()?;
    let reader = DataReader::CreateDataReader(&input).ok()?;
    reader.LoadAsync(size as u32).ok()?.get().ok()?;

    let mut buf = vec![0u8; size as usize];
    reader.ReadBytes(&mut buf).ok()?;
    Some(buf)
}

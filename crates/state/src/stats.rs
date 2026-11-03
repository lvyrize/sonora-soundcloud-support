//! Listening stats in the manner of stats.fm, worked out from what played in Sonora itself.
//!
//! [`Stats`] follows [`Playback`] and keeps a listen for every track heard for at least
//! [`COUNTED`], with the time that was actually heard, so a skip or a pause does not count
//! towards anything. Listens go to the `listens` table, which is never trimmed, and the play
//! history already on the device seeds it once. A [`Report`] is then worked out for whichever
//! [`Span`] is asked for: totals, the top tracks, artists and albums, the hours of the day the
//! listening falls in, and how it spread over the span.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use gpui::{App, Context, Entity, Task};
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan as _, Zoned};
use music::{Album, ArtistRef, ReleaseType, SavedArtist, Track};
use rusqlite::{Connection, params};
use storage::Database;

use crate::playback::PlaybackEvent;
use crate::{Io, Playback, PlaybackState, Session, join};

/// How long a track has to be heard before it counts as a stream, as on stats.fm.
pub const COUNTED: Duration = Duration::from_secs(30);
/// How many tracks, artists and albums a report ranks.
pub const TOP: usize = 50;
/// How much more listening has to build up before a saved listen is written again, so a crash
/// loses no more than this.
const REWRITE: Duration = Duration::from_secs(30);
/// The longest gap between two playback updates that still counts as listening. A longer one is
/// a suspended machine, not music.
const GAP: Duration = Duration::from_secs(5);
/// The flag that says the play history has been copied into the listens once.
const SEEDED: &str = "listens_seeded";
/// How many top artists get their own picture looked up.
const PORTRAITS: usize = 24;

/// A stretch of time a report covers. The calendar ones follow the local time zone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Span {
    Today,
    Week,
    FourWeeks,
    Month,
    SixMonths,
    ThisYear,
    Year(i16),
    Lifetime,
}

/// How long one bar of the activity chart is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Hour,
    Day,
    Week,
    Month,
}

/// One bar of the activity chart: when it starts, in seconds since the epoch, and how much was
/// heard in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bucket {
    pub start: i64,
    pub listened: Duration,
}

/// Something ranked by how often it was streamed.
#[derive(Clone, Debug, PartialEq)]
pub struct Ranked<T> {
    pub item: T,
    pub streams: u64,
    pub listened: Duration,
}

/// What a span adds up to.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub listened: Duration,
    pub streams: u64,
    pub tracks: usize,
    pub artists: usize,
    pub albums: usize,
    /// The most streamed tracks, each carrying its stream count as `playcount` and the time it
    /// was heard as `duration`, so a track table shows both.
    pub top_tracks: Vec<Track>,
    pub top_artists: Vec<Ranked<SavedArtist>>,
    pub top_albums: Vec<Ranked<Album>>,
    /// How much was heard in each hour of the day, midnight first.
    pub clock: [Duration; 24],
    pub step: Option<Step>,
    pub activity: Vec<Bucket>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatsState {
    Loading,
    Ready,
    Failed,
}

/// The track playing now and how much of it has been heard.
struct Hearing {
    track: Track,
    provider: String,
    started_at: i64,
    heard: Duration,
    /// When the last playback update arrived while playing, to measure the next stretch from.
    since: Option<Instant>,
    /// How much was heard when the listen was last written, or `None` before it first was.
    written: Option<Duration>,
}

pub struct Stats {
    session: Entity<Session>,
    playback: Entity<Playback>,
    database: Database,
    io: Io,
    span: Span,
    state: StatsState,
    report: Report,
    /// The years before this one that have listens, newest first.
    years: Vec<i16>,
    hearing: Option<Hearing>,
    task: Option<Task<()>>,
    portraits: Option<Task<()>>,
}

impl Stats {
    pub fn new(
        session: Entity<Session>,
        playback: Entity<Playback>,
        database: Database,
        io: Io,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&playback, |this, _, cx| this.tick(cx)).detach();
        cx.subscribe(&playback, |this, _, event, cx| {
            if matches!(event, PlaybackEvent::EndedPlayback) {
                this.finish(cx);
            }
        })
        .detach();
        let seeding = database.clone();
        io.spawn(async move {
            let seeded = tokio::task::spawn_blocking(move || seed(&seeding)).await;
            if let Ok(Err(error)) = seeded {
                log::warn!("stats: cannot copy the play history into the listens: {error:#}");
            }
        });
        Self {
            session,
            playback,
            database,
            io,
            span: Span::FourWeeks,
            state: StatsState::Loading,
            report: Report::default(),
            years: Vec::new(),
            hearing: None,
            task: None,
            portraits: None,
        }
    }

    pub fn span(&self) -> Span {
        self.span
    }

    pub fn state(&self) -> StatsState {
        self.state
    }

    pub fn report(&self) -> &Report {
        &self.report
    }

    pub fn years(&self) -> &[i16] {
        &self.years
    }

    pub fn set_span(&mut self, span: Span, cx: &mut Context<Self>) {
        if self.span == span {
            return;
        }
        self.span = span;
        self.refresh(cx);
    }

    /// Works the report for the current span out again from the listens.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        // what is playing now counts too, as far as it got
        self.write(cx);
        if self.report == Report::default() {
            self.state = StatsState::Loading;
            cx.notify();
        }
        let span = self.span;
        let database = self.database.clone();
        let io = self.io.clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            let worked = join(io.spawn(async move {
                tokio::task::spawn_blocking(move || work(&database, span)).await?
            }))
            .await;
            this.update(cx, |this, cx| {
                this.task = None;
                match worked {
                    Ok((report, years)) => {
                        this.report = report;
                        this.years = years;
                        this.state = StatsState::Ready;
                        this.portraits(cx);
                    }
                    Err(error) => {
                        log::warn!("stats: cannot work out the stats: {error:#}");
                        this.state = StatsState::Failed;
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Looks up real pictures for the top artists, which the listens only know a cover for.
    fn portraits(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.session.read(cx).client() else {
            return;
        };
        let ids: Vec<String> = self
            .report
            .top_artists
            .iter()
            .take(PORTRAITS)
            .map(|ranked| ranked.item.id.clone())
            .filter(|id| !music::is_local_id(id))
            .collect();
        if ids.is_empty() {
            return;
        }
        let io = self.io.clone();
        self.portraits = Some(cx.spawn(async move |this, cx| {
            let found = join(io.spawn(async move { client.artist_images(ids).await })).await;
            let Ok(found) = found else {
                return;
            };
            this.update(cx, |this, cx| {
                for ranked in &mut this.report.top_artists {
                    if let Some(image) = found.get(&ranked.item.id) {
                        ranked.item.cover = Some(image.clone());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Follows playback: measures what has been heard of the current track, starts a new listen
    /// when the track changes, and writes the listen once it counts.
    fn tick(&mut self, cx: &mut Context<Self>) {
        let playback = self.playback.read(cx);
        let playing = *playback.state() == PlaybackState::Playing;
        let current = playback.track().cloned();
        let same = matches!(
            (&self.hearing, &current),
            (Some(hearing), Some(track)) if hearing.track.id == track.id && track.id.is_some()
        );
        if !same {
            self.finish(cx);
            self.hearing = current.and_then(|track| self.begin(track, cx));
        }
        let Some(hearing) = &mut self.hearing else {
            return;
        };
        let now = Instant::now();
        match (playing, hearing.since) {
            (true, Some(since)) => {
                let stretch = now.duration_since(since);
                if stretch <= GAP {
                    hearing.heard += stretch;
                }
                hearing.since = Some(now);
            }
            (true, None) => hearing.since = Some(now),
            (false, _) => hearing.since = None,
        }
        let due = match hearing.written {
            None => hearing.heard >= COUNTED,
            Some(written) => hearing.heard >= written + REWRITE,
        };
        if due {
            self.write(cx);
        }
    }

    fn begin(&self, track: Track, cx: &App) -> Option<Hearing> {
        let id = track.id.clone()?;
        let provider = self.session.read(cx).slug_for(&id)?.to_owned();
        Some(Hearing {
            track,
            provider,
            started_at: now_ms(),
            heard: Duration::ZERO,
            since: None,
            written: None,
        })
    }

    /// Writes the current listen as far as it got, once it counts.
    fn write(&mut self, _cx: &mut Context<Self>) {
        let Some(hearing) = &mut self.hearing else {
            return;
        };
        if hearing.heard < COUNTED || hearing.written == Some(hearing.heard) {
            return;
        }
        hearing.written = Some(hearing.heard);
        let listen = Listen {
            provider: hearing.provider.clone(),
            track: hearing.track.clone(),
            started_at: hearing.started_at,
            heard: hearing.heard,
        };
        let database = self.database.clone();
        self.io.spawn(async move {
            let saved = tokio::task::spawn_blocking(move || save(&database, &listen)).await;
            if let Ok(Err(error)) = saved {
                log::warn!("stats: cannot save a listen: {error:#}");
            }
        });
    }

    /// Ends the current listen, writing what was heard of it.
    fn finish(&mut self, cx: &mut Context<Self>) {
        self.write(cx);
        self.hearing = None;
    }
}

/// One listen on its way to the table.
struct Listen {
    provider: String,
    track: Track,
    started_at: i64,
    heard: Duration,
}

/// One listen as read back for a report.
struct Row {
    track_id: String,
    started_at: i64,
    heard: Duration,
    track: Track,
}

fn open(database: &Database) -> Result<Connection> {
    database.open().context("cannot open the listens")
}

fn save(database: &Database, listen: &Listen) -> Result<()> {
    let Some(track_id) = listen.track.id.as_deref() else {
        return Ok(());
    };
    let track = &listen.track;
    let artist_refs =
        serde_json::to_string(&track.artist_refs).context("cannot encode a listen's artists")?;
    open(database)?
        .execute(
            "INSERT INTO listens (
                provider, track_id, started_at, listened_ms, duration_ms, name, artists,
                artist_refs, album, album_id, cover
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (track_id, started_at) DO UPDATE SET listened_ms = excluded.listened_ms",
            params![
                listen.provider,
                track_id,
                listen.started_at,
                millis(listen.heard),
                millis(track.duration),
                track.name,
                track.artists,
                artist_refs,
                track.album,
                track.album_id,
                track.cover,
            ],
        )
        .context("cannot save a listen")?;
    Ok(())
}

/// Copies the play history into the listens the first time, each play counted as heard in full,
/// since the history kept no more than when it started.
fn seed(database: &Database) -> Result<()> {
    let connection = open(database)?;
    let done: Option<i64> = connection
        .query_row("SELECT value FROM flags WHERE key = ?", [SEEDED], |row| {
            row.get(0)
        })
        .ok();
    if done.is_some() {
        return Ok(());
    }
    let copied = connection
        .execute(
            "INSERT OR IGNORE INTO listens (
                provider, track_id, started_at, listened_ms, duration_ms, name, artists,
                artist_refs, album, album_id, cover
             )
             SELECT provider, track_id, played_at, duration_ms, duration_ms, name, artists,
                    artist_refs, album, album_id, cover
             FROM plays",
            [],
        )
        .context("cannot copy the play history")?;
    connection
        .execute(
            "INSERT OR REPLACE INTO flags (key, value) VALUES (?, 1)",
            [SEEDED],
        )
        .context("cannot mark the play history copied")?;
    log::info!("stats: copied {copied} plays from the history into the listens");
    Ok(())
}

/// The listens that started in `[from, to)`, in milliseconds since the epoch.
fn rows(connection: &Connection, from: i64, to: i64) -> Result<Vec<Row>> {
    let mut query = connection
        .prepare(
            "SELECT track_id, started_at, listened_ms, duration_ms, name, artists, artist_refs,
                    album, album_id, cover
             FROM listens
             WHERE started_at >= ? AND started_at < ?
             ORDER BY started_at DESC",
        )
        .context("cannot prepare the listens read")?;
    let rows = query
        .query_map(params![from, to], |row| {
            let refs = row.get_ref(6)?.as_str()?;
            let artist_refs: Vec<ArtistRef> = serde_json::from_str(refs).unwrap_or_default();
            let track_id: String = row.get(0)?;
            let heard: i64 = row.get(2)?;
            let duration: i64 = row.get(3)?;
            Ok(Row {
                started_at: row.get(1)?,
                heard: Duration::from_millis(heard.max(0) as u64),
                track: Track {
                    id: Some(track_id.clone()),
                    name: row.get(4)?,
                    playable: true,
                    artists: row.get(5)?,
                    artist_refs,
                    album: row.get(7)?,
                    album_id: row.get(8)?,
                    cover: row.get(9)?,
                    duration: Duration::from_millis(duration.max(0) as u64),
                    added_at: None,
                    added_by: None,
                    playcount: None,
                    popularity: 0,
                    explicit: false,
                    track_number: 0,
                    disc_number: 0,
                    tags: Vec::new(),
                    languages: Vec::new(),
                    credits: Vec::new(),
                },
                track_id,
            })
        })
        .context("cannot read the listens")?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("cannot read a listen")
}

/// Works the report for `span` out, with the earlier years that have listens.
fn work(database: &Database, span: Span) -> Result<(Report, Vec<i16>)> {
    let connection = open(database)?;
    let zone = TimeZone::system();
    let now = Zoned::now().with_time_zone(zone.clone());
    let first: Option<i64> = connection
        .query_row("SELECT MIN(started_at) FROM listens", [], |row| row.get(0))
        .context("cannot read the first listen")?;
    let years = earlier_years(first, &now, &zone);
    let Some((from, to, step)) = bounds(span, first, &now, &zone)? else {
        return Ok((Report::default(), years));
    };
    let rows = rows(
        &connection,
        from.timestamp().as_millisecond(),
        to.timestamp().as_millisecond(),
    )?;
    let mut report = rank(&rows, &zone);
    report.step = Some(step);
    report.activity = activity(&rows, &from, &to, step)?;
    Ok((report, years))
}

/// The years before this one that have listens, newest first.
fn earlier_years(first: Option<i64>, now: &Zoned, zone: &TimeZone) -> Vec<i16> {
    let Some(first) = first.and_then(|first| Timestamp::from_millisecond(first).ok()) else {
        return Vec::new();
    };
    let since = first.to_zoned(zone.clone()).year();
    (since..now.year()).rev().collect()
}

/// Where `span` starts and ends and how long a bar of its activity chart is. `None` when there
/// is nothing to cover, as for a lifetime with no listens.
fn bounds(
    span: Span,
    first: Option<i64>,
    now: &Zoned,
    zone: &TimeZone,
) -> Result<Option<(Zoned, Zoned, Step)>> {
    let today = now.start_of_day()?;
    let tomorrow = today.checked_add(1.day())?;
    let (from, to) = match span {
        Span::Today => (today.clone(), tomorrow),
        Span::Week => (today.checked_sub(6.days())?, tomorrow),
        Span::FourWeeks => (today.checked_sub(27.days())?, tomorrow),
        Span::Month => (today.first_of_month()?, tomorrow),
        Span::SixMonths => (
            today.checked_sub(6.months())?.checked_add(1.day())?,
            tomorrow,
        ),
        Span::ThisYear => (today.first_of_year()?, tomorrow),
        Span::Year(year) => {
            let start = jiff::civil::date(year, 1, 1).to_zoned(zone.clone())?;
            let end = start.checked_add(1.year())?;
            (start, end)
        }
        Span::Lifetime => {
            let Some(first) = first.and_then(|first| Timestamp::from_millisecond(first).ok())
            else {
                return Ok(None);
            };
            (first.to_zoned(zone.clone()).start_of_day()?, tomorrow)
        }
    };
    let days = from.until(&to)?.total(jiff::Unit::Hour)? / 24.;
    let step = match days {
        days if days <= 1.5 => Step::Hour,
        days if days <= 35. => Step::Day,
        days if days <= 200. => Step::Week,
        _ => Step::Month,
    };
    Ok(Some((from, to, step)))
}

/// Adds the listens up into totals, rankings and the listening clock.
fn rank(rows: &[Row], zone: &TimeZone) -> Report {
    let mut report = Report::default();
    let mut tracks: HashMap<&str, (Track, u64, Duration)> = HashMap::new();
    let mut artists: HashMap<String, (SavedArtist, u64, Duration)> = HashMap::new();
    let mut albums: HashMap<String, (Album, u64, Duration)> = HashMap::new();
    // newest first, so the first row seen for anything is its latest look
    for row in rows {
        report.listened += row.heard;
        report.streams += 1;
        if let Ok(stamp) = Timestamp::from_millisecond(row.started_at) {
            let hour = stamp.to_zoned(zone.clone()).hour();
            report.clock[hour.clamp(0, 23) as usize] += row.heard;
        }
        let entry = tracks
            .entry(row.track_id.as_str())
            .or_insert_with(|| (row.track.clone(), 0, Duration::ZERO));
        entry.1 += 1;
        entry.2 += row.heard;
        for artist in &row.track.artist_refs {
            let Some(id) = artist.id.clone().filter(|id| !id.is_empty()) else {
                continue;
            };
            let entry = artists.entry(id.clone()).or_insert_with(|| {
                let artist = SavedArtist {
                    id,
                    name: artist.name.clone(),
                    cover: row.track.cover.clone(),
                    added_at: None,
                };
                (artist, 0, Duration::ZERO)
            });
            entry.1 += 1;
            entry.2 += row.heard;
        }
        if let Some(album_id) = row.track.album_id.clone().filter(|id| !id.is_empty())
            && !row.track.album.is_empty()
        {
            let entry = albums.entry(album_id.clone()).or_insert_with(|| {
                let album = Album {
                    id: album_id,
                    name: row.track.album.clone(),
                    artists: row.track.artists.clone(),
                    artist_refs: row.track.artist_refs.clone(),
                    cover: row.track.cover.clone(),
                    cover_large: row.track.cover.clone(),
                    release_type: ReleaseType::Album,
                    year: 0,
                    track_count: 0,
                    release_date: String::new(),
                    label: String::new(),
                    copyrights: Vec::new(),
                    added_at: None,
                };
                (album, 0, Duration::ZERO)
            });
            entry.1 += 1;
            entry.2 += row.heard;
        }
    }
    report.tracks = tracks.len();
    report.artists = artists.len();
    report.albums = albums.len();
    report.top_tracks = ordered(tracks.into_values())
        .into_iter()
        .map(|ranked| Track {
            playcount: Some(ranked.streams),
            duration: ranked.listened,
            ..ranked.item
        })
        .collect();
    report.top_artists = ordered(artists.into_values());
    report.top_albums = ordered(albums.into_values());
    report
}

/// The `TOP` most streamed, ties going to the longer heard.
fn ordered<T>(counted: impl Iterator<Item = (T, u64, Duration)>) -> Vec<Ranked<T>> {
    let mut ranked: Vec<Ranked<T>> = counted
        .map(|(item, streams, listened)| Ranked {
            item,
            streams,
            listened,
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.streams
            .cmp(&a.streams)
            .then_with(|| b.listened.cmp(&a.listened))
    });
    ranked.truncate(TOP);
    ranked
}

/// How the listening spread over `[from, to)`, one bucket per `step`, empty ones included.
fn activity(rows: &[Row], from: &Zoned, to: &Zoned, step: Step) -> Result<Vec<Bucket>> {
    let span = match step {
        Step::Hour => 1.hour(),
        Step::Day => 1.day(),
        Step::Week => 1.week(),
        Step::Month => 1.month(),
    };
    let mut start = match step {
        Step::Month => from.first_of_month()?,
        _ => from.clone(),
    };
    let mut buckets = Vec::new();
    while start < *to {
        buckets.push(Bucket {
            start: start.timestamp().as_second(),
            listened: Duration::ZERO,
        });
        start = start.checked_add(span)?;
    }
    for row in rows {
        let at = row.started_at.div_euclid(1_000);
        let index = buckets.partition_point(|bucket| bucket.start <= at);
        if let Some(bucket) = index
            .checked_sub(1)
            .and_then(|index| buckets.get_mut(index))
        {
            bucket.listened += row.heard;
        }
    }
    Ok(buckets)
}

fn millis(duration: Duration) -> i64 {
    duration.as_millis().min(i64::MAX as u128) as i64
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(millis)
        .unwrap_or_default()
}


//! Playback for SoundCloud: what [`crate::engine`] needs that is SoundCloud's own.
//!
//! A track lists the formats it is served in. Each is asked for the address of its audio, which
//! is either one progressive file or an HLS playlist of short segments. Laid end to end, behind
//! the init segment where there is one, the segments are one file, so both shapes become one
//! [`crate::stream`] download and rodio decodes that like any other file. Encrypted streams and
//! previews are passed over. The threads, the queue, the preload and the gapless join are the
//! engine's.

use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use bytes::Bytes;
use reqwest::Url;
use rodio::Source as _;

use crate::aac::Aac;
use crate::audio::Trimmed;
use crate::engine::{self, Fetch};
use crate::soundcloud::client::SoundCloudClient;
use crate::soundcloud::wire::{self, Location, Transcoding};
use crate::stream::{Plain, Reader, Source, Stream};
use crate::{Encoding, PlaybackConfig, PlaybackEvents, PlaybackFactory, Player};

/// How often one segment is asked for before the track is given up on.
const ATTEMPTS: u32 = 2;

/// A track downloading, how long SoundCloud says it is, and what it is encoded as.
#[derive(Clone)]
pub struct Loaded {
    stream: Stream,
    duration: Duration,
    kind: Kind,
}

/// The container a format arrives in, which tells the decoder what to expect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Mp3,
    Ogg,
    Mp4,
}

pub struct Factory {
    client: SoundCloudClient,
}

impl Factory {
    pub fn new(client: SoundCloudClient) -> Self {
        Self { client }
    }
}

impl PlaybackFactory for Factory {
    fn start(&self, config: PlaybackConfig) -> (Box<dyn Player>, Box<dyn PlaybackEvents>) {
        engine::start(
            SoundCloud {
                client: self.client.clone(),
            },
            config,
        )
    }
}

struct SoundCloud {
    client: SoundCloudClient,
}

/// Where the bytes of one track come from: one file, or an HLS playlist's segments in order.
enum Body {
    File(reqwest::Response),
    Segments {
        http: reqwest::Client,
        urls: std::vec::IntoIter<Url>,
        current: Option<reqwest::Response>,
    },
}

impl Source for Body {
    async fn chunk(&mut self) -> Result<Option<Bytes>> {
        match self {
            Self::File(response) => Source::chunk(response).await,
            Self::Segments {
                http,
                urls,
                current,
            } => loop {
                if let Some(response) = current {
                    match response
                        .chunk()
                        .await
                        .map_err(reqwest::Error::without_url)?
                    {
                        Some(bytes) => return Ok(Some(bytes)),
                        None => *current = None,
                    }
                }
                let Some(url) = urls.next() else {
                    return Ok(None);
                };
                *current = Some(segment(http, url).await?);
            },
        }
    }
}

impl Kind {
    fn of(mime: &str) -> Option<Self> {
        match mime.split(';').next()?.trim() {
            "audio/mpeg" => Some(Self::Mp3),
            "audio/ogg" => Some(Self::Ogg),
            "audio/mp4" | "audio/aac" => Some(Self::Mp4),
            _ => None,
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::Ogg => "ogg",
            Self::Mp4 => "mp4",
        }
    }
}

/// How much a format is preferred, lower first: a plain file over segments, and MP3, which
/// joins seamlessly, over the rest. `None` for a format that cannot be played here.
fn rank(transcoding: &Transcoding) -> Option<(u8, Kind)> {
    if transcoding.snipped || transcoding.url.is_empty() {
        return None;
    }
    let kind = Kind::of(&transcoding.format.mime_type)?;
    let shape = match transcoding.format.protocol.as_str() {
        "progressive" => 0,
        "hls" => 1,
        // the encrypted kinds of HLS need keys this does not have
        _ => return None,
    };
    let codec = match kind {
        Kind::Mp3 => 0,
        Kind::Ogg => 1,
        Kind::Mp4 => 2,
    };
    Some((shape * 3 + codec, kind))
}

impl SoundCloud {
    /// Opens the best format of a track SoundCloud will play here.
    async fn open_track(&self, track: &wire::Track) -> Result<(Body, Option<u64>, Kind)> {
        let mut choices: Vec<(u8, Kind, &Transcoding)> = track
            .media
            .iter()
            .flat_map(|media| &media.transcodings)
            .filter_map(|transcoding| {
                rank(transcoding).map(|(rank, kind)| (rank, kind, transcoding))
            })
            .collect();
        choices.sort_by_key(|(rank, ..)| *rank);
        let mut last = None;
        for (_, kind, transcoding) in choices {
            match self.open(track, transcoding).await {
                Ok((body, total)) => {
                    log::debug!(
                        "playback: soundcloud track {} plays as {}",
                        track.id,
                        transcoding.preset
                    );
                    return Ok((body, total, kind));
                }
                Err(error) => {
                    log::warn!(
                        "playback: soundcloud's {} of {} failed: {error:#}",
                        transcoding.preset,
                        track.id
                    );
                    last = Some(error);
                }
            }
        }
        Err(last.unwrap_or_else(|| {
            anyhow::anyhow!("soundcloud serves no full stream of {} here", track.id)
        }))
    }

    /// Asks a format for the address of its audio and starts fetching it.
    async fn open(
        &self,
        track: &wire::Track,
        transcoding: &Transcoding,
    ) -> Result<(Body, Option<u64>)> {
        let mut url = Url::parse(&transcoding.url).context("a transcoding url does not parse")?;
        if let Some(authorization) = &track.track_authorization {
            url.query_pairs_mut()
                .append_pair("track_authorization", authorization);
        }
        let location: Location = self.client.get_url(url).await?;
        let address = Url::parse(&location.url).context("a stream url does not parse")?;
        let http = self.client.http().clone();
        if transcoding.format.protocol == "progressive" {
            let response = http
                .get(address)
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
                .map_err(reqwest::Error::without_url)?;
            let total = response.content_length();
            return Ok((Body::File(response), total));
        }
        let response = http
            .get(address)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(reqwest::Error::without_url)?;
        let base = response.url().clone();
        let text = response
            .text()
            .await
            .context("cannot read the stream playlist")?;
        let urls = playlist(&base, &text);
        if urls.is_empty() {
            bail!("the stream playlist lists no segments");
        }
        Ok((
            Body::Segments {
                http,
                urls: urls.into_iter(),
                current: None,
            },
            None,
        ))
    }
}

#[async_trait]
impl Fetch for SoundCloud {
    type Loaded = Loaded;
    type Source = Trimmed<rodio::Decoder<Reader<Plain>>>;

    fn name(&self) -> &'static str {
        "soundcloud"
    }

    async fn load(&self, id: &str) -> Result<Loaded> {
        let track = self.client.record(id).await?;
        let (body, total, kind) = self.open_track(&track).await?;
        let stream = Stream::pulling(body, total, Plain).primed().await?;
        Ok(Loaded {
            stream,
            duration: Duration::from_millis(track.duration),
            kind,
        })
    }

    fn length(&self, loaded: &Loaded) -> Option<Duration> {
        Some(loaded.duration).filter(|duration| !duration.is_zero())
    }

    fn encoding(&self, loaded: &Loaded) -> Option<Encoding> {
        match loaded.kind {
            Kind::Mp3 => Some(Encoding::Mp3 { kbps: 128 }),
            Kind::Ogg => Some(Encoding::Opus { kbps: 64 }),
            Kind::Mp4 => Some(Encoding::Aac { kbps: 160 }),
        }
    }

    async fn downloaded(&self, loaded: &Loaded) {
        loaded.stream.finished().await;
    }

    fn progress(&self, loaded: &Loaded) -> Option<f32> {
        let total = loaded.stream.total().filter(|total| *total > 0)?;
        Some(loaded.stream.arrived() as f32 / total as f32)
    }

    /// Builds a decoder over a stream and places it at `at`. Only a whole file of known length
    /// can be seeked in; segments laid end to end, and fragmented MP4 above all, are opened as
    /// a stream and decoded through to `at` instead, as YouTube Music's DASH is.
    fn open(&self, id: &str, loaded: &Loaded, at: Duration) -> Option<Self::Source> {
        let total = loaded.stream.total();
        let seekable = total.is_some() && loaded.kind != Kind::Mp4;
        let mut builder = rodio::Decoder::builder()
            .with_symphonia_decoder::<Aac>()
            .with_data(loaded.stream.reader())
            .with_seekable(seekable)
            .with_hint(loaded.kind.hint());
        if let Some(total) = total.filter(|_| seekable) {
            builder = builder.with_byte_len(total);
        }
        let mut decoder = match builder.build() {
            Ok(decoder) => decoder,
            Err(error) => {
                log::warn!("playback: cannot decode the soundcloud track {id}: {error}");
                return None;
            }
        };
        log::debug!(
            "playback: {id} decodes at {} Hz, {} channels",
            decoder.sample_rate(),
            decoder.channels(),
        );
        let skip = match seekable && !at.is_zero() {
            true => match decoder.try_seek(at) {
                Ok(()) => Duration::ZERO,
                Err(error) => {
                    log::warn!(
                        "playback: cannot seek the soundcloud track {id} to {}s: {error}",
                        at.as_secs()
                    );
                    return None;
                }
            },
            false => at,
        };
        Some(Trimmed::new(decoder, skip, None))
    }
}

/// The segments an HLS media playlist lists, behind its init segment when it names one.
fn playlist(base: &Url, text: &str) -> Vec<Url> {
    let mut urls = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(map) = line.strip_prefix("#EXT-X-MAP:") {
            let uri = map
                .split(',')
                .find_map(|part| part.trim().strip_prefix("URI="))
                .map(|uri| uri.trim_matches('"'));
            if let Some(url) = uri.and_then(|uri| base.join(uri).ok()) {
                urls.push(url);
            }
        } else if !line.is_empty()
            && !line.starts_with('#')
            && let Ok(url) = base.join(line)
        {
            urls.push(url);
        }
    }
    urls
}

/// Opens one segment, asking again once if the CDN fails it.
async fn segment(http: &reqwest::Client, url: Url) -> Result<reqwest::Response> {
    let mut last = None;
    for _ in 0..ATTEMPTS {
        match http
            .get(url.clone())
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
        {
            Ok(response) => return Ok(response),
            Err(error) => last = Some(error.without_url()),
        }
    }
    Err(last.map_or_else(|| anyhow::anyhow!("no attempt was made"), Into::into))
        .context("cannot fetch a soundcloud stream segment")
}

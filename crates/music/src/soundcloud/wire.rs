//! What SoundCloud's web api answers with, and how each one reads as Sonora's own models. A
//! SoundCloud user stands in for an artist, and a playlist SoundCloud calls an album, EP or
//! single stands in for an album. Ids are SoundCloud's numbers, written out as text.

use std::time::Duration;

use serde::Deserialize;

use crate::{Album, ArtistRef, ReleaseType, SavedArtist, UserProfile};

/// The size SoundCloud names the small artwork it hands out by.
const SMALL: &str = "-large.";
/// The biggest square SoundCloud renders artwork at.
const LARGE: &str = "-t500x500.";
/// The artwork as it was uploaded.
const FULL: &str = "-original.";

/// One page of a listing.
#[derive(Deserialize)]
#[serde(bound = "T: Deserialize<'de>")]
pub(super) struct Collection<T> {
    #[serde(default = "Vec::new")]
    pub collection: Vec<T>,
    pub next_href: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct User {
    pub id: u64,
    pub username: String,
    pub full_name: Option<String>,
    pub avatar_url: Option<String>,
    pub description: Option<String>,
    pub followers_count: Option<u64>,
    pub followings_count: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct Track {
    pub id: u64,
    /// Empty on the stubs a long playlist lists past its first few tracks.
    pub title: String,
    pub duration: u64,
    pub artwork_url: Option<String>,
    pub permalink_url: Option<String>,
    pub user: Option<User>,
    pub publisher_metadata: Option<Publisher>,
    pub genre: Option<String>,
    pub playback_count: Option<u64>,
    pub streamable: Option<bool>,
    /// `ALLOW`, `MONETIZE`, `SNIP` for a preview only, or `BLOCK`.
    pub policy: Option<String>,
    pub media: Option<Media>,
    pub track_authorization: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct Publisher {
    pub artist: Option<String>,
    pub album_title: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct Media {
    pub transcodings: Vec<Transcoding>,
}

/// One format a track is served in. Its url still has to be asked for the address of the audio.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct Transcoding {
    pub url: String,
    pub preset: String,
    /// Set when only a preview is served this way.
    pub snipped: bool,
    pub format: Format,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct Format {
    /// `progressive`, `hls`, or an encrypted kind of HLS.
    pub protocol: String,
    pub mime_type: String,
}

/// Where a transcoding's audio is.
#[derive(Deserialize)]
pub(super) struct Location {
    pub url: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct Playlist {
    pub id: u64,
    pub title: String,
    pub artwork_url: Option<String>,
    pub permalink_url: Option<String>,
    pub user: Option<User>,
    pub sharing: Option<String>,
    pub track_count: Option<u32>,
    pub tracks: Vec<Track>,
    pub is_album: Option<bool>,
    /// `album`, `ep`, `single`, `compilation`, or empty for a playlist.
    pub set_type: Option<String>,
    pub release_date: Option<String>,
    pub created_at: Option<String>,
}

/// One entry of a user's likes, which holds a track or a playlist.
#[derive(Deserialize)]
pub(super) struct Like {
    pub track: Option<Track>,
    pub playlist: Option<Playlist>,
}

/// One entry of the play history.
#[derive(Deserialize)]
pub(super) struct Played {
    pub track: Option<Track>,
}

/// Whatever `/resolve` led to, read only for its number.
#[derive(Deserialize)]
pub(super) struct Resolved {
    pub id: u64,
}

/// The large artwork, worked out from the small url, or `None` when there is none.
pub(super) fn large(url: Option<&str>) -> Option<String> {
    url.map(|url| url.replacen(SMALL, LARGE, 1))
}

/// The artwork as it was uploaded, from the small or the large url.
pub(super) fn full(url: &str) -> String {
    url.replacen(LARGE, FULL, 1).replacen(SMALL, FULL, 1)
}

impl User {
    pub fn name(&self) -> String {
        self.full_name
            .clone()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| self.username.clone())
    }

    pub fn into_profile(self) -> UserProfile {
        UserProfile {
            display_name: self.name(),
            avatar: large(self.avatar_url.as_deref()),
            id: self.id.to_string(),
        }
    }

    pub fn into_artist(self) -> SavedArtist {
        SavedArtist {
            name: self.name(),
            cover: large(self.avatar_url.as_deref()),
            id: self.id.to_string(),
            added_at: None,
        }
    }

    fn reference(&self) -> ArtistRef {
        ArtistRef {
            name: self.name(),
            id: Some(self.id.to_string()),
        }
    }
}

impl Track {
    /// Whether this is only the id a long playlist lists in place of a track.
    pub fn stub(&self) -> bool {
        self.title.is_empty()
    }

    /// Whether the whole track plays here. A preview is only thirty seconds, so it counts as
    /// unplayable rather than as a track that stops short.
    pub fn playable(&self) -> bool {
        self.streamable != Some(false) && !matches!(self.policy.as_deref(), Some("BLOCK" | "SNIP"))
    }

    pub fn into_track(self) -> crate::Track {
        let user = self.user.clone().unwrap_or_default();
        let publisher = self.publisher_metadata.clone().unwrap_or_default();
        let artists = publisher
            .artist
            .filter(|artist| !artist.trim().is_empty())
            .unwrap_or_else(|| user.name());
        let cover =
            large(self.artwork_url.as_deref()).or_else(|| large(user.avatar_url.as_deref()));
        crate::Track {
            playable: self.playable(),
            id: Some(self.id.to_string()),
            name: self.title,
            artist_refs: vec![ArtistRef {
                name: artists.clone(),
                id: Some(user.id.to_string()),
            }],
            artists,
            album: publisher.album_title.unwrap_or_default(),
            album_id: None,
            cover,
            duration: Duration::from_millis(self.duration),
            added_at: None,
            added_by: None,
            playcount: self.playback_count,
            popularity: 0,
            explicit: false,
            track_number: 0,
            disc_number: 0,
            tags: self
                .genre
                .into_iter()
                .filter(|genre| !genre.is_empty())
                .collect(),
            languages: Vec::new(),
            credits: Vec::new(),
        }
    }
}

impl Playlist {
    /// What kind of release this is, or `None` for a playlist someone put together.
    pub fn release(&self) -> Option<ReleaseType> {
        match self.set_type.as_deref().unwrap_or_default() {
            "album" => Some(ReleaseType::Album),
            "ep" => Some(ReleaseType::Ep),
            "single" => Some(ReleaseType::Single),
            "compilation" => Some(ReleaseType::Compilation),
            _ if self.is_album == Some(true) => Some(ReleaseType::Album),
            _ => None,
        }
    }

    fn cover(&self) -> Option<String> {
        let first = self
            .tracks
            .iter()
            .find_map(|track| track.artwork_url.as_deref());
        large(self.artwork_url.as_deref().or(first))
    }

    /// The playlist as Sonora lists it, owned when `me` made it.
    pub fn to_playlist(&self, me: Option<&str>) -> crate::Playlist {
        let user = self.user.clone().unwrap_or_default();
        crate::Playlist {
            id: self.id.to_string(),
            name: self.title.clone(),
            owned: me.is_some_and(|me| me == user.id.to_string()),
            owner: user.name(),
            owner_id: user.id.to_string(),
            collaborative: false,
            blend: false,
            public: self.sharing.as_deref() != Some("private"),
            cover: self.cover(),
            track_count: self.track_count.unwrap_or_default(),
            modified_at: None,
        }
    }

    /// The playlist as an album, whatever SoundCloud calls it.
    pub fn to_album(&self) -> Album {
        let user = self.user.clone().unwrap_or_default();
        let date = self
            .release_date
            .clone()
            .filter(|date| !date.is_empty())
            .or_else(|| self.created_at.clone())
            .unwrap_or_default();
        let cover = self.cover();
        Album {
            id: self.id.to_string(),
            name: self.title.clone(),
            artists: user.name(),
            artist_refs: vec![user.reference()],
            cover_large: cover.clone(),
            cover,
            release_type: self.release().unwrap_or(ReleaseType::Album),
            year: date
                .get(..4)
                .and_then(|year| year.parse().ok())
                .unwrap_or_default(),
            track_count: self.track_count.unwrap_or_default(),
            release_date: date.get(..10).unwrap_or_default().to_owned(),
            label: String::new(),
            copyrights: Vec::new(),
            added_at: None,
        }
    }
}

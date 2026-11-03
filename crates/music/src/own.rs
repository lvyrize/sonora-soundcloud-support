//! Playlists that live in Sonora itself rather than on any service. One can hold songs from
//! every streaming service the session reaches, the main provider's and a companion's side by
//! side, and nothing about it is sent anywhere. Only what a song is gets kept, its id, name,
//! artists and cover, so every song still streams from its own service when it plays. Local
//! files are left out, since those have playlists of their own.
//!
//! The playlists are one small JSON file in Sonora's data folder, shared by every session in
//! the process. [`wrap`] puts them in front of a session's client: they come first in the
//! playlist list, a new playlist is always one of them, and every other call goes straight
//! through to the client.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};

use crate::{
    Album, AlbumCatalogue, AlbumDetail, Artist, ArtistCatalogue, ArtistProfile, CoverSize, Feed,
    Genre, GenreDetail, GenreSection, HomeFeed, ImportedPlaylist, MediaKind, MusicApi, Pages,
    PinOutcome, PinTarget, PinTargetKind, Playlist, PlaylistDetail, Report, SavedArtist, Track,
    TrackTags, UserDetail, UserProfile, credentials, distinct_covers,
};

/// What every id of a Sonora playlist starts with.
pub const PREFIX: &str = "sonora-playlist:";
/// The file the playlists are kept in, inside Sonora's data folder.
const FILE: &str = "playlists.json";
/// How many songs are looked up at once when many are added, so a whole album does not trip
/// a service's rate limit.
const AT_ONCE: usize = 8;

/// Whether `id` names one of Sonora's own playlists.
pub fn is_own_id(id: &str) -> bool {
    id.starts_with(PREFIX)
}

/// `inner` with Sonora's own playlists in front of it.
pub fn wrap(inner: Arc<dyn MusicApi>) -> Arc<dyn MusicApi> {
    Arc::new(Own { inner })
}

/// Every playlist, as written to the file.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Book {
    /// The number the next playlist is named by, so an id is never handed out twice.
    next: u64,
    playlists: Vec<Kept>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Kept {
    id: String,
    name: String,
    /// When it last changed, in seconds since the epoch.
    modified_at: i64,
    /// What each song is, in order. A song may be in it more than once.
    tracks: Vec<Track>,
}

struct Own {
    inner: Arc<dyn MusicApi>,
}

impl Kept {
    fn playlist(&self) -> Playlist {
        Playlist {
            id: self.id.clone(),
            name: self.name.clone(),
            owner: String::new(),
            owner_id: String::new(),
            owned: true,
            collaborative: false,
            blend: false,
            public: false,
            cover: distinct_covers(&self.tracks, 1).into_iter().next(),
            track_count: self.tracks.len() as u32,
            modified_at: Some(self.modified_at),
        }
    }

    fn detail(&self) -> PlaylistDetail {
        PlaylistDetail {
            playlist: self.playlist(),
            tracks: self.tracks.clone(),
            continuation: None,
        }
    }
}

impl Own {
    /// Reads one playlist out of the book.
    fn read<T>(&self, id: &str, read: impl FnOnce(&Kept) -> T) -> Result<T> {
        let book = book();
        let kept = book
            .playlists
            .iter()
            .find(|kept| kept.id == id)
            .context("the playlist is no longer there")?;
        Ok(read(kept))
    }

    /// Changes one playlist and writes the book back.
    fn change(&self, id: &str, change: impl FnOnce(&mut Kept)) -> Result<()> {
        let mut book = book();
        let kept = book
            .playlists
            .iter_mut()
            .find(|kept| kept.id == id)
            .context("the playlist is no longer there")?;
        change(kept);
        kept.modified_at = now();
        save(&book)
    }

    /// What each of `ids` is, as the services answer for them. A local file is refused, since
    /// these playlists only hold songs that stream.
    async fn songs(&self, ids: &[String]) -> Result<Vec<Track>> {
        if ids.iter().any(|id| crate::is_local_id(id)) {
            bail!("a Sonora playlist only holds songs that stream, not local files");
        }
        let inner = self.inner.clone();
        let fetched: Vec<Result<Track>> = futures::stream::iter(ids.to_vec())
            .map(move |id| {
                let inner = inner.clone();
                async move { inner.track(&id).await }
            })
            .buffered(AT_ONCE)
            .collect()
            .await;
        fetched
            .into_iter()
            .zip(ids)
            .map(|(track, id)| {
                let mut track = track.with_context(|| format!("cannot read the song {id}"))?;
                // the id it was added by is the one that routes it, whatever the answer says
                track.id = Some(id.clone());
                Ok(track)
            })
            .collect()
    }
}

#[async_trait]
impl MusicApi for Own {
    fn alive(&self) -> bool {
        self.inner.alive()
    }

    fn connect(&self) -> Option<Arc<crate::connect::Connect>> {
        self.inner.connect()
    }

    fn cover_at(&self, cover: &str, size: CoverSize) -> Option<String> {
        self.inner.cover_at(cover, size)
    }

    fn share_url(&self, kind: MediaKind, id: &str) -> Option<String> {
        match is_own_id(id) {
            true => None,
            false => self.inner.share_url(kind, id),
        }
    }

    async fn profile(&self) -> Result<UserProfile> {
        self.inner.profile().await
    }

    async fn user(&self, user_id: &str) -> Result<UserDetail> {
        self.inner.user(user_id).await
    }

    async fn artist(&self, artist_id: &str) -> Result<Artist> {
        self.inner.artist(artist_id).await
    }

    async fn artist_catalogue(&self, artist_id: &str, known: &[Track]) -> Result<ArtistCatalogue> {
        self.inner.artist_catalogue(artist_id, known).await
    }

    async fn artist_profile(&self, artist_id: &str) -> Result<ArtistProfile> {
        self.inner.artist_profile(artist_id).await
    }

    async fn artist_images(&self, ids: Vec<String>) -> Result<HashMap<String, String>> {
        self.inner.artist_images(ids).await
    }

    async fn saved_tracks(&self) -> Result<Vec<Track>> {
        self.inner.saved_tracks().await
    }

    async fn saved_tracks_paged(&self) -> Result<Pages<Track>> {
        self.inner.saved_tracks_paged().await
    }

    async fn all_tracks(&self) -> Result<Vec<Track>> {
        self.inner.all_tracks().await
    }

    async fn all_tracks_paged(&self) -> Result<Pages<Track>> {
        self.inner.all_tracks_paged().await
    }

    async fn set_track_saved(&self, track_id: &str, saved: bool) -> Result<()> {
        self.inner.set_track_saved(track_id, saved).await
    }

    async fn track_tags(&self, track_id: &str) -> Result<TrackTags> {
        self.inner.track_tags(track_id).await
    }

    async fn set_track_tags(&self, track_id: &str, tags: TrackTags) -> Result<()> {
        self.inner.set_track_tags(track_id, tags).await
    }

    async fn track(&self, track_id: &str) -> Result<Track> {
        self.inner.track(track_id).await
    }

    async fn delete_track_file(&self, track_id: &str) -> Result<()> {
        self.inner.delete_track_file(track_id).await
    }

    async fn track_playcount(&self, track_id: &str) -> Result<Option<u64>> {
        self.inner.track_playcount(track_id).await
    }

    async fn report(&self, track_id: &str, report: Report, position: Duration) -> Result<()> {
        self.inner.report(track_id, report, position).await
    }

    async fn played(&self, track_id: &str, at: SystemTime) -> Result<()> {
        self.inner.played(track_id, at).await
    }

    async fn report_play(&self, track_id: &str) -> Result<()> {
        self.inner.report_play(track_id).await
    }

    async fn recently_played(&self) -> Result<Vec<Track>> {
        self.inner.recently_played().await
    }

    /// Sonora's own playlists, newest change first, then the service's.
    async fn playlists(&self) -> Result<Vec<Playlist>> {
        let mut own: Vec<Playlist> = book().playlists.iter().map(Kept::playlist).collect();
        own.sort_by_key(|playlist| std::cmp::Reverse(playlist.modified_at));
        own.extend(self.inner.playlists().await?);
        Ok(own)
    }

    async fn set_pinned(&self, uri: &str, pinned: bool) -> Result<PinOutcome> {
        self.inner.set_pinned(uri, pinned).await
    }

    async fn pin_targets(&self) -> Result<Option<Vec<PinTarget>>> {
        self.inner.pin_targets().await
    }

    fn pin_uri(&self, kind: PinTargetKind, id: &str) -> Option<String> {
        match is_own_id(id) {
            true => None,
            false => self.inner.pin_uri(kind, id),
        }
    }

    /// A new playlist is always one of Sonora's, so it can take songs from any service.
    async fn create_playlist(&self, name: &str) -> Result<String> {
        let mut book = book();
        book.next += 1;
        let id = format!("{PREFIX}{}", book.next);
        book.playlists.push(Kept {
            id: id.clone(),
            name: name.to_owned(),
            modified_at: now(),
            tracks: Vec::new(),
        });
        save(&book)?;
        Ok(id)
    }

    async fn rename_playlist(&self, playlist_id: &str, name: &str) -> Result<()> {
        match is_own_id(playlist_id) {
            true => self.change(playlist_id, |kept| kept.name = name.to_owned()),
            false => self.inner.rename_playlist(playlist_id, name).await,
        }
    }

    async fn delete_playlist(&self, playlist_id: &str) -> Result<()> {
        if !is_own_id(playlist_id) {
            return self.inner.delete_playlist(playlist_id).await;
        }
        let mut book = book();
        book.playlists.retain(|kept| kept.id != playlist_id);
        save(&book)
    }

    /// Taking one of Sonora's playlists out of the library deletes it, since it is nowhere else.
    async fn remove_playlist_from_library(&self, playlist_id: &str) -> Result<()> {
        match is_own_id(playlist_id) {
            true => self.delete_playlist(playlist_id).await,
            false => self.inner.remove_playlist_from_library(playlist_id).await,
        }
    }

    async fn add_playlist_to_library(&self, playlist_id: &str) -> Result<()> {
        match is_own_id(playlist_id) {
            true => Ok(()),
            false => self.inner.add_playlist_to_library(playlist_id).await,
        }
    }

    async fn set_in_library(&self, kind: MediaKind, id: &str, present: bool) -> Result<()> {
        self.inner.set_in_library(kind, id, present).await
    }

    async fn set_playlist_public(&self, playlist_id: &str, public: bool) -> Result<()> {
        match is_own_id(playlist_id) {
            true => bail!("a Sonora playlist stays on this device"),
            false => self.inner.set_playlist_public(playlist_id, public).await,
        }
    }

    async fn set_playlist_cover(&self, playlist_id: &str, image: Option<&[u8]>) -> Result<()> {
        match is_own_id(playlist_id) {
            true => bail!("a Sonora playlist takes its cover from its songs"),
            false => self.inner.set_playlist_cover(playlist_id, image).await,
        }
    }

    async fn add_track_to_playlist(&self, playlist_id: &str, track_id: &str) -> Result<()> {
        self.add_tracks_to_playlist(playlist_id, &[track_id.to_owned()])
            .await
    }

    async fn add_tracks_to_playlist(&self, playlist_id: &str, track_ids: &[String]) -> Result<()> {
        if !is_own_id(playlist_id) {
            return self
                .inner
                .add_tracks_to_playlist(playlist_id, track_ids)
                .await;
        }
        if track_ids.is_empty() {
            return Ok(());
        }
        let songs = self.songs(track_ids).await?;
        self.change(playlist_id, |kept| kept.tracks.extend(songs))
    }

    async fn remove_track_from_playlist(&self, playlist_id: &str, track_id: &str) -> Result<()> {
        match is_own_id(playlist_id) {
            true => self.change(playlist_id, |kept| {
                kept.tracks
                    .retain(|track| track.id.as_deref() != Some(track_id));
            }),
            false => {
                self.inner
                    .remove_track_from_playlist(playlist_id, track_id)
                    .await
            }
        }
    }

    async fn import_playlist(&self, path: &Path) -> Result<ImportedPlaylist> {
        self.inner.import_playlist(path).await
    }

    async fn export_playlist(&self, playlist_id: &str, path: &Path) -> Result<()> {
        match is_own_id(playlist_id) {
            true => bail!("only a playlist of local files can be written to a file"),
            false => self.inner.export_playlist(playlist_id, path).await,
        }
    }

    async fn saved_albums(&self) -> Result<Vec<Album>> {
        self.inner.saved_albums().await
    }

    async fn all_albums(&self) -> Result<Vec<Album>> {
        self.inner.all_albums().await
    }

    async fn saved_albums_paged(&self) -> Result<Pages<Album>> {
        self.inner.saved_albums_paged().await
    }

    async fn all_albums_paged(&self) -> Result<Pages<Album>> {
        self.inner.all_albums_paged().await
    }

    async fn set_album_saved(&self, album_id: &str, saved: bool) -> Result<()> {
        self.inner.set_album_saved(album_id, saved).await
    }

    async fn saved_artists(&self) -> Result<Vec<SavedArtist>> {
        self.inner.saved_artists().await
    }

    async fn all_artists(&self) -> Result<Vec<SavedArtist>> {
        self.inner.all_artists().await
    }

    async fn saved_artists_paged(&self) -> Result<Pages<SavedArtist>> {
        self.inner.saved_artists_paged().await
    }

    async fn all_artists_paged(&self) -> Result<Pages<SavedArtist>> {
        self.inner.all_artists_paged().await
    }

    async fn set_artist_saved(&self, artist_id: &str, saved: bool) -> Result<()> {
        self.inner.set_artist_saved(artist_id, saved).await
    }

    async fn album(&self, album_id: &str) -> Result<AlbumDetail> {
        self.inner.album(album_id).await
    }

    async fn album_tracks(&self, album_id: &str) -> Result<Vec<Track>> {
        self.inner.album_tracks(album_id).await
    }

    async fn album_catalogue(
        &self,
        album_id: &str,
        artist_id: Option<&str>,
    ) -> Result<AlbumCatalogue> {
        self.inner.album_catalogue(album_id, artist_id).await
    }

    async fn playlist(&self, playlist_id: &str) -> Result<PlaylistDetail> {
        match is_own_id(playlist_id) {
            true => self.read(playlist_id, Kept::detail),
            false => self.inner.playlist(playlist_id).await,
        }
    }

    async fn playlist_continuation(
        &self,
        continuation: &str,
    ) -> Result<(Vec<Track>, Option<String>)> {
        self.inner.playlist_continuation(continuation).await
    }

    async fn playlist_tracks(&self, playlist_id: &str) -> Result<Vec<Track>> {
        match is_own_id(playlist_id) {
            true => self.read(playlist_id, |kept| kept.tracks.clone()),
            false => self.inner.playlist_tracks(playlist_id).await,
        }
    }

    async fn playlist_covers(&self, playlist_id: &str, wanted: usize) -> Result<Vec<String>> {
        match is_own_id(playlist_id) {
            true => self.read(playlist_id, |kept| distinct_covers(&kept.tracks, wanted)),
            false => self.inner.playlist_covers(playlist_id, wanted).await,
        }
    }

    async fn track_radio(
        &self,
        track_id: &str,
        from: Option<&str>,
    ) -> Result<(Vec<Track>, Option<String>)> {
        self.inner.track_radio(track_id, from).await
    }

    async fn search(&self, query: &str) -> Result<Vec<Track>> {
        self.inner.search(query).await
    }

    async fn search_albums(&self, query: &str) -> Result<Vec<Album>> {
        self.inner.search_albums(query).await
    }

    async fn search_playlists(&self, query: &str) -> Result<Vec<Playlist>> {
        self.inner.search_playlists(query).await
    }

    async fn home(&self) -> Result<HomeFeed> {
        self.inner.home().await
    }

    async fn home_paged(&self) -> Result<Feed> {
        self.inner.home_paged().await
    }

    async fn name_home_playlists(&self, sections: Vec<GenreSection>) -> Vec<GenreSection> {
        self.inner.name_home_playlists(sections).await
    }

    async fn genres(&self) -> Result<Vec<Genre>> {
        self.inner.genres().await
    }

    async fn genre(&self, genre_id: &str) -> Result<GenreDetail> {
        self.inner.genre(genre_id).await
    }

    async fn genre_cover(&self, genre_id: &str) -> Result<Option<String>> {
        self.inner.genre_cover(genre_id).await
    }
}

/// The playlists, read from the file the first time and kept in memory after. A file that
/// does not read is set aside rather than overwritten, so nothing in it is lost.
fn book() -> MutexGuard<'static, Book> {
    static BOOK: OnceLock<Mutex<Book>> = OnceLock::new();
    BOOK.get_or_init(|| Mutex::new(load()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn path() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("sonora").join(FILE))
}

fn load() -> Book {
    let Some(path) = path() else {
        return Book::default();
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Book::default(),
        Err(error) => {
            log::warn!("playlists: cannot read {}: {error}", path.display());
            return Book::default();
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(book) => book,
        Err(error) => {
            log::warn!(
                "playlists: {} does not read, setting it aside: {error}",
                path.display()
            );
            std::fs::rename(&path, path.with_extension("json.unreadable")).ok();
            Book::default()
        }
    }
}

fn save(book: &Book) -> Result<()> {
    let path = path().context("cannot find a folder for sonora's playlists")?;
    let bytes = serde_json::to_vec(book).context("cannot serialize sonora's playlists")?;
    credentials::write(&path, &bytes).context("cannot save sonora's playlists")
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

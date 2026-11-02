//! Two streaming providers at once: the main one, and a companion whose library, search results
//! and tracks sit beside it. To the rest of the app the pair is one provider. Every id of the
//! companion's carries its slug, as `soundcloud::123`, so a call is routed by the id alone, and
//! the ids are tagged on the way out and stripped on the way back in. Listings are merged, the
//! main provider first, and anything that belongs to one account, such as making a playlist or
//! the home page, stays the main provider's.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Result, bail};
use async_trait::async_trait;

use crate::link::Linked;
use crate::{
    Album, AlbumCatalogue, AlbumDetail, Artist, ArtistCatalogue, ArtistProfile, ArtistRef,
    CoverSize, Feed, Fraction, Genre, GenreDetail, GenreItem, GenreSection, HomeFeed,
    ImportedPlaylist, MediaKind, MusicApi, Page, Pages, PinOutcome, PinTarget, PinTargetKind,
    PlaybackConfig, PlaybackEvent, PlaybackEvents, PlaybackFactory, Player, Playlist,
    PlaylistDetail, ProviderSession, Report, SavedArtist, Spectrum, Track, TrackTags, UserDetail,
};

/// What joins a companion's slug to one of its ids.
const SEPARATOR: &str = "::";
/// How many pages a merged listing holds back before the reader takes them.
const BACKLOG: usize = 8;

/// The id `id` of the companion `slug` as the app sees it.
pub fn tag(slug: &str, id: &str) -> String {
    format!("{slug}{SEPARATOR}{id}")
}

/// The slug and the provider's own id of a companion's id, or `None` for anyone else's.
pub fn split(id: &str) -> Option<(&str, &str)> {
    let (slug, rest) = id.split_once(SEPARATOR)?;
    (!slug.is_empty() && slug.bytes().all(|byte| byte.is_ascii_lowercase())).then_some((slug, rest))
}

/// Whether `id` is a companion's, which the main provider's own services, such as its device
/// network and shared sessions, know nothing of.
pub fn is_companion_id(id: &str) -> bool {
    split(id).is_some()
}

/// What a link of the companion's points at, in the ids the app sees.
pub fn tag_linked(slug: &str, linked: Linked) -> Linked {
    match linked {
        Linked::Track(id) => Linked::Track(tag(slug, &id)),
        Linked::Album(id) => Linked::Album(tag(slug, &id)),
        Linked::Artist(id) => Linked::Artist(tag(slug, &id)),
        Linked::Playlist(id) => Linked::Playlist(tag(slug, &id)),
        Linked::User(id) => Linked::User(tag(slug, &id)),
        other => other,
    }
}

/// Puts the companion's session beside the main one. What the app is offered, from the
/// profile to what the provider can do, stays the main provider's; the companion's own limits
/// show up as a call that fails, which every caller already handles.
pub fn combine(
    main: ProviderSession,
    slug: &'static str,
    companion: ProviderSession,
) -> ProviderSession {
    ProviderSession {
        api: Arc::new(Combined {
            main: main.api,
            other: companion.api,
            slug,
        }),
        playback: Arc::new(Factory {
            main: main.playback,
            other: companion.playback,
            slug,
        }),
        ..main
    }
}

/// Which provider of the pair something belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Main,
    Other,
}

struct Combined {
    main: Arc<dyn MusicApi>,
    other: Arc<dyn MusicApi>,
    slug: &'static str,
}

/// Rewrites every id a model carries, so the companion's can be tagged or stripped in one place.
trait Ids {
    fn ids(self, map: &impl Fn(String) -> String) -> Self;
}

fn opt(id: Option<String>, map: &impl Fn(String) -> String) -> Option<String> {
    id.map(map)
}

fn all<T: Ids>(items: Vec<T>, map: &impl Fn(String) -> String) -> Vec<T> {
    items.into_iter().map(|item| item.ids(map)).collect()
}

impl Ids for ArtistRef {
    fn ids(self, map: &impl Fn(String) -> String) -> Self {
        Self {
            id: opt(self.id, map),
            ..self
        }
    }
}

impl Ids for Track {
    fn ids(self, map: &impl Fn(String) -> String) -> Self {
        Self {
            id: opt(self.id, map),
            album_id: opt(self.album_id, map),
            artist_refs: all(self.artist_refs, map),
            credits: self
                .credits
                .into_iter()
                .map(|credit| crate::Credit {
                    id: opt(credit.id, map),
                    ..credit
                })
                .collect(),
            ..self
        }
    }
}

impl Ids for Album {
    fn ids(self, map: &impl Fn(String) -> String) -> Self {
        Self {
            id: map(self.id),
            artist_refs: all(self.artist_refs, map),
            ..self
        }
    }
}

impl Ids for Playlist {
    fn ids(self, map: &impl Fn(String) -> String) -> Self {
        Self {
            id: map(self.id),
            owner_id: map(self.owner_id),
            ..self
        }
    }
}

impl Ids for SavedArtist {
    fn ids(self, map: &impl Fn(String) -> String) -> Self {
        Self {
            id: map(self.id),
            ..self
        }
    }
}

impl Ids for Genre {
    fn ids(self, map: &impl Fn(String) -> String) -> Self {
        Self {
            id: map(self.id),
            ..self
        }
    }
}

impl Ids for GenreItem {
    fn ids(self, map: &impl Fn(String) -> String) -> Self {
        match self {
            Self::Playlist(playlist) => Self::Playlist(playlist.ids(map)),
            Self::Album(album) => Self::Album(album.ids(map)),
            Self::Genre(genre) => Self::Genre(genre.ids(map)),
            Self::Track(track) => Self::Track(track.ids(map)),
            Self::Artist(artist) => Self::Artist(artist.ids(map)),
        }
    }
}

impl Ids for GenreSection {
    fn ids(self, map: &impl Fn(String) -> String) -> Self {
        Self {
            items: all(self.items, map),
            ..self
        }
    }
}

impl Ids for HomeFeed {
    fn ids(self, map: &impl Fn(String) -> String) -> Self {
        Self {
            listen_again: all(self.listen_again, map),
            quick_picks: self.quick_picks.map(|tracks| all(tracks, map)),
            sections: all(self.sections, map),
        }
    }
}

impl Ids for GenreDetail {
    fn ids(self, map: &impl Fn(String) -> String) -> Self {
        Self {
            sections: all(self.sections, map),
            ..self
        }
    }
}

impl Ids for Artist {
    fn ids(self, map: &impl Fn(String) -> String) -> Self {
        Self {
            top_tracks: all(self.top_tracks, map),
            albums: all(self.albums, map),
            ..self
        }
    }
}

impl Ids for ArtistCatalogue {
    fn ids(self, map: &impl Fn(String) -> String) -> Self {
        Self {
            albums: all(self.albums, map),
            top_tracks: all(self.top_tracks, map),
            appears_on: all(self.appears_on, map),
        }
    }
}

impl Ids for AlbumDetail {
    fn ids(self, map: &impl Fn(String) -> String) -> Self {
        Self {
            album: self.album.ids(map),
            tracks: all(self.tracks, map),
        }
    }
}

impl Ids for AlbumCatalogue {
    fn ids(self, map: &impl Fn(String) -> String) -> Self {
        Self {
            also_like: all(self.also_like, map),
            similar: all(self.similar, map),
        }
    }
}

impl Ids for PlaylistDetail {
    fn ids(self, map: &impl Fn(String) -> String) -> Self {
        Self {
            playlist: self.playlist.ids(map),
            tracks: all(self.tracks, map),
            continuation: opt(self.continuation, map),
        }
    }
}

impl Ids for UserDetail {
    fn ids(self, map: &impl Fn(String) -> String) -> Self {
        Self {
            id: map(self.id),
            playlists: all(self.playlists, map),
            ..self
        }
    }
}

impl Combined {
    /// Which provider `id` belongs to, and the id that provider knows it by.
    fn route<'a>(&self, id: &'a str) -> (Side, &'a str) {
        match split(id) {
            Some((slug, rest)) if slug == self.slug => (Side::Other, rest),
            _ => (Side::Main, id),
        }
    }

    fn api(&self, side: Side) -> &dyn MusicApi {
        match side {
            Side::Main => self.main.as_ref(),
            Side::Other => self.other.as_ref(),
        }
    }

    /// The companion's models as the app sees them. The main provider's pass through as they are.
    fn tagged<T: Ids>(&self, side: Side, value: T) -> T {
        match side {
            Side::Main => value,
            Side::Other => value.ids(&|id| tag(self.slug, &id)),
        }
    }

    /// Tracks of `side` handed back to that provider, without the tag.
    fn stripped(&self, tracks: &[Track]) -> Vec<Track> {
        tracks
            .iter()
            .cloned()
            .map(|track| track.ids(&|id| self.route(&id).1.to_owned()))
            .collect()
    }

    /// The companion's part of a merged listing. A companion that fails leaves the main
    /// provider's part standing, since it is only an addition.
    async fn extra<T: Ids>(&self, what: &str, listed: Result<Vec<T>>) -> Vec<T> {
        match listed {
            Ok(items) => all(items, &|id| tag(self.slug, &id)),
            Err(error) => {
                log::warn!("combined: the {} {what} did not load: {error:#}", self.slug);
                Vec::new()
            }
        }
    }

    /// The main provider's pages, then the companion's whole listing as one more page.
    fn chained<T, F>(&self, main: Pages<T>, other: F) -> Pages<T>
    where
        T: Ids + Send + 'static,
        F: std::future::Future<Output = Result<Vec<T>>> + Send + 'static,
    {
        let (sender, receiver) = tokio::sync::mpsc::channel(BACKLOG);
        let slug = self.slug;
        tokio::spawn(async move {
            let mut main = main;
            while let Some(page) = main.recv().await {
                // The total covers the main provider alone, so it would hide the companion's.
                let page = page.map(|page| Page {
                    total: None,
                    items: page.items,
                });
                if sender.send(page).await.is_err() {
                    return;
                }
            }
            match other.await {
                Ok(items) => {
                    let items = all(items, &|id| tag(slug, &id));
                    let total = None;
                    sender.send(Ok(Page { total, items })).await.ok();
                }
                Err(error) => log::warn!("combined: the {slug} listing did not load: {error:#}"),
            }
        });
        receiver
    }

    /// Two answers to the same question, alternating, so a search shows both providers near
    /// the top.
    async fn interleaved<T: Ids>(
        &self,
        what: &str,
        main: Result<Vec<T>>,
        other: Result<Vec<T>>,
    ) -> Result<Vec<T>> {
        let other = self.extra(what, other).await;
        let main = match main {
            Ok(main) => main,
            Err(error) if !other.is_empty() => {
                log::warn!("combined: the main {what} did not load: {error:#}");
                Vec::new()
            }
            Err(error) => return Err(error),
        };
        let mut merged = Vec::with_capacity(main.len() + other.len());
        let mut main = main.into_iter();
        let mut other = other.into_iter();
        loop {
            match (main.next(), other.next()) {
                (None, None) => break,
                (first, second) => merged.extend(first.into_iter().chain(second)),
            }
        }
        Ok(merged)
    }

    /// Refuses a change that would put one provider's track into the other's playlist.
    fn same_side(&self, playlist: Side, track_id: &str) -> Result<()> {
        if self.route(track_id).0 != playlist {
            bail!("a playlist can only hold tracks of its own provider");
        }
        Ok(())
    }
}

#[async_trait]
impl MusicApi for Combined {
    fn alive(&self) -> bool {
        self.main.alive()
    }

    fn connect(&self) -> Option<Arc<crate::connect::Connect>> {
        self.main.connect()
    }

    fn cover_at(&self, cover: &str, size: CoverSize) -> Option<String> {
        self.main
            .cover_at(cover, size)
            .or_else(|| self.other.cover_at(cover, size))
    }

    fn share_url(&self, kind: MediaKind, id: &str) -> Option<String> {
        let (side, id) = self.route(id);
        self.api(side).share_url(kind, id)
    }

    async fn profile(&self) -> Result<crate::UserProfile> {
        self.main.profile().await
    }

    async fn user(&self, user_id: &str) -> Result<UserDetail> {
        let (side, id) = self.route(user_id);
        Ok(self.tagged(side, self.api(side).user(id).await?))
    }

    async fn artist(&self, artist_id: &str) -> Result<Artist> {
        let (side, id) = self.route(artist_id);
        Ok(self.tagged(side, self.api(side).artist(id).await?))
    }

    async fn artist_catalogue(&self, artist_id: &str, known: &[Track]) -> Result<ArtistCatalogue> {
        let (side, id) = self.route(artist_id);
        let known = self.stripped(known);
        Ok(self.tagged(side, self.api(side).artist_catalogue(id, &known).await?))
    }

    async fn artist_profile(&self, artist_id: &str) -> Result<ArtistProfile> {
        let (side, id) = self.route(artist_id);
        self.api(side).artist_profile(id).await
    }

    async fn artist_images(&self, ids: Vec<String>) -> Result<HashMap<String, String>> {
        let (theirs, ours): (Vec<String>, Vec<String>) = ids
            .into_iter()
            .partition(|id| self.route(id).0 == Side::Other);
        let mut images = match ours.is_empty() {
            true => HashMap::new(),
            false => self.main.artist_images(ours).await?,
        };
        if !theirs.is_empty() {
            let plain = theirs
                .iter()
                .map(|id| self.route(id).1.to_owned())
                .collect();
            match self.other.artist_images(plain).await {
                Ok(found) => images.extend(
                    found
                        .into_iter()
                        .map(|(id, image)| (tag(self.slug, &id), image)),
                ),
                Err(error) => log::warn!("combined: {} artist images: {error:#}", self.slug),
            }
        }
        Ok(images)
    }

    async fn saved_tracks(&self) -> Result<Vec<Track>> {
        let (main, other) = tokio::join!(self.main.saved_tracks(), self.other.saved_tracks());
        let mut tracks = main?;
        tracks.extend(self.extra("favorites", other).await);
        Ok(tracks)
    }

    async fn saved_tracks_paged(&self) -> Result<Pages<Track>> {
        let other = self.other.clone();
        let main = self.main.saved_tracks_paged().await?;
        Ok(self.chained(main, async move { other.saved_tracks().await }))
    }

    async fn all_tracks(&self) -> Result<Vec<Track>> {
        self.main.all_tracks().await
    }

    async fn all_tracks_paged(&self) -> Result<Pages<Track>> {
        self.main.all_tracks_paged().await
    }

    async fn set_track_saved(&self, track_id: &str, saved: bool) -> Result<()> {
        let (side, id) = self.route(track_id);
        self.api(side).set_track_saved(id, saved).await
    }

    async fn track_tags(&self, track_id: &str) -> Result<TrackTags> {
        let (side, id) = self.route(track_id);
        self.api(side).track_tags(id).await
    }

    async fn set_track_tags(&self, track_id: &str, tags: TrackTags) -> Result<()> {
        let (side, id) = self.route(track_id);
        self.api(side).set_track_tags(id, tags).await
    }

    async fn track(&self, track_id: &str) -> Result<Track> {
        let (side, id) = self.route(track_id);
        Ok(self.tagged(side, self.api(side).track(id).await?))
    }

    async fn delete_track_file(&self, track_id: &str) -> Result<()> {
        let (side, id) = self.route(track_id);
        self.api(side).delete_track_file(id).await
    }

    async fn track_playcount(&self, track_id: &str) -> Result<Option<u64>> {
        let (side, id) = self.route(track_id);
        self.api(side).track_playcount(id).await
    }

    async fn report(&self, track_id: &str, report: Report, position: Duration) -> Result<()> {
        let (side, id) = self.route(track_id);
        self.api(side).report(id, report, position).await
    }

    async fn played(&self, track_id: &str, at: SystemTime) -> Result<()> {
        let (side, id) = self.route(track_id);
        self.api(side).played(id, at).await
    }

    async fn report_play(&self, track_id: &str) -> Result<()> {
        let (side, id) = self.route(track_id);
        self.api(side).report_play(id).await
    }

    async fn recently_played(&self) -> Result<Vec<Track>> {
        let (main, other) = tokio::join!(self.main.recently_played(), self.other.recently_played());
        let mut tracks = main?;
        tracks.extend(self.extra("history", other).await);
        Ok(tracks)
    }

    async fn playlists(&self) -> Result<Vec<Playlist>> {
        let (main, other) = tokio::join!(self.main.playlists(), self.other.playlists());
        let mut playlists = main?;
        playlists.extend(self.extra("playlists", other).await);
        Ok(playlists)
    }

    async fn set_pinned(&self, uri: &str, pinned: bool) -> Result<PinOutcome> {
        self.main.set_pinned(uri, pinned).await
    }

    async fn pin_targets(&self) -> Result<Option<Vec<PinTarget>>> {
        self.main.pin_targets().await
    }

    fn pin_uri(&self, kind: PinTargetKind, id: &str) -> Option<String> {
        match self.route(id) {
            (Side::Main, id) => self.main.pin_uri(kind, id),
            (Side::Other, _) => None,
        }
    }

    async fn create_playlist(&self, name: &str) -> Result<String> {
        self.main.create_playlist(name).await
    }

    async fn rename_playlist(&self, playlist_id: &str, name: &str) -> Result<()> {
        let (side, id) = self.route(playlist_id);
        self.api(side).rename_playlist(id, name).await
    }

    async fn delete_playlist(&self, playlist_id: &str) -> Result<()> {
        let (side, id) = self.route(playlist_id);
        self.api(side).delete_playlist(id).await
    }

    async fn remove_playlist_from_library(&self, playlist_id: &str) -> Result<()> {
        let (side, id) = self.route(playlist_id);
        self.api(side).remove_playlist_from_library(id).await
    }

    async fn add_playlist_to_library(&self, playlist_id: &str) -> Result<()> {
        let (side, id) = self.route(playlist_id);
        self.api(side).add_playlist_to_library(id).await
    }

    async fn set_in_library(&self, kind: MediaKind, id: &str, present: bool) -> Result<()> {
        let (side, id) = self.route(id);
        self.api(side).set_in_library(kind, id, present).await
    }

    async fn set_playlist_public(&self, playlist_id: &str, public: bool) -> Result<()> {
        let (side, id) = self.route(playlist_id);
        self.api(side).set_playlist_public(id, public).await
    }

    async fn set_playlist_cover(&self, playlist_id: &str, image: Option<&[u8]>) -> Result<()> {
        let (side, id) = self.route(playlist_id);
        self.api(side).set_playlist_cover(id, image).await
    }

    async fn add_track_to_playlist(&self, playlist_id: &str, track_id: &str) -> Result<()> {
        let (side, id) = self.route(playlist_id);
        self.same_side(side, track_id)?;
        self.api(side)
            .add_track_to_playlist(id, self.route(track_id).1)
            .await
    }

    async fn add_tracks_to_playlist(&self, playlist_id: &str, track_ids: &[String]) -> Result<()> {
        let (side, id) = self.route(playlist_id);
        for track_id in track_ids {
            self.same_side(side, track_id)?;
        }
        let plain: Vec<String> = track_ids
            .iter()
            .map(|track_id| self.route(track_id).1.to_owned())
            .collect();
        self.api(side).add_tracks_to_playlist(id, &plain).await
    }

    async fn remove_track_from_playlist(&self, playlist_id: &str, track_id: &str) -> Result<()> {
        let (side, id) = self.route(playlist_id);
        self.same_side(side, track_id)?;
        self.api(side)
            .remove_track_from_playlist(id, self.route(track_id).1)
            .await
    }

    async fn import_playlist(&self, path: &Path) -> Result<ImportedPlaylist> {
        self.main.import_playlist(path).await
    }

    async fn export_playlist(&self, playlist_id: &str, path: &Path) -> Result<()> {
        let (side, id) = self.route(playlist_id);
        self.api(side).export_playlist(id, path).await
    }

    async fn saved_albums(&self) -> Result<Vec<Album>> {
        let (main, other) = tokio::join!(self.main.saved_albums(), self.other.saved_albums());
        let mut albums = main?;
        albums.extend(self.extra("albums", other).await);
        Ok(albums)
    }

    async fn all_albums(&self) -> Result<Vec<Album>> {
        self.main.all_albums().await
    }

    async fn saved_albums_paged(&self) -> Result<Pages<Album>> {
        let other = self.other.clone();
        let main = self.main.saved_albums_paged().await?;
        Ok(self.chained(main, async move { other.saved_albums().await }))
    }

    async fn all_albums_paged(&self) -> Result<Pages<Album>> {
        self.main.all_albums_paged().await
    }

    async fn set_album_saved(&self, album_id: &str, saved: bool) -> Result<()> {
        let (side, id) = self.route(album_id);
        self.api(side).set_album_saved(id, saved).await
    }

    async fn saved_artists(&self) -> Result<Vec<SavedArtist>> {
        let (main, other) = tokio::join!(self.main.saved_artists(), self.other.saved_artists());
        let mut artists = main?;
        artists.extend(self.extra("artists", other).await);
        Ok(artists)
    }

    async fn all_artists(&self) -> Result<Vec<SavedArtist>> {
        self.main.all_artists().await
    }

    async fn saved_artists_paged(&self) -> Result<Pages<SavedArtist>> {
        let other = self.other.clone();
        let main = self.main.saved_artists_paged().await?;
        Ok(self.chained(main, async move { other.saved_artists().await }))
    }

    async fn all_artists_paged(&self) -> Result<Pages<SavedArtist>> {
        self.main.all_artists_paged().await
    }

    async fn set_artist_saved(&self, artist_id: &str, saved: bool) -> Result<()> {
        let (side, id) = self.route(artist_id);
        self.api(side).set_artist_saved(id, saved).await
    }

    async fn album(&self, album_id: &str) -> Result<AlbumDetail> {
        let (side, id) = self.route(album_id);
        Ok(self.tagged(side, self.api(side).album(id).await?))
    }

    async fn album_tracks(&self, album_id: &str) -> Result<Vec<Track>> {
        let (side, id) = self.route(album_id);
        Ok(self.tagged_all(side, self.api(side).album_tracks(id).await?))
    }

    async fn album_catalogue(
        &self,
        album_id: &str,
        artist_id: Option<&str>,
    ) -> Result<AlbumCatalogue> {
        let (side, id) = self.route(album_id);
        let artist = artist_id
            .map(|artist| self.route(artist))
            .filter(|(artist_side, _)| *artist_side == side)
            .map(|(_, artist)| artist);
        Ok(self.tagged(side, self.api(side).album_catalogue(id, artist).await?))
    }

    async fn playlist(&self, playlist_id: &str) -> Result<PlaylistDetail> {
        let (side, id) = self.route(playlist_id);
        Ok(self.tagged(side, self.api(side).playlist(id).await?))
    }

    async fn playlist_continuation(
        &self,
        continuation: &str,
    ) -> Result<(Vec<Track>, Option<String>)> {
        let (side, plain) = self.route(continuation);
        let (tracks, next) = self.api(side).playlist_continuation(plain).await?;
        let next = match side {
            Side::Main => next,
            Side::Other => next.map(|next| tag(self.slug, &next)),
        };
        Ok((self.tagged_all(side, tracks), next))
    }

    async fn playlist_tracks(&self, playlist_id: &str) -> Result<Vec<Track>> {
        let (side, id) = self.route(playlist_id);
        Ok(self.tagged_all(side, self.api(side).playlist_tracks(id).await?))
    }

    async fn playlist_covers(&self, playlist_id: &str, wanted: usize) -> Result<Vec<String>> {
        let (side, id) = self.route(playlist_id);
        self.api(side).playlist_covers(id, wanted).await
    }

    async fn track_radio(
        &self,
        track_id: &str,
        from: Option<&str>,
    ) -> Result<(Vec<Track>, Option<String>)> {
        let (side, id) = self.route(track_id);
        let from = from.map(|from| self.route(from).1);
        let (tracks, next) = self.api(side).track_radio(id, from).await?;
        let next = match side {
            Side::Main => next,
            Side::Other => next.map(|next| tag(self.slug, &next)),
        };
        Ok((self.tagged_all(side, tracks), next))
    }

    async fn search(&self, query: &str) -> Result<Vec<Track>> {
        let (main, other) = tokio::join!(self.main.search(query), self.other.search(query));
        self.interleaved("search", main, other).await
    }

    async fn search_albums(&self, query: &str) -> Result<Vec<Album>> {
        let (main, other) = tokio::join!(
            self.main.search_albums(query),
            self.other.search_albums(query)
        );
        self.interleaved("album search", main, other).await
    }

    async fn search_playlists(&self, query: &str) -> Result<Vec<Playlist>> {
        let (main, other) = tokio::join!(
            self.main.search_playlists(query),
            self.other.search_playlists(query)
        );
        self.interleaved("playlist search", main, other).await
    }

    async fn home(&self) -> Result<HomeFeed> {
        self.main.home().await
    }

    async fn home_paged(&self) -> Result<Feed> {
        self.main.home_paged().await
    }

    async fn name_home_playlists(&self, sections: Vec<GenreSection>) -> Vec<GenreSection> {
        self.main.name_home_playlists(sections).await
    }

    async fn genres(&self) -> Result<Vec<Genre>> {
        self.main.genres().await
    }

    async fn genre(&self, genre_id: &str) -> Result<GenreDetail> {
        let (side, id) = self.route(genre_id);
        Ok(self.tagged(side, self.api(side).genre(id).await?))
    }

    async fn genre_cover(&self, genre_id: &str) -> Result<Option<String>> {
        let (side, id) = self.route(genre_id);
        self.api(side).genre_cover(id).await
    }
}

impl Combined {
    fn tagged_all(&self, side: Side, tracks: Vec<Track>) -> Vec<Track> {
        match side {
            Side::Main => tracks,
            Side::Other => all(tracks, &|id| tag(self.slug, &id)),
        }
    }
}

/// Starts both providers' engines and hands each track to the one it belongs to.
struct Factory {
    main: Arc<dyn PlaybackFactory>,
    other: Arc<dyn PlaybackFactory>,
    slug: &'static str,
}

impl PlaybackFactory for Factory {
    fn start(&self, config: PlaybackConfig) -> (Box<dyn Player>, Box<dyn PlaybackEvents>) {
        let (main, main_events) = self.main.start(config.clone());
        let (other, other_events) = self.other.start(config);
        let playing_other = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        relay(
            main_events,
            Side::Main,
            None,
            playing_other.clone(),
            sender.clone(),
        );
        relay(
            other_events,
            Side::Other,
            Some(self.slug),
            playing_other.clone(),
            sender,
        );
        (
            Box::new(Pair {
                main,
                other,
                slug: self.slug,
                playing_other,
            }),
            Box::new(Merged(receiver)),
        )
    }
}

/// The two players as one. Only the one holding the current track is ever playing; loading a
/// track of the other provider pauses it first.
struct Pair {
    main: Box<dyn Player>,
    other: Box<dyn Player>,
    slug: &'static str,
    /// Whether the companion holds the current track, shared with the event relays.
    playing_other: Arc<AtomicBool>,
}

impl Pair {
    fn route<'a>(&self, id: &'a str) -> (Side, &'a str) {
        match split(id) {
            Some((slug, rest)) if slug == self.slug => (Side::Other, rest),
            _ => (Side::Main, id),
        }
    }

    fn player(&self, side: Side) -> &dyn Player {
        match side {
            Side::Main => self.main.as_ref(),
            Side::Other => self.other.as_ref(),
        }
    }

    fn current(&self) -> Side {
        match self.playing_other.load(Ordering::Acquire) {
            true => Side::Other,
            false => Side::Main,
        }
    }

    /// Makes `side` the one that plays, pausing the other when it held the current track.
    fn take_over(&self, side: Side) {
        let before = self.current();
        if before != side {
            self.player(before).pause();
            self.playing_other
                .store(side == Side::Other, Ordering::Release);
        }
    }
}

impl Player for Pair {
    fn load(&self, track_id: &str, at: Duration, seamless: bool) -> Result<()> {
        let (side, id) = self.route(track_id);
        let seamless = seamless && side == self.current();
        self.take_over(side);
        self.player(side).load(id, at, seamless)
    }

    fn load_paused_at(&self, track_id: &str, at: Duration) -> Result<()> {
        let (side, id) = self.route(track_id);
        self.take_over(side);
        self.player(side).load_paused_at(id, at)
    }

    fn preload(&self, track_id: &str, segue: bool) -> Result<()> {
        let (side, id) = self.route(track_id);
        // a gapless join only exists within one engine
        let segue = segue && side == self.current();
        self.player(side).preload(id, segue)
    }

    fn play(&self) {
        self.player(self.current()).play();
    }

    fn pause(&self) {
        self.player(self.current()).pause();
    }

    fn seek(&self, position: Duration) {
        self.player(self.current()).seek(position);
    }

    fn set_gain(&self, gain: f32) {
        self.main.set_gain(gain);
        self.other.set_gain(gain);
    }

    fn spectrum(&self) -> Option<Spectrum> {
        self.player(self.current()).spectrum()
    }

    fn download(
        &self,
        track_id: &str,
        progress: Fraction,
    ) -> futures::future::BoxFuture<'static, Result<()>> {
        let (side, id) = self.route(track_id);
        self.player(side).download(id, progress)
    }

    fn downloaded(&self, track_id: &str) -> bool {
        let (side, id) = self.route(track_id);
        self.player(side).downloaded(id)
    }

    fn plays_offline(&self, track_id: &str) -> bool {
        let (side, id) = self.route(track_id);
        self.player(side).plays_offline(id)
    }

    fn discard(&self, track_id: &str) {
        let (side, id) = self.route(track_id);
        self.player(side).discard(id);
    }
}

/// Both engines' events, as one stream.
struct Merged(tokio::sync::mpsc::UnboundedReceiver<PlaybackEvent>);

#[async_trait]
impl PlaybackEvents for Merged {
    async fn next(&mut self) -> Option<PlaybackEvent> {
        self.0.recv().await
    }
}

/// Pulls one engine's events and passes them on, the companion's with
/// their ids tagged. What the engine not holding the current track says about the transport,
/// such as the pause that handed the current track over, is dropped; what it says about a
/// particular track, such as a preload's signal, still goes through.
fn relay(
    mut events: Box<dyn PlaybackEvents>,
    side: Side,
    slug: Option<&'static str>,
    playing_other: Arc<AtomicBool>,
    sender: tokio::sync::mpsc::UnboundedSender<PlaybackEvent>,
) {
    let name = match side {
        Side::Main => "combined-main-events",
        Side::Other => "combined-other-events",
    };
    let pump = async move {
        while let Some(event) = events.next().await {
            let current = playing_other.load(Ordering::Acquire) == (side == Side::Other);
            let lasting = matches!(
                event,
                PlaybackEvent::Signal { .. }
                    | PlaybackEvent::Downloaded { .. }
                    | PlaybackEvent::Length { .. }
            );
            if !current && !lasting {
                continue;
            }
            let event = match slug {
                Some(slug) => tagged_event(slug, event),
                None => event,
            };
            if sender.send(event).is_err() {
                return;
            }
        }
    };
    // inside the app's runtime the pump is one more task; anywhere else it gets a thread
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(pump);
        return;
    }
    let spawned = std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || futures::executor::block_on(pump));
    if let Err(error) = spawned {
        log::warn!("combined: cannot start the {name} thread: {error}");
    }
}

/// An event of the companion's with its track id as the app knows it.
fn tagged_event(slug: &str, event: PlaybackEvent) -> PlaybackEvent {
    let tag = |id: Option<String>| id.map(|id| tag(slug, &id));
    match event {
        PlaybackEvent::Loading { id, at } => PlaybackEvent::Loading { id: tag(id), at },
        PlaybackEvent::Playing { id, at } => PlaybackEvent::Playing { id: tag(id), at },
        PlaybackEvent::Paused { id, at } => PlaybackEvent::Paused { id: tag(id), at },
        PlaybackEvent::Position { id, at } => PlaybackEvent::Position { id: tag(id), at },
        PlaybackEvent::Seeked { id, at } => PlaybackEvent::Seeked { id: tag(id), at },
        PlaybackEvent::Length { id, duration } => PlaybackEvent::Length {
            id: tag(id),
            duration,
        },
        PlaybackEvent::Downloaded { id } => PlaybackEvent::Downloaded { id: tag(id) },
        PlaybackEvent::Ended { id } => PlaybackEvent::Ended { id: tag(id) },
        PlaybackEvent::Unavailable { id } => PlaybackEvent::Unavailable { id: tag(id) },
        PlaybackEvent::Throttled { id } => PlaybackEvent::Throttled { id: tag(id) },
        PlaybackEvent::Signal { id, track } => PlaybackEvent::Signal {
            id: crate::combined::tag(slug, &id),
            track,
        },
        other => other,
    }
}

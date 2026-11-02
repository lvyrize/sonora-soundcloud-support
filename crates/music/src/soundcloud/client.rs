//! SoundCloud's web api at `api-v2.soundcloud.com`, the one soundcloud.com itself calls. Every
//! request carries the website's client id, and a signed-in one also the `oauth_token` cookie as
//! its bearer token. A guest has no token, so everything of the listener's own answers empty.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use reqwest::{Method, Response, StatusCode, Url};
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::soundcloud::key;
use crate::soundcloud::wire::{self, Collection, Like, Played, Resolved};
use crate::{
    Album, AlbumDetail, Artist, ArtistProfile, CoverSize, MediaKind, MusicApi, Playlist,
    PlaylistDetail, SavedArtist, Throttled, Track, UserDetail, UserProfile, distinct_covers,
};

const API: &str = "https://api-v2.soundcloud.com";
/// The site the api answers for, which every call names as its origin as the website's do.
const WEBSITE: &str = "https://soundcloud.com";
/// The most a listing hands back in one page.
const PAGE: &str = "200";
/// How many pages a listing follows before it stops, so a runaway `next_href` cannot spin.
const PAGES: usize = 50;
/// How many results a search asks for.
const SEARCH: &str = "50";
/// How many of an artist's tracks fill the top of their page.
const TOP_TRACKS: &str = "20";
/// How many tracks `/tracks?ids=` fills in at once.
const BATCH: usize = 50;
/// How long one call may take.
const TIMEOUT: Duration = Duration::from_secs(20);

/// A SoundCloud session, signed in or a guest. Clones share the client id, so a new one read by
/// one is used by all.
#[derive(Clone)]
pub struct SoundCloudClient {
    inner: Arc<Inner>,
}

struct Inner {
    http: reqwest::Client,
    /// The `oauth_token` cookie, or `None` for a guest.
    token: Option<String>,
    key: tokio::sync::Mutex<Option<String>>,
    /// The listener's own id, learnt from the profile.
    me: OnceLock<String>,
    /// The page of every track, playlist and user that passed through, by kind and id, since
    /// SoundCloud names its pages by permalink and an id alone cannot be turned into one.
    pages: Mutex<HashMap<String, String>>,
}

impl SoundCloudClient {
    pub(crate) fn new(token: Option<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(key::AGENT)
            .build()
            .context("cannot build the soundcloud http client")?;
        Ok(Self {
            inner: Arc::new(Inner {
                http,
                token,
                key: tokio::sync::Mutex::new(key::cached()),
                me: OnceLock::new(),
                pages: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.inner.http
    }

    pub(crate) fn signed_in(&self) -> bool {
        self.inner.token.is_some()
    }

    /// The client id to send, read from the website when there is none yet or `fresh` asks for
    /// a new one because the last was turned away.
    async fn key(&self, fresh: bool) -> Result<String> {
        let mut key = self.inner.key.lock().await;
        match key.as_ref() {
            Some(id) if !fresh => Ok(id.clone()),
            _ => {
                let id = key::fetch(&self.inner.http).await?;
                *key = Some(id.clone());
                Ok(id)
            }
        }
    }

    /// Sends one call with the client id and, when signed in, the token. A call turned away is
    /// sent once more with a client id read afresh, since that is what changes under it. A
    /// refusal for asking too often comes back as [`Throttled`].
    pub(super) async fn send(
        &self,
        method: Method,
        url: Url,
        body: Option<&serde_json::Value>,
    ) -> Result<Response> {
        let mut fresh = false;
        loop {
            let key = self.key(fresh).await?;
            let mut signed = url.clone();
            let kept: Vec<(String, String)> = url
                .query_pairs()
                .filter(|(name, _)| name != "client_id")
                .map(|(name, value)| (name.into_owned(), value.into_owned()))
                .collect();
            signed
                .query_pairs_mut()
                .clear()
                .extend_pairs(kept)
                .append_pair("client_id", &key);
            let mut request = self
                .inner
                .http
                .request(method.clone(), signed)
                .header(reqwest::header::ACCEPT, "application/json; charset=utf-8")
                .header(reqwest::header::ORIGIN, WEBSITE)
                .header(reqwest::header::REFERER, format!("{WEBSITE}/"));
            if let Some(token) = &self.inner.token {
                request = request.header(reqwest::header::AUTHORIZATION, format!("OAuth {token}"));
            }
            if let Some(body) = body {
                request = request.json(body);
            }
            let response = request
                .send()
                .await
                .map_err(reqwest::Error::without_url)
                .with_context(|| format!("cannot reach soundcloud for {}", url.path()))?;
            match response.status() {
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN if !fresh => {
                    log::info!(
                        "soundcloud: {} was turned away, reading the client id again",
                        url.path()
                    );
                    fresh = true;
                }
                StatusCode::TOO_MANY_REQUESTS => return Err(Throttled.into()),
                status if !status.is_success() => {
                    bail!("soundcloud answered {status} for {method} {}", url.path())
                }
                _ => return Ok(response),
            }
        }
    }

    fn url(path: &str, query: &[(&str, &str)]) -> Result<Url> {
        let mut url = Url::parse(&format!("{API}{path}"))?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        Ok(url)
    }

    pub(super) async fn get_url<T: DeserializeOwned>(&self, url: Url) -> Result<T> {
        let path = url.path().to_owned();
        self.send(Method::GET, url, None)
            .await?
            .json()
            .await
            .with_context(|| format!("cannot read soundcloud's answer for {path}"))
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T> {
        self.get_url(Self::url(path, query)?).await
    }

    /// Every item of a listing, following `next_href` page by page.
    async fn all<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<Vec<T>> {
        let mut query = query.to_vec();
        query.extend([("limit", PAGE), ("linked_partitioning", "1")]);
        let mut url = Self::url(path, &query)?;
        let mut items = Vec::new();
        for _ in 0..PAGES {
            let page: Collection<T> = self.get_url(url).await?;
            items.extend(page.collection);
            let Some(next) = page.next_href.and_then(|next| Url::parse(&next).ok()) else {
                break;
            };
            url = next;
        }
        Ok(items)
    }

    /// The first page of a listing, for a search or a rail that needs no more.
    async fn first<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Vec<T>> {
        let page: Collection<T> = self.get(path, query).await?;
        Ok(page.collection)
    }

    /// The number `id` stands for. A number is itself, and a web address a link handed over is
    /// resolved through SoundCloud.
    pub(super) async fn id(&self, id: &str) -> Result<String> {
        if !id.starts_with("https://") {
            return Ok(id.to_owned());
        }
        let resolved: Resolved = self
            .get("/resolve", &[("url", id)])
            .await
            .context("cannot resolve the soundcloud link")?;
        Ok(resolved.id.to_string())
    }

    /// The listener's own id, asked for once. A guest has none.
    async fn me(&self) -> Result<String> {
        if !self.signed_in() {
            bail!("a guest has no soundcloud account");
        }
        if let Some(me) = self.inner.me.get() {
            return Ok(me.clone());
        }
        let me: wire::User = self.get("/me", &[]).await?;
        Ok(self.inner.me.get_or_init(|| me.id.to_string()).clone())
    }

    fn owner(&self) -> Option<&str> {
        self.inner.me.get().map(String::as_str)
    }

    /// Remembers the page of something that passed through, for `share_url`.
    fn page(&self, kind: MediaKind, id: u64, url: Option<&String>) {
        let Some(url) = url else {
            return;
        };
        if let Ok(mut pages) = self.inner.pages.lock() {
            pages.insert(format!("{kind:?}:{id}"), url.clone());
        }
    }

    /// SoundCloud's track as Sonora's, remembering its page on the way.
    fn track_of(&self, track: wire::Track) -> Track {
        self.page(MediaKind::Track, track.id, track.permalink_url.as_ref());
        track.into_track()
    }

    fn tracks_of(&self, tracks: Vec<wire::Track>) -> Vec<Track> {
        tracks
            .into_iter()
            .map(|track| self.track_of(track))
            .collect()
    }

    fn playlist_of(&self, playlist: &wire::Playlist) -> Playlist {
        self.page(
            MediaKind::Playlist,
            playlist.id,
            playlist.permalink_url.as_ref(),
        );
        playlist.to_playlist(self.owner())
    }

    fn album_of(&self, playlist: &wire::Playlist) -> Album {
        self.page(
            MediaKind::Album,
            playlist.id,
            playlist.permalink_url.as_ref(),
        );
        playlist.to_album()
    }

    /// The track's own record, read for its formats when it plays.
    pub(super) async fn record(&self, id: &str) -> Result<wire::Track> {
        let id = self.id(id).await?;
        self.get(&format!("/tracks/{id}"), &[]).await
    }

    /// A playlist with every track filled in. SoundCloud lists only the first few of a long one
    /// in full and the rest as bare ids, which are asked for a batch at a time.
    async fn full_playlist(&self, id: &str) -> Result<wire::Playlist> {
        let id = self.id(id).await?;
        let mut playlist: wire::Playlist = self
            .get(&format!("/playlists/{id}"), &[("representation", "full")])
            .await?;
        let missing: Vec<String> = playlist
            .tracks
            .iter()
            .filter(|track| track.stub())
            .map(|track| track.id.to_string())
            .collect();
        let mut found: HashMap<u64, wire::Track> = HashMap::new();
        for batch in missing.chunks(BATCH) {
            let ids = batch.join(",");
            let tracks: Vec<wire::Track> = self.get("/tracks", &[("ids", &ids)]).await?;
            found.extend(tracks.into_iter().map(|track| (track.id, track)));
        }
        for track in &mut playlist.tracks {
            if track.stub()
                && let Some(full) = found.remove(&track.id)
            {
                *track = full;
            }
        }
        playlist.tracks.retain(|track| !track.stub());
        Ok(playlist)
    }

    /// Writes a playlist's whole track list, which is how SoundCloud changes one.
    async fn write_tracks(&self, id: &str, ids: &[u64]) -> Result<()> {
        let id = self.id(id).await?;
        let body = json!({ "playlist": { "tracks": ids } });
        self.send(
            Method::PUT,
            Self::url(&format!("/playlists/{id}"), &[])?,
            Some(&body),
        )
        .await?;
        Ok(())
    }

    async fn update_playlist(&self, id: &str, fields: serde_json::Value) -> Result<()> {
        let id = self.id(id).await?;
        let body = json!({ "playlist": fields });
        self.send(
            Method::PUT,
            Self::url(&format!("/playlists/{id}"), &[])?,
            Some(&body),
        )
        .await?;
        Ok(())
    }

    /// Likes or unlikes a track or a playlist, which is all SoundCloud's library is.
    async fn like(&self, kind: &str, id: &str, liked: bool) -> Result<()> {
        let me = self.me().await?;
        let id = self.id(id).await?;
        let method = match liked {
            true => Method::PUT,
            false => Method::DELETE,
        };
        self.send(
            method,
            Self::url(&format!("/users/{me}/{kind}_likes/{id}"), &[])?,
            None,
        )
        .await?;
        Ok(())
    }

    /// Everything the listener liked, tracks and playlists together, newest first.
    async fn likes(&self) -> Result<Vec<Like>> {
        if !self.signed_in() {
            return Ok(Vec::new());
        }
        let me = self.me().await?;
        self.all(&format!("/users/{me}/likes"), &[]).await
    }

    async fn liked_playlists(&self) -> Result<Vec<wire::Playlist>> {
        Ok(self
            .likes()
            .await?
            .into_iter()
            .filter_map(|like| like.playlist)
            .collect())
    }

    async fn user_record(&self, id: &str) -> Result<wire::User> {
        self.get(&format!("/users/{id}"), &[]).await
    }

    async fn top_tracks(&self, id: &str) -> Result<Vec<Track>> {
        let path = format!("/users/{id}/toptracks");
        let top: Vec<wire::Track> = match self.first(&path, &[("limit", TOP_TRACKS)]).await {
            Ok(top) if !top.is_empty() => top,
            _ => {
                self.first(&format!("/users/{id}/tracks"), &[("limit", TOP_TRACKS)])
                    .await?
            }
        };
        Ok(self.tracks_of(top))
    }
}

#[async_trait]
impl MusicApi for SoundCloudClient {
    fn cover_at(&self, cover: &str, size: CoverSize) -> Option<String> {
        match size {
            CoverSize::Large => Some(cover.to_owned()),
            CoverSize::Full => Some(wire::full(cover)),
        }
    }

    fn share_url(&self, kind: MediaKind, id: &str) -> Option<String> {
        if id.starts_with("https://") {
            return Some(id.to_owned());
        }
        let pages = self.inner.pages.lock().ok()?;
        pages.get(&format!("{kind:?}:{id}")).cloned()
    }

    async fn profile(&self) -> Result<UserProfile> {
        let me: wire::User = self.get("/me", &[]).await?;
        self.inner.me.get_or_init(|| me.id.to_string());
        Ok(me.into_profile())
    }

    async fn user(&self, user_id: &str) -> Result<UserDetail> {
        let id = self.id(user_id).await?;
        let path = format!("/users/{id}/playlists_without_albums");
        let (user, playlists) = tokio::try_join!(
            self.user_record(&id),
            self.all::<wire::Playlist>(&path, &[]),
        )?;
        Ok(UserDetail {
            name: user.name(),
            avatar: wire::large(user.avatar_url.as_deref()),
            followers: user.followers_count,
            following: user.followings_count,
            playlists: playlists.iter().map(|p| self.playlist_of(p)).collect(),
            id: user.id.to_string(),
        })
    }

    async fn artist(&self, artist_id: &str) -> Result<Artist> {
        let id = self.id(artist_id).await?;
        let path = format!("/users/{id}/albums");
        let (user, top_tracks, albums) = tokio::try_join!(
            self.user_record(&id),
            self.top_tracks(&id),
            self.all::<wire::Playlist>(&path, &[]),
        )?;
        Ok(Artist {
            name: user.name(),
            cover_large: wire::large(user.avatar_url.as_deref()),
            banner: None,
            biography: user.description.filter(|text| !text.trim().is_empty()),
            monthly_listeners: user.followers_count,
            top_tracks,
            albums: albums.iter().map(|album| self.album_of(album)).collect(),
        })
    }

    async fn artist_profile(&self, artist_id: &str) -> Result<ArtistProfile> {
        let id = self.id(artist_id).await?;
        let user = self.user_record(&id).await?;
        Ok(ArtistProfile {
            name: user.name(),
            cover_large: wire::large(user.avatar_url.as_deref()),
            biography: user.description.filter(|text| !text.trim().is_empty()),
        })
    }

    async fn artist_images(&self, ids: Vec<String>) -> Result<HashMap<String, String>> {
        let mut images = HashMap::new();
        for id in ids {
            let Ok(number) = self.id(&id).await else {
                continue;
            };
            let user = self.user_record(&number).await;
            if let Some(avatar) = user
                .ok()
                .and_then(|user| wire::large(user.avatar_url.as_deref()))
            {
                images.insert(id, avatar);
            }
        }
        Ok(images)
    }

    async fn saved_tracks(&self) -> Result<Vec<Track>> {
        let tracks = self
            .likes()
            .await?
            .into_iter()
            .filter_map(|like| like.track)
            .collect();
        Ok(self.tracks_of(tracks))
    }

    async fn set_track_saved(&self, track_id: &str, saved: bool) -> Result<()> {
        self.like("track", track_id, saved).await
    }

    async fn track(&self, track_id: &str) -> Result<Track> {
        Ok(self.track_of(self.record(track_id).await?))
    }

    async fn track_playcount(&self, track_id: &str) -> Result<Option<u64>> {
        Ok(self.record(track_id).await?.playback_count)
    }

    async fn recently_played(&self) -> Result<Vec<Track>> {
        if !self.signed_in() {
            return Ok(Vec::new());
        }
        let played: Vec<Played> = self
            .first("/me/play-history/tracks", &[("limit", PAGE)])
            .await?;
        Ok(self.tracks_of(played.into_iter().filter_map(|entry| entry.track).collect()))
    }

    /// The listener's own playlists, then the ones they liked, leaving out the releases, which
    /// the albums page lists.
    async fn playlists(&self) -> Result<Vec<Playlist>> {
        if !self.signed_in() {
            return Ok(Vec::new());
        }
        let me = self.me().await?;
        let path = format!("/users/{me}/playlists_without_albums");
        let (own, liked) = tokio::try_join!(
            self.all::<wire::Playlist>(&path, &[]),
            self.liked_playlists(),
        )?;
        let mut playlists: Vec<Playlist> = Vec::new();
        for playlist in own
            .iter()
            .chain(liked.iter().filter(|playlist| playlist.release().is_none()))
        {
            let id = playlist.id.to_string();
            if playlists.iter().all(|kept| kept.id != id) {
                playlists.push(self.playlist_of(playlist));
            }
        }
        Ok(playlists)
    }

    async fn create_playlist(&self, name: &str) -> Result<String> {
        let body = json!({ "playlist": { "title": name, "sharing": "private", "tracks": [] } });
        let created: Resolved = self
            .send(Method::POST, Self::url("/playlists", &[])?, Some(&body))
            .await?
            .json()
            .await
            .context("cannot read the playlist soundcloud made")?;
        Ok(created.id.to_string())
    }

    async fn rename_playlist(&self, playlist_id: &str, name: &str) -> Result<()> {
        self.update_playlist(playlist_id, json!({ "title": name }))
            .await
    }

    async fn delete_playlist(&self, playlist_id: &str) -> Result<()> {
        let id = self.id(playlist_id).await?;
        self.send(
            Method::DELETE,
            Self::url(&format!("/playlists/{id}"), &[])?,
            None,
        )
        .await?;
        Ok(())
    }

    async fn remove_playlist_from_library(&self, playlist_id: &str) -> Result<()> {
        self.like("playlist", playlist_id, false).await
    }

    async fn add_playlist_to_library(&self, playlist_id: &str) -> Result<()> {
        self.like("playlist", playlist_id, true).await
    }

    async fn set_playlist_public(&self, playlist_id: &str, public: bool) -> Result<()> {
        let sharing = match public {
            true => "public",
            false => "private",
        };
        self.update_playlist(playlist_id, json!({ "sharing": sharing }))
            .await
    }

    async fn add_track_to_playlist(&self, playlist_id: &str, track_id: &str) -> Result<()> {
        self.add_tracks_to_playlist(playlist_id, &[track_id.to_owned()])
            .await
    }

    async fn add_tracks_to_playlist(&self, playlist_id: &str, track_ids: &[String]) -> Result<()> {
        let mut ids: Vec<u64> = self
            .full_playlist(playlist_id)
            .await?
            .tracks
            .iter()
            .map(|track| track.id)
            .collect();
        for id in track_ids {
            ids.push(
                self.id(id)
                    .await?
                    .parse()
                    .context("a soundcloud track id is not a number")?,
            );
        }
        self.write_tracks(playlist_id, &ids).await
    }

    async fn remove_track_from_playlist(&self, playlist_id: &str, track_id: &str) -> Result<()> {
        let gone: u64 = self
            .id(track_id)
            .await?
            .parse()
            .context("a soundcloud track id is not a number")?;
        let ids: Vec<u64> = self
            .full_playlist(playlist_id)
            .await?
            .tracks
            .iter()
            .map(|track| track.id)
            .filter(|id| *id != gone)
            .collect();
        self.write_tracks(playlist_id, &ids).await
    }

    /// The releases the listener liked, since SoundCloud keeps no albums apart from playlists.
    async fn saved_albums(&self) -> Result<Vec<Album>> {
        Ok(self
            .liked_playlists()
            .await?
            .iter()
            .filter(|playlist| playlist.release().is_some())
            .map(|playlist| self.album_of(playlist))
            .collect())
    }

    async fn set_album_saved(&self, album_id: &str, saved: bool) -> Result<()> {
        self.like("playlist", album_id, saved).await
    }

    async fn saved_artists(&self) -> Result<Vec<SavedArtist>> {
        if !self.signed_in() {
            return Ok(Vec::new());
        }
        let me = self.me().await?;
        let users: Vec<wire::User> = self.all(&format!("/users/{me}/followings"), &[]).await?;
        Ok(users.into_iter().map(wire::User::into_artist).collect())
    }

    async fn set_artist_saved(&self, artist_id: &str, saved: bool) -> Result<()> {
        if !self.signed_in() {
            bail!("a guest cannot follow anyone on soundcloud");
        }
        let id = self.id(artist_id).await?;
        let method = match saved {
            true => Method::POST,
            false => Method::DELETE,
        };
        self.send(
            method,
            Self::url(&format!("/me/followings/{id}"), &[])?,
            None,
        )
        .await?;
        Ok(())
    }

    async fn album(&self, album_id: &str) -> Result<AlbumDetail> {
        let record = self.full_playlist(album_id).await?;
        let album = self.album_of(&record);
        let tracks = record
            .tracks
            .into_iter()
            .enumerate()
            .map(|(at, track)| Track {
                track_number: at as u32 + 1,
                album: album.name.clone(),
                album_id: Some(album.id.clone()),
                ..self.track_of(track)
            })
            .collect();
        Ok(AlbumDetail { album, tracks })
    }

    async fn album_tracks(&self, album_id: &str) -> Result<Vec<Track>> {
        Ok(self.album(album_id).await?.tracks)
    }

    async fn playlist(&self, playlist_id: &str) -> Result<PlaylistDetail> {
        let record = self.full_playlist(playlist_id).await?;
        let playlist = self.playlist_of(&record);
        Ok(PlaylistDetail {
            playlist,
            tracks: self.tracks_of(record.tracks),
            continuation: None,
        })
    }

    async fn playlist_tracks(&self, playlist_id: &str) -> Result<Vec<Track>> {
        let record = self.full_playlist(playlist_id).await?;
        Ok(self.tracks_of(record.tracks))
    }

    async fn playlist_covers(&self, playlist_id: &str, wanted: usize) -> Result<Vec<String>> {
        let tracks = self.playlist_tracks(playlist_id).await?;
        Ok(distinct_covers(&tracks, wanted))
    }

    /// The tracks SoundCloud lists as related, which is what it plays after one ends.
    async fn track_radio(
        &self,
        track_id: &str,
        _from: Option<&str>,
    ) -> Result<(Vec<Track>, Option<String>)> {
        let id = self.id(track_id).await?;
        let related: Vec<wire::Track> = self
            .first(&format!("/tracks/{id}/related"), &[("limit", SEARCH)])
            .await?;
        let playable = related.into_iter().filter(wire::Track::playable).collect();
        Ok((self.tracks_of(playable), None))
    }

    async fn search(&self, query: &str) -> Result<Vec<Track>> {
        let tracks: Vec<wire::Track> = self
            .first("/search/tracks", &[("q", query), ("limit", SEARCH)])
            .await?;
        Ok(self.tracks_of(tracks))
    }

    async fn search_albums(&self, query: &str) -> Result<Vec<Album>> {
        let albums: Vec<wire::Playlist> = self
            .first("/search/albums", &[("q", query), ("limit", SEARCH)])
            .await?;
        Ok(albums.iter().map(|album| self.album_of(album)).collect())
    }

    async fn search_playlists(&self, query: &str) -> Result<Vec<Playlist>> {
        let playlists: Vec<wire::Playlist> = self
            .first(
                "/search/playlists_without_albums",
                &[("q", query), ("limit", SEARCH)],
            )
            .await?;
        Ok(playlists.iter().map(|p| self.playlist_of(p)).collect())
    }
}

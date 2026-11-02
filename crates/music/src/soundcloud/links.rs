//! SoundCloud's links: soundcloud.com pages and the on.soundcloud.com short links its share
//! sheet hands out. A page is named by permalink rather than by id, so the id handed back is the
//! page's own address, which the client resolves into an urn when it first uses it.

use crate::link::{self, Linked};

const HOST: &str = "soundcloud.com";
const SHORT_HOST: &str = "on.soundcloud.com";
/// The pages under a user that list something of theirs rather than being a track.
const LISTINGS: [&str; 12] = [
    "albums",
    "comments",
    "followers",
    "following",
    "likes",
    "popular-tracks",
    "reposts",
    "sets",
    "spotlight",
    "stations",
    "tracks",
    "you",
];

/// What a SoundCloud link points at.
pub(super) fn read(text: &str) -> Option<Linked> {
    let url = link::address(text)?;
    if !matches!(url.scheme(), "https" | "http") {
        return None;
    }
    let host = link::host(&url)?;
    if host == SHORT_HOST {
        return Some(Linked::Short);
    }
    if host != HOST {
        return None;
    }
    let segments = link::segments(&url);
    let page = |parts: &[&str]| format!("https://{HOST}/{}", parts.join("/"));
    match segments.as_slice() {
        [user] if !LISTINGS.contains(user) => Some(Linked::Artist(page(&[user]))),
        [user, "sets", set, ..] => Some(Linked::Playlist(page(&[user, "sets", set]))),
        [user, track, ..] if !LISTINGS.contains(track) => Some(Linked::Track(page(&[user, track]))),
        [user, ..] => Some(Linked::Artist(page(&[user]))),
        [] => None,
    }
}

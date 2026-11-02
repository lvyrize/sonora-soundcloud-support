//! The client id SoundCloud's website calls its own api with. Every request needs one, and
//! SoundCloud changes it now and then, so it is read out of the website's scripts, kept beside
//! the sign-in, and read again once the api stops taking it.

use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};

use crate::credentials;

const HOME: &str = "https://soundcloud.com/";
/// Where the website's scripts live. The id sits in one of the last few.
const ASSETS: &str = "https://a-v2.sndcdn.com/assets/";
/// What precedes the id in the scripts, in the two shapes it has been written in.
const MARKERS: [&str; 2] = ["client_id:\"", "client_id="];
/// How long an id is.
const LENGTH: usize = 32;
/// What a browser says about itself, since the home page answers a bare client with less.
pub(super) const AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0";

fn path() -> PathBuf {
    credentials::dir("soundcloud").join("client-id")
}

/// The id read last time, when there is one.
pub(super) fn cached() -> Option<String> {
    let id = std::fs::read_to_string(path()).ok()?;
    let id = id.trim();
    valid(id).then(|| id.to_owned())
}

/// Reads the id out of the website's scripts, newest first, and keeps it for next time.
pub(super) async fn fetch(http: &reqwest::Client) -> Result<String> {
    let page = http
        .get(HOME)
        .header(reqwest::header::USER_AGENT, AGENT)
        .send()
        .await
        .context("cannot reach soundcloud.com")?
        .text()
        .await
        .context("cannot read soundcloud.com")?;
    let scripts: Vec<&str> = page
        .match_indices(ASSETS)
        .filter_map(|(at, _)| {
            let rest = &page[at..];
            let end = rest.find('"')?;
            Some(&rest[..end]).filter(|script| script.ends_with(".js"))
        })
        .collect();
    for script in scripts.into_iter().rev() {
        let Ok(response) = http
            .get(script)
            .header(reqwest::header::USER_AGENT, AGENT)
            .send()
            .await
        else {
            continue;
        };
        let Ok(body) = response.text().await else {
            continue;
        };
        if let Some(id) = find(&body) {
            if let Err(error) = credentials::write(&path(), id.as_bytes()) {
                log::warn!("soundcloud: cannot keep the client id: {error:#}");
            }
            log::info!("soundcloud: read a new client id from the website");
            return Ok(id);
        }
    }
    bail!("none of soundcloud.com's scripts carries a client id")
}

/// The first id written after one of `MARKERS` in a script.
fn find(script: &str) -> Option<String> {
    MARKERS.iter().find_map(|marker| {
        script.match_indices(marker).find_map(|(at, _)| {
            let id = script.get(at + marker.len()..at + marker.len() + LENGTH)?;
            valid(id).then(|| id.to_owned())
        })
    })
}

fn valid(id: &str) -> bool {
    id.len() == LENGTH && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

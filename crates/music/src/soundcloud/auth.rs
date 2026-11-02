//! Sign-in for SoundCloud: either a guest, who can search and play but has no library, or the
//! browser's `oauth_token` cookie, which the website itself sends as its bearer token. Which
//! one it is stays in the provider's own accounts folder.

use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

use crate::credentials;

/// What the last sign-in left behind.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub(crate) enum Saved {
    Guest,
    Account { oauth_token: String },
}

fn path() -> PathBuf {
    credentials::dir("soundcloud").join(credentials::FILE)
}

/// Extracts the `oauth_token` value from what the user pastes: a full `Cookie` header, a lone
/// `oauth_token=…` pair, or the bare token, with the quotes devtools may show around it.
/// Refuses anything without one.
pub fn token(input: &str) -> Result<String> {
    let trimmed = unquote(input);
    for pair in trimmed.split(';') {
        if let Some((name, value)) = pair.trim().split_once('=')
            && name.trim() == "oauth_token"
        {
            let value = unquote(value);
            if valid(value) {
                return Ok(value.to_owned());
            }
            bail!("the oauth_token cookie does not look like one");
        }
    }
    if valid(trimmed) {
        return Ok(trimmed.to_owned());
    }
    bail!("the cookies carry no oauth_token; sign in to soundcloud.com first");
}

fn unquote(value: &str) -> &str {
    value.trim().trim_matches('"').trim()
}

/// A token is either the older dash-separated kind, such as `2-123456-123456789-AbCd`, or a
/// JWT, whose three base64url parts are joined by dots.
fn valid(value: &str) -> bool {
    value.len() >= 16
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

pub(crate) fn load() -> Option<Saved> {
    let bytes = std::fs::read(path()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub(crate) fn store(saved: &Saved) -> Result<()> {
    let bytes =
        serde_json::to_vec_pretty(saved).context("cannot serialize the soundcloud sign-in")?;
    credentials::write(&path(), &bytes).context("cannot store the soundcloud sign-in")
}

pub(crate) fn forget() {
    credentials::remove(&path());
}

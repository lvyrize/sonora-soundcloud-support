//! SoundCloud through the web api soundcloud.com itself calls, with the client id read out of
//! the website's scripts. A guest can search and play; signing in adds likes, playlists and
//! follows. The sign-in opens in the listener's own browser, where they may already be signed
//! in, and they paste its `oauth_token` cookie back. Playback streams whatever full
//! format SoundCloud serves through the shared engine.

mod auth;
mod client;
mod key;
mod links;
mod playback;
mod wire;

use std::sync::Arc;

use anyhow::{Context as _, Result};
use async_trait::async_trait;

use crate::soundcloud::auth::Saved;
use crate::soundcloud::playback::Factory;
use crate::{
    Capabilities, InputSource, MusicApi as _, MusicProvider, PromptSink, ProviderSession, Shape,
    SignIn, SignInPrompt, UserProfile,
};

pub use auth::token;
pub use client::SoundCloudClient;

/// The page the sign-in opens in the default browser.
const SIGN_IN_URL: &str = "https://soundcloud.com/signin";
/// The id a guest session's profile carries.
const GUEST_ID: &str = "soundcloud-guest";

pub struct SoundCloudProvider;

impl SoundCloudProvider {
    pub fn new() -> Self {
        Self
    }

    async fn account(oauth_token: String) -> Result<ProviderSession> {
        let client = SoundCloudClient::new(Some(oauth_token))?;
        let profile = client
            .profile()
            .await
            .context("cannot read the soundcloud account")?;
        Ok(session(client, profile, true))
    }

    fn guest() -> Result<ProviderSession> {
        let profile = UserProfile {
            id: GUEST_ID.to_owned(),
            display_name: "SoundCloud".to_owned(),
            avatar: None,
        };
        Ok(session(SoundCloudClient::new(None)?, profile, false))
    }
}

impl Default for SoundCloudProvider {
    fn default() -> Self {
        Self::new()
    }
}

fn session(client: SoundCloudClient, profile: UserProfile, signed_in: bool) -> ProviderSession {
    ProviderSession {
        profile,
        api: Arc::new(client.clone()),
        playback: Arc::new(Factory::new(client)),
        shape: Shape::Saved,
        authenticated: signed_in,
        capabilities: Capabilities {
            follow_artists: signed_in,
            keeps: false,
            downloads: false,
            ..Capabilities::ALL
        },
    }
}

#[async_trait]
impl MusicProvider for SoundCloudProvider {
    fn name(&self) -> &'static str {
        "SoundCloud"
    }

    fn slug(&self) -> &'static str {
        "soundcloud"
    }

    fn link(&self, link: &str) -> Option<crate::link::Linked> {
        links::read(link)
    }

    fn reach(&self) -> Option<String> {
        Some("api-v2.soundcloud.com".to_owned())
    }

    fn sign_in_options(&self) -> Vec<SignIn> {
        vec![SignIn::Anonymous, SignIn::Secret]
    }

    fn stored(&self) -> bool {
        auth::load().is_some()
    }

    fn stored_guest(&self) -> bool {
        matches!(auth::load(), Some(Saved::Guest))
    }

    fn public_art(&self) -> bool {
        // artwork lives on i1.sndcdn.com, open to anyone with the url
        true
    }

    async fn restore(&self) -> Result<Option<ProviderSession>> {
        match auth::load() {
            None => Ok(None),
            Some(Saved::Guest) => Self::guest().map(Some),
            Some(Saved::Account { oauth_token }) => match Self::account(oauth_token).await {
                Ok(session) => Ok(Some(session)),
                Err(error) if crate::trouble::offline(&format!("{error:#}")) => Err(error),
                Err(error) => {
                    log::warn!("soundcloud: the stored session is no longer usable: {error:#}");
                    Ok(None)
                }
            },
        }
    }

    async fn sign_in(
        &self,
        method: SignIn,
        prompt: PromptSink,
        mut input: InputSource,
    ) -> Result<ProviderSession> {
        match method {
            SignIn::Anonymous => {
                auth::store(&Saved::Guest)?;
                Self::guest()
            }
            SignIn::Secret | SignIn::Default => {
                open::that_in_background(SIGN_IN_URL);
                prompt(SignInPrompt::Secret);
                let cookies = input.recv().await.context("sign-in was cancelled")?;
                let oauth_token = auth::token(&cookies)
                    .inspect_err(|error| log::warn!("soundcloud: sign-in refused: {error:#}"))?;
                let session = Self::account(oauth_token.clone())
                    .await
                    .inspect_err(|error| log::warn!("soundcloud: sign-in failed: {error:#}"))?;
                auth::store(&Saved::Account { oauth_token })?;
                Ok(session)
            }
            SignIn::Path(_) | SignIn::Credentials { .. } => Err(anyhow::anyhow!(
                "soundcloud signs in as a guest or with the browser"
            )),
        }
    }

    fn sign_out(&self) {
        auth::forget();
    }
}

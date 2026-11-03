//! Spotify OAuth (Authorization Code with PKCE). No client secret is involved:
//! each user brings their own Client ID. The refresh token lives in the Windows
//! Credential Manager, never on disk in plain text.

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const AUTH_URL: &str = "https://accounts.spotify.com/authorize";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const KEYRING_SERVICE: &str = "Lume";
/// Where versions before the rename kept the login.
const OLD_KEYRING_SERVICE: &str = "SpotifyApp";
const KEYRING_USER: &str = "refresh_token";

/// Renew the access token this long before it actually expires.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

pub const SCOPES: &str = "streaming user-read-email user-read-private \
    user-read-playback-state user-modify-playback-state user-read-currently-playing \
    user-library-read user-library-modify user-read-recently-played user-top-read \
    playlist-read-private playlist-read-collaborative playlist-modify-public playlist-modify-private";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("not logged in")]
    NotLoggedIn,
    #[error("network error: {0}")]
    Network(String),
    #[error("Spotify rejected the request ({status}): {body}")]
    Rejected { status: u16, body: String },
    #[error("credential store error: {0}")]
    Keyring(String),
    #[error("invalid login callback: {0}")]
    BadCallback(&'static str),
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
    refresh_token: Option<String>,
}

struct Pending {
    verifier: String,
    state: String,
}

#[derive(Default)]
struct Inner {
    access: Option<(String, Instant)>,
    pending: Option<Pending>,
}

pub struct Auth {
    client_id: Mutex<String>,
    redirect_uri: String,
    agent: ureq::Agent,
    inner: Mutex<Inner>,
}

impl Auth {
    pub fn new(client_id: impl Into<String>, redirect_uri: impl Into<String>) -> Self {
        let agent = crate::http::agent(Duration::from_secs(15));
        Self { client_id: Mutex::new(client_id.into()), redirect_uri: redirect_uri.into(), agent, inner: Mutex::default() }

    }

    /// Spotify Client IDs are 32 hexadecimal characters.
    pub fn is_valid_client_id(id: &str) -> bool {
        id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
    }

    /// Uses another Spotify app from now on. Logins belong to one app: log out first.
    pub fn set_client_id(&self, id: &str) {
        *self.client_id.lock().unwrap() = id.to_string();
    }

    fn client_id(&self) -> String {
        self.client_id.lock().unwrap().clone()
    }

    pub fn has_saved_login(&self) -> bool {
        load_refresh_token().is_some()
    }

    /// Starts a login and returns the URL to open in the user's browser.
    pub fn begin_login(&self) -> String {
        let verifier = random_urlsafe(64);
        let state = random_urlsafe(16);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let url = format!(
            "{AUTH_URL}?client_id={}&response_type=code&redirect_uri={}&scope={}&state={}&code_challenge_method=S256&code_challenge={}",
            percent_encode(&self.client_id()),
            percent_encode(&self.redirect_uri),
            percent_encode(SCOPES),
            percent_encode(&state),
            percent_encode(&challenge),
        );
        self.inner.lock().unwrap().pending = Some(Pending { verifier, state });
        url
    }

    /// Finishes a login with the `code` and `state` from the redirect.
    pub fn complete_login(&self, code: &str, state: &str) -> Result<(), AuthError> {
        let mut inner = self.inner.lock().unwrap();
        let pending = inner.pending.take().ok_or(AuthError::BadCallback("no login in progress"))?;
        if pending.state != state {
            return Err(AuthError::BadCallback("state mismatch"));
        }
        let client_id = self.client_id();
        let token = self.token_request(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &self.redirect_uri),
            ("client_id", &client_id),
            ("code_verifier", &pending.verifier),
        ])?;
        self.store(&mut inner, token)
    }

    /// Returns a valid access token, refreshing it when needed.
    pub fn access_token(&self) -> Result<String, AuthError> {
        let mut inner = self.inner.lock().unwrap();
        if let Some((token, expires)) = &inner.access {
            if Instant::now() + EXPIRY_MARGIN < *expires {
                return Ok(token.clone());
            }
        }
        let refresh = load_refresh_token().ok_or(AuthError::NotLoggedIn)?;
        let client_id = self.client_id();
        let token = match self.token_request(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh),
            ("client_id", &client_id),
        ]) {
            Ok(t) => t,
            Err(AuthError::Rejected { body, .. }) if body.contains("invalid_grant") => {
                delete_refresh_token();
                inner.access = None;
                return Err(AuthError::NotLoggedIn);
            }
            Err(e) => return Err(e),
        };
        self.store(&mut inner, token)?;
        Ok(inner.access.as_ref().map(|(t, _)| t.clone()).expect("just stored"))
    }

    pub fn logout(&self) {
        delete_refresh_token();
        let mut inner = self.inner.lock().unwrap();
        inner.access = None;
        inner.pending = None;
    }

    fn store(&self, inner: &mut Inner, token: TokenResponse) -> Result<(), AuthError> {
        if let Some(refresh) = &token.refresh_token {
            save_refresh_token(refresh)?;
        }
        inner.access = Some((token.access_token, Instant::now() + Duration::from_secs(token.expires_in)));
        Ok(())
    }

    fn token_request(&self, form: &[(&str, &str)]) -> Result<TokenResponse, AuthError> {
        let mut resp = self
            .agent
            .post(TOKEN_URL)
            .send_form(form.iter().copied())
            .map_err(|e| AuthError::Network(e.to_string()))?;
        let status = resp.status().as_u16();
        if status != 200 {
            let body = resp.body_mut().read_to_string().unwrap_or_default();
            return Err(AuthError::Rejected { status, body });
        }
        resp.body_mut().read_json::<TokenResponse>().map_err(|e| AuthError::Network(e.to_string()))
    }
}

fn entry() -> Result<keyring::Entry, AuthError> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).map_err(|e| AuthError::Keyring(e.to_string()))
}

fn old_entry() -> Option<keyring::Entry> {
    keyring::Entry::new(OLD_KEYRING_SERVICE, KEYRING_USER).ok()
}

/// The saved login; one kept under the old name moves to the new one.
fn load_refresh_token() -> Option<String> {
    if let Ok(token) = entry().ok()?.get_password() {
        return Some(token);
    }
    let old = old_entry()?;
    let token = old.get_password().ok()?;
    if save_refresh_token(&token).is_ok() {
        let _ = old.delete_credential();
    }
    Some(token)
}

fn save_refresh_token(token: &str) -> Result<(), AuthError> {
    entry()?.set_password(token).map_err(|e| AuthError::Keyring(e.to_string()))
}

fn delete_refresh_token() {
    if let Ok(e) = entry() {
        let _ = e.delete_credential();
    }
    if let Some(e) = old_entry() {
        let _ = e.delete_credential();
    }
}

/// `len` characters of URL-safe randomness (not for cryptographic keys, only PKCE/state values
/// that are single-use; they come from the OS-seeded thread RNG).
fn random_urlsafe(len: usize) -> String {
    let mut bytes = Vec::with_capacity(len + 16);
    while bytes.len() < len {
        bytes.extend_from_slice(&rand::random::<u128>().to_le_bytes());
    }
    let mut s = URL_SAFE_NO_PAD.encode(&bytes);
    s.truncate(len);
    s
}

pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_values_have_valid_length() {
        let v = random_urlsafe(64);
        assert_eq!(v.len(), 64);
        assert!(v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    }

    #[test]
    fn login_url_contains_pkce_parameters() {
        let auth = Auth::new("abc123", "http://127.0.0.1:8898/");
        let url = auth.begin_login();
        assert!(url.contains("client_id=abc123"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A8898%2F"));
        assert!(matches!(auth.complete_login("x", "wrong-state"), Err(AuthError::BadCallback(_))));
    }

    #[test]
    fn client_id_format() {
        assert!(Auth::is_valid_client_id("0123456789abcdef0123456789ABCDEF"));
        assert!(!Auth::is_valid_client_id("too-short"));
        assert!(!Auth::is_valid_client_id("g123456789abcdef0123456789abcdef"));
        assert!(!Auth::is_valid_client_id(""));
    }

    #[test]
    fn percent_encoding() {
        assert_eq!(percent_encode("a b/c"), "a%20b%2Fc");
    }
}

//! Signing in to a hosted MCP server.
//!
//! A server on this machine wants no credential, and one behind a company's
//! gateway usually wants a static token in a header -- both already worked. The
//! third kind, and the one every hosted MCP service is, wants OAuth: you sign
//! in at their site, they redirect back, and IRA holds a token that refreshes
//! itself.
//!
//! **rmcp does the protocol.** Discovery, dynamic client registration, PKCE,
//! the code exchange and refresh are all `AuthorizationManager`. What is here
//! is the three things it cannot know: where to redirect to, where to keep the
//! tokens, and how to hold a half-finished sign-in across two HTTP requests.
//!
//! ```text
//!   window: Sign in    ──►  begin()   ──►  the provider's URL, opened in a browser
//!   provider redirects ──►  GET /oauth/callback?code=…&state=…
//!                           finish()  ──►  tokens into the keyring, server connects
//! ```
//!
//! **The redirect is IRA's own port**, which is the only reason this works
//! without a public URL: she already serves on loopback, so
//! `http://127.0.0.1:8180/oauth/callback` is a real address the provider can
//! send a browser back to.
//!
//! **Tokens go to the keyring**, not the database, for exactly the reason
//! [0015](../docs/decisions/0015-settings-come-from-the-keyring-not-the-environment.md)
//! put the API keys there. `ira.local.db` holds the client id and the scopes,
//! which are not secrets.

use anyhow::{anyhow, Context, Result};
use rmcp::transport::auth::{
    AuthClient, AuthError, AuthorizationManager, CredentialStore, StoredCredentials,
};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// How long a half-finished sign-in is held before it is forgotten. Long enough
/// to find your password, short enough that an abandoned attempt does not sit
/// in memory for the life of the process.
const PENDING_FOR: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Sign-ins that have been started and not yet come back.
///
/// Keyed by the CSRF state the provider will hand back, so a callback can find
/// the manager holding its PKCE verifier. Two sign-ins at once are fine, which
/// is not hypothetical: the obvious way to configure three servers is to start
/// all three and work through the tabs.
fn pending() -> &'static Mutex<HashMap<String, Pending>> {
    static PENDING: OnceLock<Mutex<HashMap<String, Pending>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

struct Pending {
    server: String,
    manager: AuthorizationManager,
    started: std::time::Instant,
}

/// The keyring, as somewhere rmcp will keep tokens.
///
/// One entry per server, holding the whole `StoredCredentials` as JSON --
/// access token, refresh token, granted scopes. rmcp writes through this on
/// every refresh, so a token that rotates is persisted without IRA doing
/// anything.
struct Keyring {
    server: String,
}

impl Keyring {
    fn key(&self) -> String {
        crate::settings::secret::oauth_key(&self.server)
    }
}

#[async_trait::async_trait]
impl CredentialStore for Keyring {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        // A keyring failure is reported as "not signed in" rather than as an
        // error: the recovery is the same -- sign in again -- and a headless
        // box with no Secret Service must not make the server unusable in a way
        // that reads as a protocol fault.
        match crate::settings::secret::read(&self.key()) {
            Ok(Some(json)) => match serde_json::from_str(&json) {
                Ok(c) => Ok(Some(c)),
                Err(e) => {
                    tracing::warn!(server = %self.server, "stored sign-in is unreadable: {e}");
                    Ok(None)
                }
            },
            Ok(None) => Ok(None),
            Err(e) => {
                tracing::warn!(server = %self.server, "could not read the keyring: {e:#}");
                Ok(None)
            }
        }
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let json = serde_json::to_string(&credentials)
            .map_err(|e| AuthError::InternalError(e.to_string()))?;
        crate::settings::secret::write(&self.key(), &json)
            .map_err(|e| AuthError::InternalError(format!("{e:#}")))
    }

    async fn clear(&self) -> Result<(), AuthError> {
        crate::settings::secret::delete(&self.key())
            .map_err(|e| AuthError::InternalError(format!("{e:#}")))
    }
}

/// A manager pointed at one server, with the keyring behind it and whatever was
/// stored last time already loaded.
async fn manager(server: &str, url: &str) -> Result<AuthorizationManager> {
    let mut m = AuthorizationManager::new(url)
        .await
        .with_context(|| format!("ask {url} how it wants to be signed in to"))?;
    m.set_credential_store(Keyring { server: server.to_string() });
    // Whether there is anything stored is not an error either way: false simply
    // means this is the first sign-in.
    let _ = m.initialize_from_store().await;
    Ok(m)
}

/// Starts a sign-in and returns the URL to open.
///
/// Registers IRA with the provider if it has never seen her -- dynamic client
/// registration, which is what lets this work without anyone creating an app in
/// a dashboard first. A provider that does not support it needs a client id
/// entered by hand, and says so.
pub async fn begin(server: &str, url: &str, redirect: &str, scopes: &[String]) -> Result<String> {
    let mut m = manager(server, url).await?;
    let wanted: Vec<&str> = scopes.iter().map(String::as_str).collect();

    let stored = crate::db::oauth_get(server)?.unwrap_or_default();
    match stored.client_id {
        // Registered before: reuse the id rather than making a second client
        // every time someone presses the button.
        Some(id) => m
            .configure_client_id(&id)
            .map_err(|e| anyhow!("{server} did not accept the stored client id: {e}"))?,
        None => {
            let config = m
                .register_client("IRA", redirect, &wanted)
                .await
                .map_err(|e| anyhow!("{server} would not register IRA: {e}"))?;
            crate::db::oauth_set(
                server,
                &crate::db::OAuth {
                    client_id: Some(config.client_id.clone()),
                    scopes: scopes.to_vec(),
                },
            )?;
        }
    }

    let auth_url = m
        .get_authorization_url(&wanted)
        .await
        .map_err(|e| anyhow!("could not start the sign-in: {e}"))?;

    // The CSRF state the provider will hand back is in the URL it just built,
    // and it is how the callback finds this manager again.
    let state = state_of(&auth_url)
        .ok_or_else(|| anyhow!("{server} produced a sign-in URL with no state to track"))?;

    if let Ok(mut p) = pending().lock() {
        p.retain(|_, v| v.started.elapsed() < PENDING_FOR);
        p.insert(
            state,
            Pending {
                server: server.to_string(),
                manager: m,
                started: std::time::Instant::now(),
            },
        );
    }
    Ok(auth_url)
}

/// Finishes a sign-in from the redirect. Returns the server it was for, so the
/// callback route knows what to reconnect.
pub async fn finish(code: &str, state: &str) -> Result<String> {
    let Pending { server, manager, .. } = {
        let mut p = pending()
            .lock()
            .map_err(|_| anyhow!("the sign-in table is poisoned"))?;
        p.retain(|_, v| v.started.elapsed() < PENDING_FOR);
        // Removed, not read: a state is good for exactly one callback, so a
        // replayed redirect finds nothing.
        p.remove(state)
            .ok_or_else(|| anyhow!("that sign-in has expired or already finished"))?
    };

    manager
        .exchange_code_for_token(code, state)
        .await
        .map_err(|e| anyhow!("{server} refused the sign-in: {e}"))?;
    tracing::info!(server = %server, "signed in");
    Ok(server)
}

/// An HTTP client that carries this server's token and refreshes it.
///
/// `None` when the server has never been signed in to, which the caller reports
/// rather than treating as a failure -- an unsigned server is a server waiting
/// for a button press.
pub async fn client(server: &str, url: &str) -> Result<Option<AuthClient<reqwest::Client>>> {
    if crate::db::oauth_get(server)?.is_none() {
        return Ok(None);
    }
    let m = manager(server, url).await?;
    // Nothing stored means registered but never completed, which is not usable.
    if m.get_access_token().await.is_err() {
        return Ok(None);
    }
    Ok(Some(AuthClient::new(reqwest::Client::default(), m)))
}

/// Forgets a server's sign-in: the tokens and the client registration.
pub fn forget(server: &str) -> Result<()> {
    crate::settings::secret::delete(&crate::settings::secret::oauth_key(server))?;
    crate::db::oauth_delete(server)
}

/// The `state` parameter out of an authorization URL.
///
/// Parsed rather than tracked separately because rmcp generates the CSRF token
/// inside `get_authorization_url` and hands back only the URL. Deliberately not
/// a URL-parsing dependency for one query parameter.
fn state_of(url: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query
        .split('&')
        .filter_map(|p| p.split_once('='))
        .find(|(k, _)| *k == "state")
        .map(|(_, v)| percent_decode(v))
}

/// Enough percent-decoding for a query parameter. A CSRF state is generated by
/// the OAuth library and is URL-safe base64, so this is defensive rather than
/// load-bearing -- but a state that comes back subtly different matches nothing
/// and the sign-in fails with a confusing message.
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The state links a sign-in to its callback. If it is read wrong the
    /// callback finds no pending sign-in and the failure reads as "expired",
    /// which points at the wrong thing entirely.
    #[test]
    fn the_state_is_recovered_from_the_url_it_was_generated_into() {
        let url = "https://example.com/authorize?response_type=code&state=abc123&scope=read";
        assert_eq!(state_of(url).as_deref(), Some("abc123"));

        // Last parameter, first parameter, and one that was encoded.
        assert_eq!(state_of("https://x/a?state=xyz").as_deref(), Some("xyz"));
        assert_eq!(
            state_of("https://x/a?state=a%2Fb%2Bc&code=1").as_deref(),
            Some("a/b+c")
        );
        // A parameter that merely ends in "state" is not the state.
        assert_eq!(state_of("https://x/a?mystate=no").as_deref(), None);
        assert_eq!(state_of("https://x/a").as_deref(), None);
    }

    /// A replayed redirect must not finish a second time.
    #[tokio::test]
    async fn a_state_is_good_for_one_callback() {
        assert!(
            finish("code", "never-issued").await.is_err(),
            "an unknown state must not be accepted"
        );
    }
}

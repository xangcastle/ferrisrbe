//! OAuth2 Authorization Code + PKCE against a Keycloak realm: loopback
//! redirect on an ephemeral port, S256 challenge, token exchange and
//! refresh. Any public client of the realm that allows a loopback redirect
//! works.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use base64::Engine;
use serde::Deserialize;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
const TOKEN_TIMEOUT: Duration = Duration::from_secs(15);

/// Keycloak realm and public client used for login and refresh.
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub keycloak_url: String,
    pub realm: String,
    pub client_id: String,
}

impl OAuthConfig {
    /// `RBE_KEYCLOAK_URL`, `RBE_KEYCLOAK_REALM` and `RBE_KEYCLOAK_CLIENT_ID`,
    /// all three required: the realm and public client `login` uses.
    pub fn from_env() -> anyhow::Result<Self> {
        let var = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.is_empty())
                .with_context(|| format!("falta {name}"))
        };
        Ok(Self {
            keycloak_url: var("RBE_KEYCLOAK_URL")?,
            realm: var("RBE_KEYCLOAK_REALM")?,
            client_id: var("RBE_KEYCLOAK_CLIENT_ID")?,
        })
    }

    /// The realm and client that issued `tokens`, which refreshing them
    /// needs; the environment plays no part after `login`.
    pub fn of(tokens: &crate::store::StoredTokens) -> Self {
        Self {
            keycloak_url: tokens.keycloak_url.clone(),
            realm: tokens.realm.clone(),
            client_id: tokens.client_id.clone(),
        }
    }

    /// The realm's issuer URL, `<keycloak_url>/realms/<realm>`.
    pub fn issuer(&self) -> String {
        format!(
            "{}/realms/{}",
            self.keycloak_url.trim_end_matches('/'),
            self.realm
        )
    }
}

/// The endpoints this client needs from the realm's OpenID configuration.
#[derive(Debug, Deserialize)]
pub struct DiscoveryDocument {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default = "default_expires_in")]
    expires_in: u64,
}

/// Token lifetime assumed when the token endpoint omits `expires_in`.
fn default_expires_in() -> u64 {
    300
}

/// Tokens obtained by an interactive login.
pub struct LoginOutput {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Access token lifetime, in seconds.
    pub expires_in: u64,
}

/// Unix time, in seconds, `expires_in` seconds from now.
pub fn expires_at(expires_in: u64) -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|now| now.as_secs() + expires_in)
        .unwrap_or(0)
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(TOKEN_TIMEOUT)
        .build()
        .context("construyendo el cliente HTTP")
}

/// Fetch the realm's `.well-known/openid-configuration`.
pub async fn discover(config: &OAuthConfig) -> anyhow::Result<DiscoveryDocument> {
    let url = format!("{}/.well-known/openid-configuration", config.issuer());
    let body = http_client()?
        .get(url)
        .timeout(DISCOVERY_TIMEOUT)
        .send()
        .await
        .context("openid-configuration inalcanzable")?
        .error_for_status()
        .context("openid-configuration devolvio error")?
        .text()
        .await
        .context("leyendo openid-configuration")?;
    let document: DiscoveryDocument = serde_json::from_str(&body)
        .with_context(|| format!("openid-configuration invalida: {body}"))?;
    Ok(document)
}

fn random_hex(len: usize) -> String {
    const HEX: [char; 16] = [
        '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
    ];
    (0..len).map(|_| HEX[fastrand::usize(..16)]).collect()
}

/// A PKCE code verifier: 96 hex characters, within RFC 7636's 43..=128.
fn new_verifier() -> String {
    random_hex(96)
}

/// The S256 PKCE challenge for `verifier`: base64url(SHA-256), unpadded.
pub fn pkce_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// Percent-encode everything outside the RFC 3986 unreserved set.
fn form_urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn new_state() -> String {
    random_hex(32)
}

/// Run the Authorization Code + PKCE flow through the user's browser.
///
/// A one-shot listener on an ephemeral `127.0.0.1` port receives the
/// callback. The redirect URI names `localhost` rather than the address,
/// because that is what the Keycloak client registers
/// (`http://localhost:*`); the browser falls back from `::1` to IPv4 when it
/// follows it. A callback whose `state` does not match is rejected as a
/// possible CSRF.
pub async fn login_interactive(config: &OAuthConfig) -> anyhow::Result<LoginOutput> {
    let discovered = discover(config).await?;
    let verifier = new_verifier();
    let state = new_state();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://localhost:{port}/callback");

    let authorize_url = format!(
        "{}?response_type=code&client_id={}&scope=openid&redirect_uri={}&code_challenge={}&code_challenge_method=S256&state={}",
        discovered.authorization_endpoint,
        form_urlencode(&config.client_id),
        form_urlencode(&redirect_uri),
        pkce_challenge(&verifier),
        state,
    );

    println!("Abre esta URL en el browser para autenticarte:\n");
    println!("  {authorize_url}\n");
    if let Err(e) = std::process::Command::new("open")
        .arg(&authorize_url)
        .spawn()
    {
        let _ = std::process::Command::new("xdg-open")
            .arg(&authorize_url)
            .spawn();
        eprintln!("(no se pudo abrir el browser automaticamente: {e})");
    }

    let (code, returned_state) = await_callback(listener).await?;
    if returned_state.as_deref() != Some(state.as_str()) {
        bail!("el state devuelto no coincide; posible CSRF, abortando");
    }

    let tokens = exchange_code(
        config,
        &discovered.token_endpoint,
        &code,
        &redirect_uri,
        &verifier,
    )
    .await?;
    Ok(LoginOutput {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_in: tokens.expires_in,
    })
}

/// Accept the single browser callback, answer it, and return its `code` and
/// `state` query parameters.
async fn await_callback(
    listener: tokio::net::TcpListener,
) -> anyhow::Result<(String, Option<String>)> {
    let (mut socket, _) = listener.accept().await?;
    let mut buffer = vec![0u8; 16 * 1024];
    let read = tokio::io::AsyncReadExt::read(&mut socket, &mut buffer).await?;
    let request = String::from_utf8_lossy(&buffer[..read]);

    let request_line = request.lines().next().unwrap_or_default();
    let path = request_line.split_whitespace().nth(1).unwrap_or_default();
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or_default();

    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        match key {
            "code" => code = Some(value.to_string()),
            "state" => state = Some(value.to_string()),
            _ => {}
        }
    }

    let body = match &code {
        Some(_) => "Login completado. Puedes cerrar esta pestana.",
        None => "Falta el parametro code. Vuelve a intentar el login.",
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await?;
    let _ = tokio::io::AsyncWriteExt::shutdown(&mut socket).await;

    let Some(code) = code else {
        bail!("Keycloak no devolvio un authorization code");
    };
    Ok((code, state))
}

/// Exchange an authorization code and its PKCE verifier for tokens.
async fn exchange_code(
    config: &OAuthConfig,
    token_endpoint: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> anyhow::Result<TokenResponse> {
    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", config.client_id.as_str()),
        ("redirect_uri", redirect_uri),
        ("code", code),
        ("code_verifier", verifier),
    ];

    let response = http_client()?
        .post(token_endpoint)
        .form(&form)
        .send()
        .await
        .context("token endpoint inalcanzable")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("intercambio de token fallo ({status}): {body}");
    }
    let body = response
        .text()
        .await
        .context("leyendo la respuesta de token")?;
    serde_json::from_str::<TokenResponse>(&body).context("respuesta de token invalida")
}

/// Renew `tokens` with their refresh token, discovering the token endpoint.
pub async fn refresh(
    config: &OAuthConfig,
    tokens: &crate::store::StoredTokens,
) -> anyhow::Result<crate::store::StoredTokens> {
    let discovered = discover(config).await?;
    refresh_with_endpoint(config, &discovered.token_endpoint, tokens).await
}

/// Renew `tokens` against a known token endpoint.
///
/// Keycloak may omit a new refresh token; the current one is kept then.
pub async fn refresh_with_endpoint(
    config: &OAuthConfig,
    token_endpoint: &str,
    tokens: &crate::store::StoredTokens,
) -> anyhow::Result<crate::store::StoredTokens> {
    let refresh_token = tokens
        .refresh_token
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no hay refresh token; corre: rbe-cli login"))?;

    let response = http_client()?
        .post(token_endpoint)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", config.client_id.as_str()),
            ("refresh_token", refresh_token.as_str()),
        ])
        .send()
        .await
        .context("token endpoint inalcanzable durante refresh")?;

    if !response.status().is_success() {
        bail!(
            "refresh fallo ({}); corre: rbe-cli login",
            response.status()
        );
    }
    let body = response
        .text()
        .await
        .context("leyendo la respuesta de refresh")?;
    let renewed: TokenResponse =
        serde_json::from_str(&body).context("respuesta de refresh invalida")?;
    Ok(crate::store::StoredTokens {
        keycloak_url: tokens.keycloak_url.clone(),
        realm: tokens.realm.clone(),
        client_id: tokens.client_id.clone(),
        access_token: renewed.access_token,
        refresh_token: renewed.refresh_token.or(Some(refresh_token)),
        expires_at: expires_at(renewed.expires_in),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_matches_the_rfc_7636_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifier_is_long_hex() {
        let verifier = new_verifier();
        assert_eq!(verifier.len(), 96);
        assert!(verifier.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn urlencode_escapes_the_redirect_uri() {
        assert_eq!(
            form_urlencode("http://127.0.0.1:54123/callback"),
            "http%3A%2F%2F127.0.0.1%3A54123%2Fcallback"
        );
        assert_eq!(form_urlencode("rbe-cli"), "rbe-cli");
    }

    #[test]
    fn expires_at_is_in_the_future() {
        assert!(
            expires_at(300)
                > SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
        );
    }
}

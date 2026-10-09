//! Persistence of Keycloak tokens in ~/.rbe/token.json (mode 0600).

use std::io::Write as _;
use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// Seconds before `expires_at` at which a token already counts as expired,
/// so a request never leaves with a token that lapses in flight.
const EXPIRY_LEEWAY_SECS: u64 = 60;

/// The persisted login: where the tokens came from and the tokens themselves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTokens {
    pub keycloak_url: String,
    pub realm: String,
    pub client_id: String,
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Unix seconds after which the access token must be refreshed.
    pub expires_at: u64,
}

impl StoredTokens {
    /// Whether the access token expires within [`EXPIRY_LEEWAY_SECS`].
    pub fn expired(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now + EXPIRY_LEEWAY_SECS >= self.expires_at
    }
}

/// `$HOME/.rbe`, read from the environment on every call.
pub fn rbe_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".rbe")
}

/// `$HOME/.rbe/token.json`.
pub fn path() -> PathBuf {
    rbe_dir().join("token.json")
}

/// The stored tokens, or `None` when no login has been saved.
pub fn load() -> anyhow::Result<Option<StoredTokens>> {
    let path = path();
    if !path.exists() {
        return Ok(None);
    }
    let contents =
        std::fs::read_to_string(&path).with_context(|| format!("leyendo {}", path.display()))?;
    let tokens = serde_json::from_str(&contents)
        .with_context(|| format!("token file invalido: {}", path.display()))?;
    Ok(Some(tokens))
}

/// Write `tokens`, creating `~/.rbe` if needed. On Unix the file is created
/// with mode `0600`.
pub fn save(tokens: &StoredTokens) -> anyhow::Result<()> {
    let path = path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creando {}", parent.display()))?;
    }
    let contents = serde_json::to_string_pretty(tokens).context("serializando tokens")?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("abriendo {}", path.display()))?;
    file.write_all(contents.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> StoredTokens {
        StoredTokens {
            keycloak_url: "https://keycloak.example.com".to_string(),
            realm: "ferris".to_string(),
            client_id: "rbe-cli".to_string(),
            access_token: "token".to_string(),
            refresh_token: Some("refresh".to_string()),
            expires_at: 0,
        }
    }

    #[test]
    fn zero_expiry_counts_as_expired() {
        assert!(sample().expired());
    }

    #[test]
    fn roundtrip_preserves_fields() {
        let dir = tempfile::TempDir::new().unwrap();
        let home = dir.path().to_path_buf();
        let saved = std::env::var("HOME");
        std::env::set_var("HOME", &home);
        save(&sample()).unwrap();
        let loaded = load().unwrap().unwrap();
        assert_eq!(loaded.client_id, "rbe-cli");
        assert_eq!(loaded.refresh_token.as_deref(), Some("refresh"));
        match saved {
            Ok(previous) => std::env::set_var("HOME", previous),
            Err(_) => std::env::remove_var("HOME"),
        }
    }
}

//! rbe-cli — Keycloak login and local access to a private FerrisRBE.
//!
//! `login` runs the OAuth2 Authorization Code + PKCE loopback flow against
//! the realm and client in `RBE_KEYCLOAK_*` and persists tokens to
//! ~/.rbe/token.json, which later refreshes reuse. `connect` keeps a kubectl
//! port-forward to the in-cluster server alive: HTTP goes through a local reverse proxy that injects the
//! Bearer token and refreshes it on expiry or 401, so MCP clients and curl can
//! just talk to http://localhost:<port>; gRPC is forwarded as is, and Bazel
//! authenticates it through `credential-helper`.

mod oauth;
mod proxy;
mod store;

use std::io::Read as _;

use anyhow::Context as _;
use oauth::OAuthConfig;

/// Help text printed by `help`, `--help`, `-h` and a bare invocation.
const USAGE: &str = "rbe-cli — acceso autenticado a FerrisRBE

USAGE:
  rbe-cli login [--force]      Login OAuth2 (PKCE, loopback) contra Keycloak
  rbe-cli token [--refresh]    Imprime el access token vigente
  rbe-cli connect [OPTIONS]    Port-forward al clúster + proxy local con Bearer
  rbe-cli install              Copia este binario a ~/.rbe/bin/rbe-cli
  rbe-cli credential-helper get
                               Credential helper de Bazel (--credential_helper)

CONNECT OPTIONS:
  --port <PORT>             Puerto local del proxy HTTP (default 18080)
  --grpc-port <PORT>        Puerto local del gRPC reenviado (default 19094)
  --namespace <NS>          Namespace del servicio (default $RBE_CONNECT_NAMESPACE o default)
  --service <SVC>           Servicio a reenviar (default $RBE_CONNECT_SERVICE o rbe-cache)
  --remote-port <P>         Puerto HTTP del server en el pod (default 8080)
  --remote-grpc-port <P>    Puerto gRPC del server en el pod (default 9094)

ENV (login):
  RBE_KEYCLOAK_URL / RBE_KEYCLOAK_REALM / RBE_KEYCLOAK_CLIENT_ID";

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

/// Dispatch the subcommand in the first argument.
async fn run() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("login") => login().await,
        Some("token") => print_token(&args[1..]).await,
        Some("connect") => proxy::connect(&args[1..]).await,
        Some("install") => install(),
        Some("credential-helper") => credential_helper(&args[1..]).await,
        Some("--help" | "-h" | "help") | None => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => anyhow::bail!("comando desconocido: {other}\n\n{USAGE}"),
    }
}

/// `rbe-cli login`: interactive login, then persist the tokens.
async fn login() -> anyhow::Result<()> {
    let config = OAuthConfig::from_env()?;
    let output = oauth::login_interactive(&config).await?;
    store::save(&store::StoredTokens {
        keycloak_url: config.keycloak_url.clone(),
        realm: config.realm.clone(),
        client_id: config.client_id.clone(),
        access_token: output.access_token,
        refresh_token: output.refresh_token,
        expires_at: oauth::expires_at(output.expires_in),
    })?;
    println!("Login OK. Tokens en {}", store::path().display());
    Ok(())
}

/// `rbe-cli token`: print a valid access token, refreshing it when it is
/// about to expire or when `--refresh` is given.
async fn print_token(args: &[String]) -> anyhow::Result<()> {
    let force_refresh = args.iter().any(|a| a == "--refresh");
    println!("{}", valid_access_token(force_refresh).await?);
    Ok(())
}

/// The stored access token, refreshed and persisted first when it is about
/// to expire or when `force_refresh` is set.
async fn valid_access_token(force_refresh: bool) -> anyhow::Result<String> {
    let mut tokens = store::load()?
        .ok_or_else(|| anyhow::anyhow!("no hay tokens; corre primero: rbe-cli login"))?;
    if force_refresh || tokens.expired() {
        tokens = oauth::refresh(&OAuthConfig::of(&tokens), &tokens).await?;
        store::save(&tokens)?;
    }
    Ok(tokens.access_token)
}

/// `rbe-cli credential-helper get`: the Bazel credential helper protocol.
///
/// Bazel writes `{"uri": ...}` on stdin and reads the headers to attach to
/// every remote cache, BES and remote downloader call for that URI. The
/// token is the same for every URI, so the request is only drained. Bazel
/// reuses the answer for `--credential_helper_cache_duration`, which must
/// stay below the refresh leeway of [`store::StoredTokens::expired`].
async fn credential_helper(args: &[String]) -> anyhow::Result<()> {
    anyhow::ensure!(
        args.first().map(String::as_str) == Some("get"),
        "uso: rbe-cli credential-helper get"
    );
    let mut request = String::new();
    std::io::stdin()
        .read_to_string(&mut request)
        .context("leyendo la peticion de Bazel")?;
    let token = valid_access_token(false).await?;
    println!("{}", credential_response(&token));
    Ok(())
}

fn credential_response(token: &str) -> serde_json::Value {
    serde_json::json!({ "headers": { "Authorization": [format!("Bearer {token}")] } })
}

/// `rbe-cli install`: copy the running binary to `~/.rbe/bin/rbe-cli`, the
/// fixed path where `tools/rbe_credential_helper` looks for it. The copy is
/// staged and renamed so a running helper is never overwritten in place.
fn install() -> anyhow::Result<()> {
    let source = std::env::current_exe().context("localizando el binario actual")?;
    let bin = store::rbe_dir().join("bin");
    std::fs::create_dir_all(&bin).with_context(|| format!("creando {}", bin.display()))?;
    let staged = bin.join(".rbe-cli.partial");
    std::fs::copy(&source, &staged).with_context(|| format!("copiando {}", source.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    let target = bin.join("rbe-cli");
    std::fs::rename(&staged, &target)
        .with_context(|| format!("instalando {}", target.display()))?;
    println!("Instalado en {}", target.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_response_follows_the_bazel_protocol() {
        assert_eq!(
            credential_response("abc").to_string(),
            r#"{"headers":{"Authorization":["Bearer abc"]}}"#
        );
    }
}

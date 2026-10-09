//! Secrets a build's command line carries, kept out of what the BES stores
//! and serves.
//!
//! Bazel reports the whole client environment of a build as `--client_env`
//! (on CI: registry passwords, job tokens) and the values of `--action_env`,
//! `--repo_env`, `--test_env` and `--host_action_env`. A variable whose name
//! looks like a secret keeps its name and loses its value. A URL keeps its
//! user and loses the password or token of its userinfo, whatever variable
//! carries it: `CI_REPOSITORY_URL=https://gitlab-ci-token:<job token>@...`.

use std::borrow::Cow;
use std::collections::HashMap;

use serde_json::Value;

/// What a redacted value is replaced with.
pub const REDACTED: &str = "<redacted>";

/// Flags whose value is `NAME=VALUE`.
const ENV_FLAGS: [&str; 5] = [
    "--client_env=",
    "--action_env=",
    "--host_action_env=",
    "--repo_env=",
    "--test_env=",
];

/// Name fragments of a secret, matched case-insensitively.
const SECRET_FRAGMENTS: [&str; 10] = [
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "KEY",
    "CREDENTIAL",
    "AUTH",
    "PRIVATE",
    "COOKIE",
    "SESSION",
];

/// Whether an environment variable named `name` may hold a secret.
pub fn is_secret_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SECRET_FRAGMENTS
        .iter()
        .any(|fragment| upper.contains(fragment))
}

/// `arg` with the value of a secret environment flag replaced by
/// [`REDACTED`], and the credentials of its URLs too; the flag may appear
/// anywhere in the argument, as in
/// `--default_override=0:build=--action_env=NAME=VALUE`.
pub fn arg(arg: &str) -> String {
    for flag in ENV_FLAGS {
        let Some(at) = arg.rfind(flag) else { continue };
        let assignment = &arg[at + flag.len()..];
        if let Some((name, _)) = assignment.split_once('=') {
            if is_secret_name(name) {
                return format!("{}{name}={REDACTED}", &arg[..at + flag.len()]);
            }
        }
    }
    url_credentials(arg).into_owned()
}

/// `text` with the userinfo secret of every `scheme://userinfo@host` in it
/// replaced by [`REDACTED`]: `user:secret` keeps the user, a lone userinfo
/// is taken as a token and replaced whole.
pub fn url_credentials(text: &str) -> Cow<'_, str> {
    if !text.contains("://") {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let start = at + 3;
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let end = rest
            .find(|c: char| {
                matches!(c, '/' | '?' | '#' | '"' | '\'' | '<' | '>') || c.is_whitespace()
            })
            .unwrap_or(rest.len());
        let Some(userinfo_end) = rest[..end].rfind('@').filter(|&i| i > 0) else {
            continue;
        };
        match rest[..userinfo_end].split_once(':') {
            Some((user, _)) => out.push_str(&format!("{user}:{REDACTED}")),
            None => out.push_str(REDACTED),
        }
        rest = &rest[userinfo_end..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// Replaces, in place, the URL credentials of every string under `value`.
fn redact_urls(value: &mut Value) {
    match value {
        Value::String(s) => {
            if let Cow::Owned(redacted) = url_credentials(s) {
                *s = redacted;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_urls),
        Value::Object(fields) => fields.values_mut().for_each(redact_urls),
        _ => {}
    }
}

/// Redacts, in place, a stored build event: the command-line arguments of
/// `UnstructuredCommandLine.args`, `StructuredCommandLine.args` and
/// `OptionsParsed`'s option lists, and the URL credentials of any string.
pub fn event(event: &mut Value) {
    let Some(payload) = event.get_mut("payload") else {
        return;
    };
    let lists = [
        ("UnstructuredCommandLine", "args"),
        ("StructuredCommandLine", "args"),
        ("OptionsParsed", "cmd_line"),
        ("OptionsParsed", "explicit_cmd_line"),
        ("OptionsParsed", "startup_options"),
        ("OptionsParsed", "explicit_startup_options"),
    ];
    for (kind, field) in lists {
        let Some(Value::Array(args)) = payload.get_mut(kind).and_then(|p| p.get_mut(field)) else {
            continue;
        };
        for value in args.iter_mut() {
            if let Value::String(s) = value {
                *s = arg(s);
            }
        }
    }
    redact_urls(payload);
}

/// Redacts, in place, the values of secret variables of an environment map
/// and the URL credentials of the others.
pub fn env(vars: &mut HashMap<String, String>) {
    for (name, value) in vars.iter_mut() {
        if is_secret_name(name) {
            *value = REDACTED.to_string();
        } else if let Cow::Owned(redacted) = url_credentials(value) {
            *value = redacted;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_env_flags_lose_their_value_and_keep_their_name() {
        assert_eq!(
            arg("--client_env=PODMAN_ACCESS_TOKEN=glpat-abc"),
            "--client_env=PODMAN_ACCESS_TOKEN=<redacted>"
        );
        assert_eq!(
            arg("--client_env=CI_REGISTRY_PASSWORD=x=y"),
            "--client_env=CI_REGISTRY_PASSWORD=<redacted>"
        );
        assert_eq!(
            arg("--default_override=0:build=--action_env=AWS_SECRET_ACCESS_KEY=s3cr3t"),
            "--default_override=0:build=--action_env=AWS_SECRET_ACCESS_KEY=<redacted>"
        );
    }

    #[test]
    fn other_flags_and_variables_are_left_alone() {
        for kept in [
            "--client_env=CI_PIPELINE_URL=https://example/pipelines/1",
            "--action_env=PATH=/bin",
            "--action_env=GITHUB_TOKEN",
            "--jobs=200",
            "--remote_header=x-auth=abc",
        ] {
            assert_eq!(arg(kept), kept);
        }
    }

    #[test]
    fn an_event_has_its_command_line_and_options_redacted() {
        let mut stored = serde_json::json!({
            "kind": "UnstructuredCommandLine",
            "payload": {"UnstructuredCommandLine": {"args": [
                "build",
                "--client_env=CI_JOB_TOKEN=abc",
                "--client_env=CI=true",
            ]}},
        });
        event(&mut stored);
        assert_eq!(
            stored["payload"]["UnstructuredCommandLine"]["args"],
            serde_json::json!([
                "build",
                "--client_env=CI_JOB_TOKEN=<redacted>",
                "--client_env=CI=true"
            ])
        );

        let mut options = serde_json::json!({
            "payload": {"OptionsParsed": {"cmd_line": ["--test_env=DB_PASSWORD=pw"]}},
        });
        event(&mut options);
        assert_eq!(
            options["payload"]["OptionsParsed"]["cmd_line"][0],
            "--test_env=DB_PASSWORD=<redacted>"
        );
    }

    #[test]
    fn secret_values_of_an_environment_map_are_redacted() {
        let mut vars = HashMap::from([
            ("NPM_AUTH_TOKEN".to_string(), "t".to_string()),
            ("NODE_ENV".to_string(), "production".to_string()),
        ]);
        env(&mut vars);
        assert_eq!(vars["NPM_AUTH_TOKEN"], REDACTED);
        assert_eq!(vars["NODE_ENV"], "production");
    }

    #[test]
    fn url_credentials_lose_their_secret_and_keep_their_user() {
        assert_eq!(
            arg("--client_env=CI_REPOSITORY_URL=https://gitlab-ci-token:glcbt-x@code.example/a/b.git"),
            "--client_env=CI_REPOSITORY_URL=https://gitlab-ci-token:<redacted>@code.example/a/b.git"
        );
        assert_eq!(
            url_credentials("clone https://ghp_abc@github.com/o/r and https://u:p@h:8080?x=1"),
            "clone https://<redacted>@github.com/o/r and https://u:<redacted>@h:8080?x=1"
        );
        for kept in [
            "https://code.example/a/b.git",
            "mailto:someone@example.com",
            "https://example/path/user@host",
            "s3://bucket/key",
        ] {
            assert_eq!(url_credentials(kept), kept);
        }

        let mut vars = HashMap::from([(
            "CI_REPOSITORY_URL".to_string(),
            "https://gitlab-ci-token:t@code.example/r.git".to_string(),
        )]);
        env(&mut vars);
        assert_eq!(
            vars["CI_REPOSITORY_URL"],
            "https://gitlab-ci-token:<redacted>@code.example/r.git"
        );
    }

    #[test]
    fn an_event_has_the_url_credentials_of_any_string_redacted() {
        let mut stored = serde_json::json!({
            "payload": {"WorkspaceStatus": {"item": [
                {"key": "REMOTE", "value": "https://u:p@code.example/r.git"},
            ]}},
        });
        event(&mut stored);
        assert_eq!(
            stored["payload"]["WorkspaceStatus"]["item"][0]["value"],
            "https://u:<redacted>@code.example/r.git"
        );
    }
}

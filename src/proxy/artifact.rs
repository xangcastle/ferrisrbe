//! What a cached URL is: the ecosystem it belongs to, the package and
//! version it carries, and where that package is documented. Derived from
//! the URL alone, so it needs no extra storage and applies to every
//! artifact already on disk.

use serde::{Deserialize, Serialize};

/// The package ecosystem or registry an artifact comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ecosystem {
    Python,
    Npm,
    Go,
    Rust,
    Java,
    Bazel,
    Github,
    Docker,
    Node,
    Other,
}

impl Ecosystem {
    /// The value the API accepts as an `ecosystem` filter.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "python" => Self::Python,
            "npm" => Self::Npm,
            "go" => Self::Go,
            "rust" => Self::Rust,
            "java" => Self::Java,
            "bazel" => Self::Bazel,
            "github" => Self::Github,
            "docker" => Self::Docker,
            "node" => Self::Node,
            "other" => Self::Other,
            _ => return None,
        })
    }
}

/// What role the artifact plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// A versioned library: a wheel, an npm tarball, a crate, a jar.
    Package,
    /// Registry metadata: index pages, version lists, module files.
    Index,
    /// A compiler or runtime distribution.
    Toolchain,
    /// A file attached to a GitHub release.
    Release,
    /// A source archive of a repository at some ref.
    Source,
    /// A container image manifest or layer.
    Image,
}

/// An artifact as a person reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactInfo {
    pub ecosystem: Ecosystem,
    pub kind: ArtifactKind,
    pub name: Option<String>,
    pub version: Option<String>,
    /// The package's page in its registry, or its repository.
    pub docs_url: Option<String>,
}

impl ArtifactInfo {
    fn new(ecosystem: Ecosystem, kind: ArtifactKind) -> Self {
        Self {
            ecosystem,
            kind,
            name: None,
            version: None,
            docs_url: None,
        }
    }

    fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    fn version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    fn docs(mut self, url: impl Into<String>) -> Self {
        self.docs_url = Some(url.into());
        self
    }
}

const ARCHIVE_EXTENSIONS: [&str; 9] = [
    ".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst", ".tgz", ".zip", ".crate", ".jar", ".whl",
];

fn strip_archive_extension(file: &str) -> &str {
    ARCHIVE_EXTENSIONS
        .iter()
        .find_map(|ext| file.strip_suffix(ext))
        .unwrap_or(file)
}

/// Host and path segments of `url`, without scheme, query or fragment.
fn host_and_segments(url: &str) -> (&str, Vec<&str>) {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let mut parts = rest.split('/');
    let host = parts.next().unwrap_or("");
    (host, parts.filter(|s| !s.is_empty()).collect())
}

/// Classifies a cached URL. Unknown shapes fall back to [`Ecosystem::Other`]
/// with the file name, so every artifact gets a readable label.
pub fn classify(url: &str) -> ArtifactInfo {
    let (host, segments) = host_and_segments(url);
    let classified = match host {
        "files.pythonhosted.org" => python_file(&segments),
        "pypi.org" => python_index(&segments),
        "registry.npmjs.org" => npm(&segments),
        "proxy.golang.org" => go_module(&segments),
        "static.crates.io" | "crates.io" => crate_download(&segments),
        "repo1.maven.org" | "repo.maven.apache.org" => maven(&segments),
        "bcr.bazel.build" => bcr(&segments),
        "bcr.cloudflaremirrors.com" | "mirror.bazel.build" => mirrored(&segments),
        "github.com" => github(&segments),
        "codeload.github.com" => github_codeload(&segments),
        "raw.githubusercontent.com" => github_raw(&segments),
        "objects.githubusercontent.com" | "release-assets.githubusercontent.com" => {
            Some(ArtifactInfo::new(Ecosystem::Github, ArtifactKind::Release))
        }
        "index.docker.io" | "registry-1.docker.io" => docker_registry(&segments),
        "auth.docker.io" => Some(docker_token(url)),
        "static.rust-lang.org" => rust_toolchain(&segments),
        "dl.google.com" => google_download(&segments),
        "go.dev" | "golang.google.cn" => Some(
            ArtifactInfo::new(Ecosystem::Go, ArtifactKind::Index)
                .named("go")
                .docs("https://go.dev/dl/"),
        ),
        "nodejs.org" => node(&segments),
        "cdn.azul.com" => zulu_jdk(&segments),
        _ => None,
    };
    classified.unwrap_or_else(|| other(&segments))
}

/// A dotted version such as `3.14.3` or `v16.6`, without the `v`.
fn dotted_version(token: &str) -> Option<&str> {
    let version = token.strip_prefix('v').unwrap_or(token);
    let dotted = version.contains('.')
        && version.starts_with(|c: char| c.is_ascii_digit())
        && version.chars().all(|c| c.is_ascii_digit() || c == '.');
    dotted.then_some(version)
}

/// Best effort for a download of no known registry: the version is the
/// first dotted number in the file name (`helm-v3.14.3-linux-arm64.tar.gz`)
/// or else in the path (`release/v1.29.2/bin/linux/arm64/kubectl`), and the
/// name is what precedes it in the file, or the whole file.
fn other(segments: &[&str]) -> ArtifactInfo {
    let info = ArtifactInfo::new(Ecosystem::Other, ArtifactKind::Source);
    let Some(file) = segments.last() else {
        return info;
    };
    let stem = strip_archive_extension(file);
    let mut offset = 0;
    for token in stem.split(['-', '_']) {
        if let (Some(version), true) = (dotted_version(token), offset > 0) {
            let name = stem[..offset].trim_end_matches(['-', '_']);
            return info.named(name).version(version);
        }
        offset += token.len() + 1;
    }
    let info = info.named(*file);
    match segments
        .iter()
        .rev()
        .skip(1)
        .find_map(|s| dotted_version(s))
    {
        Some(version) => info.version(version),
        None => info,
    }
}

/// PEP 503 normalized project name, the one pypi.org URLs use.
fn pypi_normalized(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_dash = false;
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            if !last_dash {
                out.push('-');
            }
            last_dash = true;
        } else {
            out.extend(c.to_lowercase());
            last_dash = false;
        }
    }
    out
}

fn python_file(segments: &[&str]) -> Option<ArtifactInfo> {
    let file = *segments.last()?;
    let (name, version) = if let Some(stem) = file.strip_suffix(".whl") {
        let mut parts = stem.split('-');
        (parts.next()?, parts.next()?)
    } else {
        strip_archive_extension(file).rsplit_once('-')?
    };
    let project = pypi_normalized(name);
    Some(
        ArtifactInfo::new(Ecosystem::Python, ArtifactKind::Package)
            .docs(format!("https://pypi.org/project/{project}/{version}/"))
            .named(project)
            .version(version),
    )
}

fn python_index(segments: &[&str]) -> Option<ArtifactInfo> {
    let info = ArtifactInfo::new(Ecosystem::Python, ArtifactKind::Index);
    let name = match segments {
        ["simple", name, ..] | ["pypi", name, ..] => pypi_normalized(name),
        _ => return Some(info.docs("https://pypi.org/")),
    };
    Some(
        info.docs(format!("https://pypi.org/project/{name}/"))
            .named(name),
    )
}

fn npm(segments: &[&str]) -> Option<ArtifactInfo> {
    let dash = segments.iter().position(|s| *s == "-");
    let Some(dash) = dash else {
        let name = segments.join("/");
        return Some(
            ArtifactInfo::new(Ecosystem::Npm, ArtifactKind::Index)
                .docs(format!("https://www.npmjs.com/package/{name}"))
                .named(name),
        );
    };
    let name = segments[..dash].join("/");
    let base = segments[..dash].last()?;
    let file = segments.last()?;
    let version = file
        .strip_suffix(".tgz")?
        .strip_prefix(base)?
        .strip_prefix('-')?;
    Some(
        ArtifactInfo::new(Ecosystem::Npm, ArtifactKind::Package)
            .docs(format!("https://www.npmjs.com/package/{name}/v/{version}"))
            .named(name)
            .version(version),
    )
}

/// Undoes the module proxy's case encoding: `!x` stands for `X`.
fn go_unescape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut upper = false;
    for c in path.chars() {
        if c == '!' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

fn go_module(segments: &[&str]) -> Option<ArtifactInfo> {
    let at = segments
        .iter()
        .position(|s| *s == "@v" || *s == "@latest")?;
    let module = go_unescape(&segments[..at].join("/"));
    let index = ArtifactInfo::new(Ecosystem::Go, ArtifactKind::Index);
    let Some(file) = segments.get(at + 1) else {
        return Some(
            index
                .docs(format!("https://pkg.go.dev/{module}"))
                .named(module),
        );
    };
    let (version, kind) = if let Some(v) = file.strip_suffix(".zip") {
        (v, ArtifactKind::Package)
    } else if let Some(v) = file
        .strip_suffix(".mod")
        .or_else(|| file.strip_suffix(".info"))
    {
        (v, ArtifactKind::Index)
    } else {
        return Some(
            index
                .docs(format!("https://pkg.go.dev/{module}"))
                .named(module),
        );
    };
    let version = go_unescape(version);
    Some(
        ArtifactInfo::new(Ecosystem::Go, kind)
            .docs(format!("https://pkg.go.dev/{module}@{version}"))
            .named(module)
            .version(version),
    )
}

fn crate_download(segments: &[&str]) -> Option<ArtifactInfo> {
    let (name, version) = match segments {
        ["crates", name, version, "download"] => (*name, *version),
        ["api", "v1", "crates", name, version, "download"] => (*name, *version),
        ["crates", name, file] => {
            let version = file
                .strip_suffix(".crate")?
                .strip_prefix(name)?
                .strip_prefix('-')?;
            (*name, version)
        }
        _ => return None,
    };
    Some(
        ArtifactInfo::new(Ecosystem::Rust, ArtifactKind::Package)
            .docs(format!("https://crates.io/crates/{name}/{version}"))
            .named(name)
            .version(version),
    )
}

fn maven(segments: &[&str]) -> Option<ArtifactInfo> {
    let path = match segments {
        ["maven2", rest @ ..] => rest,
        _ => segments,
    };
    let file = *path.last()?;
    if file.starts_with("maven-metadata.xml") && path.len() >= 3 {
        let artifact = path[path.len() - 2];
        let group = path[..path.len() - 2].join(".");
        return Some(
            ArtifactInfo::new(Ecosystem::Java, ArtifactKind::Index)
                .docs(format!(
                    "https://central.sonatype.com/artifact/{group}/{artifact}"
                ))
                .named(format!("{group}:{artifact}")),
        );
    }
    if path.len() < 4 {
        return None;
    }
    let version = path[path.len() - 2];
    let artifact = path[path.len() - 3];
    let group = path[..path.len() - 3].join(".");
    let kind = if file.ends_with(".pom") || file.ends_with(".sha1") || file.ends_with(".md5") {
        ArtifactKind::Index
    } else {
        ArtifactKind::Package
    };
    Some(
        ArtifactInfo::new(Ecosystem::Java, kind)
            .docs(format!(
                "https://central.sonatype.com/artifact/{group}/{artifact}/{version}"
            ))
            .named(format!("{group}:{artifact}"))
            .version(version),
    )
}

fn bcr(segments: &[&str]) -> Option<ArtifactInfo> {
    let info = ArtifactInfo::new(Ecosystem::Bazel, ArtifactKind::Index);
    match segments {
        ["modules", name, rest @ ..] => {
            let info = info
                .docs(format!("https://registry.bazel.build/modules/{name}"))
                .named(*name);
            Some(match rest {
                [version, _, ..] => info.version(*version),
                _ => info,
            })
        }
        _ => Some(info.docs("https://registry.bazel.build/")),
    }
}

/// A Bazel mirror keeps the original host as the first path segment.
fn mirrored(segments: &[&str]) -> Option<ArtifactInfo> {
    let inner_host = segments.first()?;
    if inner_host.contains('.') {
        let inner = format!("https://{}", segments.join("/"));
        let info = classify(&inner);
        if info.ecosystem != Ecosystem::Other {
            return Some(info);
        }
    }
    let mut info = other(segments);
    info.ecosystem = Ecosystem::Bazel;
    Some(info)
}

fn github(segments: &[&str]) -> Option<ArtifactInfo> {
    let (owner, repo, rest) = match segments {
        [owner, repo, rest @ ..] => (*owner, *repo, rest),
        _ => return None,
    };
    let name = format!("{owner}/{repo}");
    match rest {
        ["releases", "download", tag, ..] => Some(
            ArtifactInfo::new(Ecosystem::Github, ArtifactKind::Release)
                .docs(format!("https://github.com/{name}/releases/tag/{tag}"))
                .named(name)
                .version(*tag),
        ),
        ["archive", refs @ ..] if !refs.is_empty() => {
            let reference = strip_archive_extension(&refs.join("/"))
                .trim_start_matches("refs/tags/")
                .trim_start_matches("refs/heads/")
                .to_string();
            Some(
                ArtifactInfo::new(Ecosystem::Github, ArtifactKind::Source)
                    .docs(format!("https://github.com/{name}/tree/{reference}"))
                    .named(name)
                    .version(reference),
            )
        }
        _ => Some(
            ArtifactInfo::new(Ecosystem::Github, ArtifactKind::Source)
                .docs(format!("https://github.com/{name}"))
                .named(name),
        ),
    }
}

fn github_codeload(segments: &[&str]) -> Option<ArtifactInfo> {
    match segments {
        [owner, repo, _format, reference @ ..] if !reference.is_empty() => {
            let name = format!("{owner}/{repo}");
            let reference = reference.join("/");
            Some(
                ArtifactInfo::new(Ecosystem::Github, ArtifactKind::Source)
                    .docs(format!("https://github.com/{name}/tree/{reference}"))
                    .named(name)
                    .version(reference),
            )
        }
        _ => None,
    }
}

fn github_raw(segments: &[&str]) -> Option<ArtifactInfo> {
    match segments {
        [owner, repo, reference, path @ ..] if !path.is_empty() => {
            let name = format!("{owner}/{repo}");
            Some(
                ArtifactInfo::new(Ecosystem::Github, ArtifactKind::Source)
                    .docs(format!(
                        "https://github.com/{name}/blob/{reference}/{}",
                        path.join("/")
                    ))
                    .named(name)
                    .version(*reference),
            )
        }
        _ => None,
    }
}

fn docker_hub_page(repository: &str) -> String {
    match repository.strip_prefix("library/") {
        Some(official) => format!("https://hub.docker.com/_/{official}"),
        None => format!("https://hub.docker.com/r/{repository}"),
    }
}

fn docker_registry(segments: &[&str]) -> Option<ArtifactInfo> {
    let path = match segments {
        ["v2", rest @ ..] => rest,
        _ => return None,
    };
    let marker = path
        .iter()
        .position(|s| *s == "manifests" || *s == "blobs")?;
    let repository = path[..marker].join("/");
    let reference = path.get(marker + 1)?;
    let mut info = ArtifactInfo::new(Ecosystem::Docker, ArtifactKind::Image)
        .docs(docker_hub_page(&repository))
        .named(repository.trim_start_matches("library/").to_string());
    if !reference.starts_with("sha256:") {
        info = info.version(*reference);
    }
    Some(info)
}

fn docker_token(url: &str) -> ArtifactInfo {
    let info = ArtifactInfo::new(Ecosystem::Docker, ArtifactKind::Index);
    let repository = url
        .split_once("scope=repository:")
        .and_then(|(_, rest)| rest.split(':').next());
    match repository {
        Some(repository) => info
            .docs(docker_hub_page(repository))
            .named(repository.trim_start_matches("library/").to_string()),
        None => info,
    }
}

fn rust_toolchain(segments: &[&str]) -> Option<ArtifactInfo> {
    let file = strip_archive_extension(segments.last()?);
    let date = segments
        .iter()
        .find(|s| s.len() == 10 && s.as_bytes().get(4) == Some(&b'-'));
    let parts: Vec<&str> = file.split('-').collect();
    let at = parts.iter().position(|p| {
        p.starts_with(|c: char| c.is_ascii_digit()) || *p == "nightly" || *p == "beta"
    })?;
    let name = parts[..at].join("-");
    let mut version = parts[at].to_string();
    if let (Some(date), "nightly" | "beta") = (date, parts[at]) {
        version = format!("{version}-{date}");
    }
    Some(
        ArtifactInfo::new(Ecosystem::Rust, ArtifactKind::Toolchain)
            .docs("https://forge.rust-lang.org/infra/other-installation-methods.html")
            .named(name)
            .version(version),
    )
}

fn google_download(segments: &[&str]) -> Option<ArtifactInfo> {
    match segments {
        ["go", file] => {
            let version = file.strip_prefix("go")?.split(".linux").next()?;
            let version = version.split(".darwin").next()?;
            Some(
                ArtifactInfo::new(Ecosystem::Go, ArtifactKind::Toolchain)
                    .docs(format!("https://go.dev/doc/devel/release#go{version}"))
                    .named("go")
                    .version(version),
            )
        }
        _ => None,
    }
}

/// `zulu25.32.17-ca-jdk25.0.2-linux_aarch64.tar.gz`: the JDK version follows `jdk`.
fn zulu_jdk(segments: &[&str]) -> Option<ArtifactInfo> {
    let file = strip_archive_extension(segments.last()?);
    let version = file.split('-').find_map(|t| t.strip_prefix("jdk"))?;
    Some(
        ArtifactInfo::new(Ecosystem::Java, ArtifactKind::Toolchain)
            .docs("https://www.azul.com/downloads/")
            .named("zulu-jdk")
            .version(version),
    )
}

fn node(segments: &[&str]) -> Option<ArtifactInfo> {
    let release = segments.iter().find(|s| s.starts_with('v'))?;
    let version = release.trim_start_matches('v');
    let kind = if segments.last().is_some_and(|f| f.starts_with("SHASUMS")) {
        ArtifactKind::Index
    } else {
        ArtifactKind::Toolchain
    };
    Some(
        ArtifactInfo::new(Ecosystem::Node, kind)
            .docs(format!("https://nodejs.org/en/blog/release/{release}"))
            .named("node")
            .version(version),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(
        url: &str,
        ecosystem: Ecosystem,
        kind: ArtifactKind,
        name: &str,
        version: Option<&str>,
        docs: Option<&str>,
    ) {
        let info = classify(url);
        assert_eq!(info.ecosystem, ecosystem, "{url}");
        assert_eq!(info.kind, kind, "{url}");
        assert_eq!(info.name.as_deref(), Some(name), "{url}");
        assert_eq!(info.version.as_deref(), version, "{url}");
        if let Some(docs) = docs {
            assert_eq!(info.docs_url.as_deref(), Some(docs), "{url}");
        }
    }

    #[test]
    fn python_wheels_sdists_and_index_pages() {
        check(
            "https://files.pythonhosted.org/packages/b7/b9/c538f279a4e237a006a2c98387d081e9eb060d203d8ed34467cc0f0b9b53/packaging-26.0-py3-none-any.whl",
            Ecosystem::Python, ArtifactKind::Package, "packaging", Some("26.0"),
            Some("https://pypi.org/project/packaging/26.0/"),
        );
        check(
            "https://files.pythonhosted.org/packages/56/3f/adf5f6b963307765209fc27943516c7c605d250234b01899d524b8f01fe3/google_api_python_client-2.154.0-py2.py3-none-any.whl",
            Ecosystem::Python, ArtifactKind::Package, "google-api-python-client", Some("2.154.0"),
            Some("https://pypi.org/project/google-api-python-client/2.154.0/"),
        );
        check(
            "https://files.pythonhosted.org/packages/d3/e8/536555596dbb79f6e77418aeb40bdc1758c26725aba31919ba449e6d5e6a/Django-5.1.4.tar.gz",
            Ecosystem::Python, ArtifactKind::Package, "django", Some("5.1.4"), None,
        );
        check(
            "https://pypi.org/simple/aiofiles/",
            Ecosystem::Python,
            ArtifactKind::Index,
            "aiofiles",
            None,
            Some("https://pypi.org/project/aiofiles/"),
        );
    }

    #[test]
    fn npm_tarballs_scoped_and_not() {
        check(
            "https://registry.npmjs.org/prettier/-/prettier-3.8.1.tgz",
            Ecosystem::Npm,
            ArtifactKind::Package,
            "prettier",
            Some("3.8.1"),
            Some("https://www.npmjs.com/package/prettier/v/3.8.1"),
        );
        check(
            "https://registry.npmjs.org/@types/react/-/react-19.2.7.tgz",
            Ecosystem::Npm,
            ArtifactKind::Package,
            "@types/react",
            Some("19.2.7"),
            Some("https://www.npmjs.com/package/@types/react/v/19.2.7"),
        );
    }

    #[test]
    fn go_modules_with_escaped_paths() {
        check(
            "https://proxy.golang.org/github.com/golang/protobuf/@v/v1.5.4.zip",
            Ecosystem::Go,
            ArtifactKind::Package,
            "github.com/golang/protobuf",
            Some("v1.5.4"),
            Some("https://pkg.go.dev/github.com/golang/protobuf@v1.5.4"),
        );
        check(
            "https://proxy.golang.org/github.com/!burnt!sushi/toml/@v/v1.4.0.mod",
            Ecosystem::Go,
            ArtifactKind::Index,
            "github.com/BurntSushi/toml",
            Some("v1.4.0"),
            None,
        );
        check(
            "https://proxy.golang.org/golang.org/x/sync/@v/list",
            Ecosystem::Go,
            ArtifactKind::Index,
            "golang.org/x/sync",
            None,
            None,
        );
    }

    #[test]
    fn crates_in_both_download_shapes() {
        check(
            "https://static.crates.io/crates/regex-syntax/0.8.11/download",
            Ecosystem::Rust,
            ArtifactKind::Package,
            "regex-syntax",
            Some("0.8.11"),
            Some("https://crates.io/crates/regex-syntax/0.8.11"),
        );
        check(
            "https://static.crates.io/crates/serde/serde-1.0.215.crate",
            Ecosystem::Rust,
            ArtifactKind::Package,
            "serde",
            Some("1.0.215"),
            None,
        );
    }

    #[test]
    fn maven_artifacts() {
        check(
            "https://repo1.maven.org/maven2/com/google/code/gson/gson/2.11.0/gson-2.11.0.jar",
            Ecosystem::Java,
            ArtifactKind::Package,
            "com.google.code.gson:gson",
            Some("2.11.0"),
            Some("https://central.sonatype.com/artifact/com.google.code.gson/gson/2.11.0"),
        );
    }

    #[test]
    fn bazel_registry_and_mirrors() {
        check(
            "https://bcr.bazel.build/modules/aspect_bazel_lib/2.22.5/patches/go_dev_dep.patch",
            Ecosystem::Bazel,
            ArtifactKind::Index,
            "aspect_bazel_lib",
            Some("2.22.5"),
            Some("https://registry.bazel.build/modules/aspect_bazel_lib"),
        );
        check(
            "https://bcr.cloudflaremirrors.com/github.com/bazelbuild/rules_rust/releases/download/0.70.0/rules_rust-0.70.0.tar.gz",
            Ecosystem::Github, ArtifactKind::Release, "bazelbuild/rules_rust", Some("0.70.0"), None,
        );
        check(
            "https://mirror.bazel.build/bazel_java_tools/releases/java/v19.0/java_tools-v19.0.zip",
            Ecosystem::Bazel,
            ArtifactKind::Source,
            "java_tools",
            Some("19.0"),
            None,
        );
        check(
            "https://mirror.bazel.build/cdn.azul.com/zulu/bin/zulu25.32.17-ca-jdk25.0.2-linux_aarch64.tar.gz",
            Ecosystem::Java,
            ArtifactKind::Toolchain,
            "zulu-jdk",
            Some("25.0.2"),
            None,
        );
    }

    #[test]
    fn github_releases_archives_and_codeload() {
        check(
            "https://github.com/mgoltzsche/podman-static/releases/download/v5.5.2/podman-linux-arm64.tar.gz",
            Ecosystem::Github, ArtifactKind::Release, "mgoltzsche/podman-static", Some("v5.5.2"),
            Some("https://github.com/mgoltzsche/podman-static/releases/tag/v5.5.2"),
        );
        check(
            "https://github.com/bazelbuild/rules_go/archive/refs/tags/v0.50.1.tar.gz",
            Ecosystem::Github,
            ArtifactKind::Source,
            "bazelbuild/rules_go",
            Some("v0.50.1"),
            None,
        );
        check(
            "https://codeload.github.com/markedjs/marked/tar.gz/3ee82dc4a1d8a2d0d6c3a5b6c3e2a9c9b47a1b8f",
            Ecosystem::Github, ArtifactKind::Source, "markedjs/marked",
            Some("3ee82dc4a1d8a2d0d6c3a5b6c3e2a9c9b47a1b8f"), None,
        );
    }

    #[test]
    fn docker_images_and_tokens() {
        check(
            "https://index.docker.io/v2/library/ubuntu/blobs/sha256:20377134ad8875ad73c4a4f12b5d8e28c8665da80756c2afbb47d1f730bf2e5e",
            Ecosystem::Docker, ArtifactKind::Image, "ubuntu", None,
            Some("https://hub.docker.com/_/ubuntu"),
        );
        check(
            "https://auth.docker.io/token?scope=repository:ubuntu/apache2:pull&service=registry.docker.io",
            Ecosystem::Docker, ArtifactKind::Index, "ubuntu/apache2", None,
            Some("https://hub.docker.com/r/ubuntu/apache2"),
        );
    }

    #[test]
    fn toolchains() {
        check(
            "https://static.rust-lang.org/dist/rustc-1.92.0-aarch64-unknown-linux-gnu.tar.xz",
            Ecosystem::Rust,
            ArtifactKind::Toolchain,
            "rustc",
            Some("1.92.0"),
            None,
        );
        check(
            "https://static.rust-lang.org/dist/2026-04-16/rustfmt-nightly-aarch64-unknown-linux-gnu.tar.xz",
            Ecosystem::Rust, ArtifactKind::Toolchain, "rustfmt", Some("nightly-2026-04-16"), None,
        );
        check(
            "https://dl.google.com/go/go1.25.1.linux-arm64.tar.gz",
            Ecosystem::Go,
            ArtifactKind::Toolchain,
            "go",
            Some("1.25.1"),
            None,
        );
        check(
            "https://nodejs.org/dist/v22.14.0/node-v22.14.0-linux-arm64.tar.xz",
            Ecosystem::Node,
            ArtifactKind::Toolchain,
            "node",
            Some("22.14.0"),
            Some("https://nodejs.org/en/blog/release/v22.14.0"),
        );
    }

    #[test]
    fn unknown_hosts_get_a_best_effort_name_and_version() {
        check(
            "https://get.helm.sh/helm-v3.14.3-linux-arm64.tar.gz",
            Ecosystem::Other,
            ArtifactKind::Source,
            "helm",
            Some("3.14.3"),
            None,
        );
        check(
            "https://releases.hashicorp.com/terraform/1.7.5/terraform_1.7.5_linux_arm64.zip",
            Ecosystem::Other,
            ArtifactKind::Source,
            "terraform",
            Some("1.7.5"),
            None,
        );
        check(
            "https://dl.k8s.io/release/v1.29.2/bin/linux/arm64/kubectl",
            Ecosystem::Other,
            ArtifactKind::Source,
            "kubectl",
            Some("1.29.2"),
            None,
        );
        check(
            "https://curl.se/ca/cacert.pem",
            Ecosystem::Other,
            ArtifactKind::Source,
            "cacert.pem",
            None,
            None,
        );
    }
}

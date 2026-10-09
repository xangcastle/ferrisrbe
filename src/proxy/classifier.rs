//! Retention policy heuristics for cached assets.
//!
//! Package ecosystems split cleanly in two: versioned artifacts are sealed
//! forever, and the index files that point at them are rewritten constantly.
//! Caching both under one TTL means either serving stale indexes or re-fetching
//! immutable tarballs, so the class drives retention instead.

/// How long a cached asset stays valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetClass {
    /// Versioned, content-sealed artifact. Evicted only under disk pressure.
    Immutable,
    /// Index or manifest that upstream rewrites. Expires on a short TTL.
    Mutable,
}

impl AssetClass {
    /// Prometheus label for this class.
    pub fn as_str(self) -> &'static str {
        match self {
            AssetClass::Immutable => "immutable",
            AssetClass::Mutable => "mutable",
        }
    }
}

/// Suffixes that identify a sealed, versioned artifact.
const IMMUTABLE_SUFFIXES: &[&str] = &[
    ".whl", ".jar", ".war", ".aar", ".egg", ".gem", ".crate", ".deb", ".rpm", ".apk", ".nupkg",
    ".zip", ".tgz", ".tbz2", ".txz", ".tar", ".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst", ".exe",
    ".msi", ".dmg", ".bin", ".so", ".dylib", ".wasm", ".pom", ".sha256", ".sha512", ".asc", ".sig",
];

/// Filenames that are always indexes, whatever their extension.
const MUTABLE_FILENAMES: &[&str] = &[
    "maven-metadata.xml",
    "packages.gz",
    "packages.xz",
    "packages",
    "release",
    "inrelease",
    "release.gpg",
    "sources.gz",
    "repomd.xml",
    "index.html",
    "index.json",
    "index.yaml",
];

/// Classify an asset from the path component of its URL.
///
/// Query strings and fragments are ignored and matching is case-insensitive.
/// Known index filenames, and every `maven-metadata.*` checksum or signature
/// (which tracks the index rather than an artifact), are mutable; a filename
/// with a sealed-artifact suffix is immutable.
///
/// Anything unrecognised is mutable. That is the safe default: a short TTL on
/// an artifact that is really immutable only costs a revalidation, while a
/// perpetual TTL on an index that is really mutable serves a stale index to
/// every build until the entry is evicted.
pub fn classify(url_path: &str) -> AssetClass {
    let path = url_path.split(['?', '#']).next().unwrap_or(url_path);
    let filename = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();

    let is_index = filename.is_empty()
        || MUTABLE_FILENAMES.contains(&filename.as_str())
        || filename.starts_with("maven-metadata.");
    if !is_index
        && IMMUTABLE_SUFFIXES
            .iter()
            .any(|suffix| filename.ends_with(suffix))
    {
        return AssetClass::Immutable;
    }
    AssetClass::Mutable
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versioned_artifacts_are_immutable() {
        for path in [
            "/org/repo/releases/download/v1.2.3/tool-linux-arm64.tar.gz",
            "/packages/py3/d/django/Django-5.0.1-py3-none-any.whl",
            "/maven2/com/google/guava/guava/33.0.0/guava-33.0.0.jar",
            "/api/v1/crates/serde/1.0.197/download.crate",
            "/pool/main/c/curl/curl_8.5.0-1_arm64.deb",
            "/archive/refs/tags/v9.tar.xz",
        ] {
            assert_eq!(classify(path), AssetClass::Immutable, "{path}");
        }
    }

    #[test]
    fn indexes_and_manifests_are_mutable() {
        for path in [
            "/maven2/com/google/guava/guava/maven-metadata.xml",
            "/maven2/com/google/guava/guava/maven-metadata.xml.sha1",
            "/simple/django/",
            "/django",
            "/dists/bookworm/main/binary-arm64/Packages.gz",
            "/dists/bookworm/InRelease",
            "/index.html",
        ] {
            assert_eq!(classify(path), AssetClass::Mutable, "{path}");
        }
    }

    #[test]
    fn query_strings_and_fragments_are_ignored() {
        assert_eq!(
            classify("/download/tool.tar.gz?token=abc&expires=1"),
            AssetClass::Immutable
        );
        assert_eq!(
            classify("/download/tool.tar.gz#frag"),
            AssetClass::Immutable
        );
    }

    #[test]
    fn classification_is_case_insensitive() {
        assert_eq!(
            classify("/Downloads/Tool-1.0.TAR.GZ"),
            AssetClass::Immutable
        );
        assert_eq!(classify("/repo/Maven-Metadata.xml"), AssetClass::Mutable);
    }

    #[test]
    fn unknown_extensions_default_to_mutable() {
        assert_eq!(classify("/some/thing.weird"), AssetClass::Mutable);
        assert_eq!(classify("/no-extension-at-all"), AssetClass::Mutable);
    }
}

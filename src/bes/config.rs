//! BES (Build Event Service) configuration and on-disk layout.
//!
//! Every path the BES store reads or writes is derived here from
//! [`BesConfig::data_dir`]:
//!
//! ```text
//! <data_dir>/
//!   <invocation_id>/{events.jsonl, summary.json, targets.jsonl}
//!   targets.jsonl, tests.jsonl          global execution histories
//!   by-label/{targets,tests}/<sha256>.jsonl
//!   by-label/.complete                  per-label index sentinel
//! ```

use std::path::{Path, PathBuf};

/// Name of [`BesConfig::label_dir`] under the data directory.
const LABEL_DIR_NAME: &str = "by-label";

/// Configuration for the BES backend.
#[derive(Debug, Clone)]
pub struct BesConfig {
    /// Root of the BES store.
    pub data_dir: PathBuf,
    /// Age after which the maintenance task deletes a finished build and
    /// compacts it out of the histories. `None` retains every build.
    /// Never `Some(0)`.
    pub max_retention_days: Option<u32>,
}

impl BesConfig {
    /// Reads `RBE_BES_DATA_DIR` (default `/data/bes`) and
    /// `RBE_BES_MAX_RETENTION_DAYS`; a missing, unparsable or zero retention
    /// disables it.
    pub fn from_env() -> Self {
        let data_dir = std::env::var("RBE_BES_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/data/bes"));
        let max_retention_days = std::env::var("RBE_BES_MAX_RETENTION_DAYS")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|days| *days > 0);
        Self {
            data_dir,
            max_retention_days,
        }
    }

    /// Directory holding everything recorded for one invocation. Callers
    /// pass an id accepted by [`is_invocation_id`], which every entry point
    /// (BES stream, REST API, MCP) checks.
    pub fn invocation_dir(&self, invocation_id: &str) -> PathBuf {
        self.data_dir.join(invocation_id)
    }

    /// Raw build events of one invocation, one JSON object per line.
    pub fn events_path(&self, invocation_id: &str) -> PathBuf {
        self.invocation_dir(invocation_id).join("events.jsonl")
    }

    /// Derived summary of one invocation.
    pub fn summary_path(&self, invocation_id: &str) -> PathBuf {
        self.invocation_dir(invocation_id).join("summary.json")
    }

    /// Target executions of one invocation, one record per line.
    pub fn invocation_targets_path(&self, invocation_id: &str) -> PathBuf {
        self.invocation_dir(invocation_id).join("targets.jsonl")
    }

    /// Target executions of every retained invocation.
    pub fn targets_path(&self) -> PathBuf {
        self.data_dir.join("targets.jsonl")
    }

    /// Test executions of every retained invocation.
    pub fn tests_path(&self) -> PathBuf {
        self.data_dir.join("tests.jsonl")
    }

    /// Whether `path`, an entry of `data_dir`, is an invocation directory.
    /// Every directory there is one, except [`Self::label_dir`].
    pub fn is_invocation_dir(&self, path: &Path) -> bool {
        path.is_dir() && path != self.label_dir()
    }

    /// Root of the per-label histories: one JSONL per target or test label.
    pub fn label_dir(&self) -> PathBuf {
        self.data_dir.join(LABEL_DIR_NAME)
    }

    /// Per-label histories of targets.
    pub fn target_labels_dir(&self) -> PathBuf {
        self.label_dir().join("targets")
    }

    /// Per-label histories of tests.
    pub fn test_labels_dir(&self) -> PathBuf {
        self.label_dir().join("tests")
    }

    /// Present once the per-label histories cover every record of the global
    /// histories; until then label lookups read the global files.
    pub fn label_index_sentinel(&self) -> PathBuf {
        self.label_dir().join(".complete")
    }

    /// History of one target label.
    pub fn target_label_path(&self, label: &str) -> PathBuf {
        self.target_labels_dir().join(label_file_name(label))
    }

    /// History of one test label.
    pub fn test_label_path(&self, label: &str) -> PathBuf {
        self.test_labels_dir().join(label_file_name(label))
    }
}

/// Longest accepted invocation id; Bazel's are 36-character UUIDs.
const MAX_INVOCATION_ID_LEN: usize = 64;

/// Whether `id` can name an invocation directory: 1 to
/// [`MAX_INVOCATION_ID_LEN`] ASCII letters, digits, `-` or `_`, and not the
/// name of [`BesConfig::label_dir`]. Anything else (`/`, `..`, an absolute
/// path) would resolve outside `<data_dir>/<id>`.
pub fn is_invocation_id(id: &str) -> bool {
    (1..=MAX_INVOCATION_ID_LEN).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && id != LABEL_DIR_NAME
}

/// File name of a label's history: the hex SHA-256 of the label, since
/// labels carry `/`, `:` and `@` and have no length bound.
pub fn label_file_name(label: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}.jsonl", Sha256::digest(label.as_bytes()))
}

impl Default for BesConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("/data/bes"),
            max_retention_days: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bazel_invocation_ids_are_accepted() {
        assert!(is_invocation_id("1e0bb3be-e2d0-4d2e-ad35-23530d3b40c9"));
        assert!(is_invocation_id("inv_1"));
    }

    #[test]
    fn ids_that_leave_the_invocation_directory_are_refused() {
        let too_long = "a".repeat(MAX_INVOCATION_ID_LEN + 1);
        for id in [
            "",
            "..",
            "../etc",
            "/etc/passwd",
            "a/b",
            "a\\b",
            "by-label",
            "ñ",
            &too_long,
        ] {
            assert!(!is_invocation_id(id), "{id:?}");
        }
    }
}

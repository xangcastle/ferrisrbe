//! Actions the server promotes to the critical path by their
//! `RequestMetadata` (action mnemonic and owning target), without touching
//! the action itself.
//!
//! Setting `Priority`/`Pool` through `exec_properties` in a BUILD file does
//! the same, but it applies to every action of the target and changes their
//! keys: the `CopyFile` actions of a rule that copies its `srcs` to the bin
//! dir then conflict with the identical copies a lint aspect makes of the
//! same files. Matching on the metadata leaves the action digest, and so
//! the cache, untouched.

use std::collections::HashSet;

use crate::proto::build::bazel::remote::execution::v2::RequestMetadata;

/// Comma-separated `<mnemonic>:<target>` entries, e.g.
/// `Webpack://web:dist,JsRunBinary://lib:dist`.
pub const ENV_ACTIONS: &str = "RBE_CRITICAL_ACTIONS";

/// Worker pool the critical actions require, e.g. `fast`; unset leaves them
/// on any worker.
pub const ENV_POOL: &str = "RBE_CRITICAL_POOL";

/// The configured critical actions and the pool they run on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CriticalActions {
    actions: HashSet<(String, String)>,
    pool: Option<String>,
}

/// `//pkg:name` for the main-repository forms `@//pkg:name` and
/// `@@//pkg:name`; any other label as is.
fn normalize_target(target: &str) -> String {
    match target
        .strip_prefix("@@")
        .or_else(|| target.strip_prefix('@'))
    {
        Some(rest) if rest.starts_with("//") => rest.to_string(),
        _ => target.to_string(),
    }
}

impl CriticalActions {
    /// The critical actions of an [`ENV_ACTIONS`] value and an optional
    /// [`ENV_POOL`] value.
    ///
    /// # Errors
    ///
    /// For an entry that is not `<mnemonic>:<target>` with a target starting
    /// with `//` or `@`, or an empty pool.
    pub fn parse(actions: &str, pool: Option<&str>) -> Result<Self, String> {
        let actions = actions
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(|entry| {
                entry
                    .split_once(':')
                    .filter(|(mnemonic, target)| {
                        !mnemonic.is_empty()
                            && (target.starts_with("//") || target.starts_with('@'))
                    })
                    .map(|(mnemonic, target)| (mnemonic.to_string(), normalize_target(target)))
                    .ok_or_else(|| format!("{ENV_ACTIONS}: {entry:?} is not <mnemonic>:<target>"))
            })
            .collect::<Result<_, _>>()?;
        let pool = match pool.map(str::trim) {
            Some("") => return Err(format!("{ENV_POOL} is empty")),
            pool => pool.map(str::to_string),
        };
        Ok(Self { actions, pool })
    }

    /// The critical actions of [`ENV_ACTIONS`] and [`ENV_POOL`]; none when
    /// [`ENV_ACTIONS`] is unset.
    ///
    /// # Errors
    ///
    /// When either variable does not parse (see [`CriticalActions::parse`]).
    pub fn from_env() -> Result<Self, String> {
        let actions = std::env::var(ENV_ACTIONS).unwrap_or_default();
        let pool = std::env::var(ENV_POOL).ok();
        Self::parse(&actions, pool.as_deref())
    }

    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// The platform properties the server adds to an action with this
    /// metadata: `Priority=critical` and, with a pool configured,
    /// `Pool=<pool>`. Properties the action already sets are kept, so an
    /// explicit `Pool` or `Priority` wins. Nothing for any other action.
    pub fn properties_for(
        &self,
        metadata: Option<&RequestMetadata>,
        properties: &[(String, String)],
    ) -> Vec<(String, String)> {
        let Some(metadata) = metadata else {
            return Vec::new();
        };
        let key = (
            metadata.action_mnemonic.clone(),
            normalize_target(&metadata.target_id),
        );
        if !self.actions.contains(&key) {
            return Vec::new();
        }
        let has = |name: &str| properties.iter().any(|(n, _)| n == name);
        let mut added = Vec::new();
        if !has("Priority") {
            added.push(("Priority".to_string(), "critical".to_string()));
        }
        if let Some(pool) = self.pool.as_ref().filter(|_| !has("Pool")) {
            added.push(("Pool".to_string(), pool.clone()));
        }
        added
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(mnemonic: &str, target: &str) -> RequestMetadata {
        RequestMetadata {
            action_mnemonic: mnemonic.into(),
            target_id: target.into(),
            ..Default::default()
        }
    }

    fn pairs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn a_listed_action_gets_critical_priority_and_the_pool() {
        let critical =
            CriticalActions::parse("Webpack://web:dist, JsRunBinary://lib:dist", Some("fast"))
                .unwrap();
        let props = critical.properties_for(Some(&metadata("Webpack", "//web:dist")), &[]);
        assert_eq!(props, pairs(&[("Priority", "critical"), ("Pool", "fast")]));
    }

    #[test]
    fn the_same_target_with_another_mnemonic_is_not_critical() {
        let critical = CriticalActions::parse("Webpack://web:dist", Some("fast")).unwrap();
        assert!(critical
            .properties_for(Some(&metadata("CopyFile", "//web:dist")), &[])
            .is_empty());
        assert!(critical.properties_for(None, &[]).is_empty());
    }

    #[test]
    fn main_repository_labels_match_in_any_form() {
        let critical = CriticalActions::parse("Webpack:@@//web:dist", None).unwrap();
        let props = critical.properties_for(Some(&metadata("Webpack", "@//web:dist")), &[]);
        assert_eq!(props, pairs(&[("Priority", "critical")]));
        let props = critical.properties_for(Some(&metadata("Webpack", "//web:dist")), &[]);
        assert_eq!(props, pairs(&[("Priority", "critical")]));
    }

    #[test]
    fn properties_the_action_sets_win() {
        let critical = CriticalActions::parse("Webpack://web:dist", Some("fast")).unwrap();
        let props = critical.properties_for(
            Some(&metadata("Webpack", "//web:dist")),
            &pairs(&[("Pool", "big"), ("Priority", "critical")]),
        );
        assert!(props.is_empty());
    }

    #[test]
    fn malformed_entries_and_empty_pools_are_errors() {
        assert!(CriticalActions::parse("Webpack", None).is_err());
        assert!(CriticalActions::parse(":web:dist", None).is_err());
        assert!(CriticalActions::parse("Webpack:web:dist", None).is_err());
        assert!(CriticalActions::parse("Webpack://web:dist", Some(" ")).is_err());
        assert!(CriticalActions::parse("", None).unwrap().is_empty());
    }
}

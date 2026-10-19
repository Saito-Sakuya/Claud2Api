use super::{disk, version::Version};
use crate::error::{Fault, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    schema_version: u32,
    pub revision: u64,
    pub enabled: bool,
    pub blocked_versions: Vec<String>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            schema_version: 1,
            revision: 1,
            enabled: true,
            blocked_versions: vec![],
        }
    }
}
impl Policy {
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join("policy.json");
        if !path.exists() {
            let policy = Self::default();
            disk::atomic_json(&path, &policy)?;
            return Ok(policy);
        }
        let policy: Self = disk::read_json(&path, 65536)?;
        if policy.schema_version != 1
            || policy.revision == 0
            || policy.blocked_versions.len() > 512
            || policy
                .blocked_versions
                .iter()
                .any(|v| Version::parse(v).is_none())
        {
            return Err(Fault::internal());
        }
        Ok(policy)
    }
    fn commit(&mut self, root: &Path, mut next: Self) -> Result<()> {
        next.revision = self.revision.checked_add(1).ok_or_else(Fault::internal)?;
        disk::atomic_json(&root.join("policy.json"), &next)?;
        *self = next;
        Ok(())
    }
    pub fn set_enabled(&mut self, root: &Path, enabled: bool) -> Result<()> {
        if self.enabled != enabled {
            let mut next = self.clone();
            next.enabled = enabled;
            self.commit(root, next)?;
        }
        Ok(())
    }
    pub fn attempted(
        &mut self,
        root: &Path,
        version: &str,
        rollback: bool,
        automatic: bool,
    ) -> Result<()> {
        let mut next = self.clone();
        next.blocked_versions.retain(|v| v != version);
        if next.blocked_versions.len() >= 512 {
            if automatic {
                return Err(Fault::new(
                    409,
                    "api_error",
                    "too many unfinished updates; install a working version manually",
                ));
            }
            // Keep manual recovery possible without evicting failed targets.
            // Pausing also prevents retrying this unrecorded manual attempt.
            next.enabled = false;
            return self.commit(root, next);
        }
        next.blocked_versions.push(version.to_owned());
        if rollback {
            next.enabled = false;
        }
        // Persist before starting: an interrupted attempt must not loop on restart.
        self.commit(root, next)
    }
    pub fn completed(&mut self, root: &Path, version: &str) -> Result<()> {
        let current = Version::parse(version).ok_or_else(Fault::internal)?;
        let mut next = self.clone();
        next.blocked_versions
            .retain(|v| Version::parse(v).is_some_and(|target| target.is_patch_of(&current)));
        if next.blocked_versions != self.blocked_versions {
            self.commit(root, next)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempts_survive_restart_and_success_clears_obsolete_versions() {
        let folder = tempfile::tempdir().unwrap();
        let root = folder.path();
        let mut policy = Policy::load(root).unwrap();
        assert!(policy.enabled);
        policy
            .attempted(root, "v0.1.0-alpha.6.1", false, true)
            .unwrap();
        let mut policy = Policy::load(root).unwrap();
        assert_eq!(policy.blocked_versions, ["v0.1.0-alpha.6.1"]);
        policy
            .attempted(root, "v0.1.0-alpha.6.2", false, true)
            .unwrap();
        policy.completed(root, "v0.1.0-alpha.6").unwrap();
        assert_eq!(
            Policy::load(root).unwrap().blocked_versions,
            ["v0.1.0-alpha.6.1", "v0.1.0-alpha.6.2"]
        );
        policy.completed(root, "v0.1.0-alpha.6.2").unwrap();
        assert!(Policy::load(root).unwrap().blocked_versions.is_empty());
        policy
            .attempted(root, "v0.1.0-alpha.6", true, false)
            .unwrap();
        assert!(!Policy::load(root).unwrap().enabled);
        policy.completed(root, "v0.1.0-alpha.6").unwrap();
        assert!(!Policy::load(root).unwrap().enabled);
    }

    #[test]
    fn failed_policy_write_keeps_the_in_memory_revision_and_setting() {
        let folder = tempfile::tempdir().unwrap();
        let mut policy = Policy::load(folder.path()).unwrap();
        let revision = policy.revision;
        let absent = folder.path().join("absent");
        assert!(policy.set_enabled(&absent, false).is_err());
        assert_eq!(policy.revision, revision);
        assert!(policy.enabled);
    }

    #[test]
    fn later_failures_never_evict_an_earlier_blocked_update() {
        let folder = tempfile::tempdir().unwrap();
        let mut policy = Policy::load(folder.path()).unwrap();
        for n in 1..=65 {
            policy
                .attempted(folder.path(), &format!("v0.1.0-alpha.6.{n}"), false, true)
                .unwrap();
        }
        let mut policy = Policy::load(folder.path()).unwrap();
        assert!(
            policy
                .blocked_versions
                .contains(&"v0.1.0-alpha.6.1".to_owned())
        );
        policy.blocked_versions = (1..=512).map(|n| format!("v0.1.0-alpha.6.{n}")).collect();
        assert!(
            policy
                .attempted(folder.path(), "v0.1.0-alpha.6.513", false, true)
                .is_err()
        );
        assert!(
            policy
                .blocked_versions
                .contains(&"v0.1.0-alpha.6.1".to_owned())
        );
        policy
            .attempted(folder.path(), "v0.1.0-alpha.7", false, false)
            .unwrap();
        assert!(!policy.enabled);
        policy.completed(folder.path(), "v0.1.0-alpha.7").unwrap();
        assert!(policy.blocked_versions.is_empty());
    }
}

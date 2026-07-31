//! The daemon-wide map of per-domain registries — spec §3.5.

use super::persist::{self, DomainRegistry};
use chrono::Utc;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

/// Holds one [`DomainRegistry`] per domain that has been touched.
///
/// **Registries open lazily, on the first `domain_data` call for a domain.**
/// The alternative — opening at domain creation — writes a permanent file for
/// every domain that ever existed, and the skill actively recommends the
/// topology that maximises that: one domain per test run. Tens of thousands of
/// empty registries in `~/.config/logmon/domain_data/` would be the cost of a
/// feature nobody used on those domains.
///
/// The visible consequence, stated because it is a real one: a `get` on a
/// never-touched domain returns nothing at all, not even `/logmon/domain`. It
/// reports `total: 0`, which is the honest answer — no provenance was ever
/// recorded here — rather than a file conjured to make the reply look furnished.
pub struct DomainDataStore {
    config_dir: PathBuf,
    broker_version: String,
    open: RwLock<HashMap<String, Arc<DomainRegistry>>>,
}

impl DomainDataStore {
    pub fn new(config_dir: PathBuf, broker_version: String) -> Self {
        Self {
            config_dir,
            broker_version,
            open: RwLock::new(HashMap::new()),
        }
    }

    /// Get, or open, a domain's registry.
    ///
    /// Opening loads the file if there is one and stamps `/logmon/*` (§3.7).
    /// Any persistence failure at that point is logged rather than returned:
    /// the caller asked for a registry, and refusing to hand one back would
    /// turn a bookkeeping problem into a failed query. The failure surfaces
    /// again — and is returned — on the first call that actually writes.
    pub fn for_domain(&self, domain: &str) -> Arc<DomainRegistry> {
        if let Some(existing) = self
            .open
            .read()
            .expect("domain_data map poisoned")
            .get(domain)
        {
            return Arc::clone(existing);
        }
        let mut guard = self.open.write().expect("domain_data map poisoned");
        // Re-check: another thread may have opened it between the two locks.
        if let Some(existing) = guard.get(domain) {
            return Arc::clone(existing);
        }
        let path = persist::path_for(&self.config_dir, domain);
        let loaded = persist::load(&path);
        let reg = Arc::new(DomainRegistry::new(domain.to_string(), path, loaded));
        if let Err(e) = persist::stamp_boot(&reg, &self.broker_version, Utc::now()) {
            tracing::warn!(domain, error = %e, "domain_data boot stamp not persisted");
        }
        guard.insert(domain.to_string(), Arc::clone(&reg));
        reg
    }

    /// Count a new era for a domain whose registry file already exists (§3.5).
    ///
    /// Called when a domain's seq counter is seeded at 0 — the event that makes
    /// two archived seq ranges for one name incomparable. **Does nothing when
    /// no file exists**, which is what keeps the lazy-open rule above intact:
    /// a domain nobody recorded provenance for does not gain a registry just by
    /// being created twice.
    pub fn note_seq_origin(&self, domain: &str) {
        let path = persist::path_for(&self.config_dir, domain);
        if !path.exists() {
            return;
        }
        let reg = self.for_domain(domain);
        if let Err(e) = persist::note_seq_origin(&reg, Utc::now()) {
            tracing::warn!(domain, error = %e, "domain_data incarnation not persisted");
        }
    }

    /// Drop a domain's registry from memory without touching its file.
    ///
    /// Lifetime is by file, not by domain (§3.5): the registry for `X` survives
    /// while its file does, so a deleted domain that is re-created adopts its
    /// own history. This only releases the handle.
    pub fn forget(&self, domain: &str) {
        self.open
            .write()
            .expect("domain_data map poisoned")
            .remove(domain);
    }
}

impl std::fmt::Debug for DomainDataStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DomainDataStore")
            .field("config_dir", &self.config_dir)
            .field("open", &self.open.read().map(|g| g.len()).unwrap_or(0))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain_data::store::DataEntry;

    fn store(dir: &tempfile::TempDir) -> DomainDataStore {
        DomainDataStore::new(dir.path().to_path_buf(), "0.9.0".into())
    }

    #[test]
    fn a_domain_gets_one_registry_however_many_times_it_is_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        assert!(Arc::ptr_eq(&s.for_domain("t3"), &s.for_domain("t3")));
        assert!(!Arc::ptr_eq(&s.for_domain("t3"), &s.for_domain("t4")));
    }

    #[test]
    fn opening_stamps_the_broker_keys() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        s.for_domain("t3").read(|r| {
            assert_eq!(r.get("/logmon/domain").unwrap().value, "t3");
            assert_eq!(r.get("/logmon/version").unwrap().value, "0.9.0");
            assert_eq!(r.get("/logmon/incarnation").unwrap().value, "1");
        });
    }

    #[test]
    fn a_domain_nobody_touches_leaves_no_file() {
        // The skill recommends one domain per test run; opening eagerly would
        // leave a permanent file for every one of them.
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        s.note_seq_origin("never-used");
        assert!(!persist::path_for(dir.path(), "never-used").exists());
    }

    #[test]
    fn a_seq_origin_advances_the_era_only_once_a_registry_exists() {
        let dir = tempfile::tempdir().unwrap();
        {
            let s = store(&dir);
            s.for_domain("t3");
        }
        let s = store(&dir);
        s.note_seq_origin("t3");
        s.for_domain("t3")
            .read(|r| assert_eq!(r.get("/logmon/incarnation").unwrap().value, "2"));
    }

    #[test]
    fn forgetting_a_domain_keeps_its_file_so_a_recreated_name_adopts_it() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        let (_, saved) = s.for_domain("t3").mutate(|r| {
            r.update(
                &[DataEntry {
                    path: "/Build/commit".into(),
                    value: Some("abc".into()),
                    ttl: None,
                }],
                Utc::now(),
            )
        });
        saved.unwrap();
        s.forget("t3");
        s.for_domain("t3")
            .read(|r| assert_eq!(r.get("/Build/commit").unwrap().value, "abc"));
    }

    #[test]
    fn two_domains_differing_only_in_case_do_not_share_a_registry() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        let (_, saved) = s.for_domain("Build").mutate(|r| {
            r.update(
                &[DataEntry {
                    path: "/x".into(),
                    value: Some("upper".into()),
                    ttl: None,
                }],
                Utc::now(),
            )
        });
        saved.unwrap();
        let (_, saved) = s.for_domain("build").mutate(|r| {
            r.update(
                &[DataEntry {
                    path: "/x".into(),
                    value: Some("lower".into()),
                    ttl: None,
                }],
                Utc::now(),
            )
        });
        saved.unwrap();
        // On APFS these would be one file, and one would have silently
        // overwritten the other's provenance.
        s.forget("Build");
        s.forget("build");
        s.for_domain("Build")
            .read(|r| assert_eq!(r.get("/x").unwrap().value, "upper"));
        s.for_domain("build")
            .read(|r| assert_eq!(r.get("/x").unwrap().value, "lower"));
    }
}

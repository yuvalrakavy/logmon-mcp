//! Machine-owned API domain definitions. Runtime data and active runs are
//! ephemeral. Each domain has an independent, atomically published definition.
use crate::daemon::domain::DomainId;
use crate::daemon::persistence::atomic_write;
use logmon_broker_protocol::DomainsCreate;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

pub(crate) struct DomainDefinitions(PathBuf);

impl DomainDefinitions {
    pub fn new(dir: &Path) -> Self {
        Self(dir.join("persistent-domains"))
    }

    fn path(&self, id: &DomainId) -> PathBuf {
        self.0.join(format!("{id}.json"))
    }

    pub fn contains(&self, id: &DomainId) -> std::io::Result<bool> {
        self.path(id).try_exists()
    }

    pub fn save(&self, id: &DomainId, config: &DomainsCreate) -> anyhow::Result<()> {
        atomic_write(&self.path(id), &serde_json::to_vec_pretty(config)?)
    }

    pub fn delete(&self, id: &DomainId) -> std::io::Result<bool> {
        match std::fs::remove_file(self.path(id)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub fn load(&self) -> anyhow::Result<Vec<DomainsCreate>> {
        let entries = match std::fs::read_dir(&self.0) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut definitions = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let read = || -> anyhow::Result<DomainsCreate> {
                let def: DomainsCreate = serde_json::from_slice(&std::fs::read(&path)?)?;
                let id = DomainId::new(&def.name)?;
                anyhow::ensure!(
                    def.persist && self.path(&id) == path,
                    "definition name or persistence mismatch"
                );
                Ok(def)
            };
            match read() {
                Ok(def) => definitions.push(def),
                Err(e) => {
                    tracing::warn!(path=%path.display(), error=%e, "could not read persistent domain")
                }
            }
        }
        Ok(definitions)
    }
}

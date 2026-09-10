//! In-memory plan construction. These helpers never mutate source files.
use crate::adapters::{PlannedFile, ReconcilePlan};
use gh_common::GhError;
use std::path::{Path, PathBuf};

impl ReconcilePlan {
    pub(crate) fn normalize(&mut self) {
        self.writes
            .sort_by(|left, right| left.path.cmp(&right.path));
        self.remove_paths.sort();
        self.remove_paths.dedup();
        let mut roots = Vec::<PathBuf>::new();
        for path in self.remove_paths.drain(..) {
            if !roots.iter().any(|root| path.starts_with(root)) {
                roots.push(path);
            }
        }
        self.remove_paths = roots;
        self.files.sort();
        self.files.dedup();
        self.owned_paths.sort();
        self.owned_paths.dedup();
    }

    pub(crate) fn write(&mut self, path: &Path, body: impl AsRef<[u8]>) -> Result<(), GhError> {
        self.writes.retain(|write| write.path != path);
        self.writes.push(PlannedFile {
            path: path.to_path_buf(),
            body: body.as_ref().to_vec(),
            mode: Some(0o600),
        });
        Ok(())
    }

    pub(crate) fn remove(&mut self, path: &Path) -> Result<(), GhError> {
        self.writes.retain(|write| !write.path.starts_with(path));
        self.remove_paths.push(path.to_path_buf());
        Ok(())
    }

    pub(crate) fn read(&self, path: &Path) -> Result<Vec<u8>, GhError> {
        if let Some(write) = self.writes.iter().find(|write| write.path == path) {
            return Ok(write.body.clone());
        }
        if self.remove_paths.iter().any(|root| path.starts_with(root)) {
            return Err(GhError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::from(std::io::ErrorKind::NotFound),
            });
        }
        if path.is_symlink() {
            return Err(GhError::config(format!(
                "refusing symlink source: {}",
                path.display()
            )));
        }
        std::fs::read(path).map_err(|source| GhError::Io {
            path: path.to_path_buf(),
            source,
        })
    }

    pub(crate) fn read_json_object(
        &self,
        path: &Path,
    ) -> Result<serde_json::Map<String, serde_json::Value>, GhError> {
        match self.read(path) {
            Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
                .map_err(|error| GhError::config(format!("parsing {}: {error}", path.display())))?
                .as_object()
                .cloned()
                .ok_or_else(|| {
                    GhError::config(format!("{} must contain an object", path.display()))
                }),
            Err(GhError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                Ok(Default::default())
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn read_toml_table(
        &self,
        path: &Path,
    ) -> Result<toml::map::Map<String, toml::Value>, GhError> {
        match self.read(path) {
            Ok(bytes) => String::from_utf8(bytes)
                .map_err(|error| GhError::Serde(error.to_string()))?
                .parse::<toml::Table>()
                .map_err(|error| GhError::config(format!("parsing {}: {error}", path.display()))),
            Err(GhError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                Ok(Default::default())
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn component_dirs(
        &mut self,
        sources: &[PathBuf],
        target: &Path,
        files: &mut Vec<PathBuf>,
    ) -> Result<(), GhError> {
        self.remove(target)?;
        for source in sources {
            self.component_tree(source, target)?;
        }
        if !sources.is_empty() {
            files.push(target.to_path_buf());
        }
        Ok(())
    }

    pub(crate) fn component_files(
        &mut self,
        sources: &[PathBuf],
        target: &Path,
        files: &mut Vec<PathBuf>,
    ) -> Result<(), GhError> {
        self.remove(target)?;
        for source in sources {
            let name = source
                .file_name()
                .ok_or_else(|| GhError::config("component has no filename"))?;
            self.component_file(source, &target.join(name))?;
        }
        if !sources.is_empty() {
            files.push(target.to_path_buf());
        }
        Ok(())
    }

    fn component_file(&mut self, source: &Path, target: &Path) -> Result<(), GhError> {
        if self.writes.iter().any(|write| write.path == target) {
            return Err(GhError::config(format!(
                "component collision at {}",
                target.display()
            )));
        }
        let metadata = std::fs::symlink_metadata(source).map_err(|source_error| GhError::Io {
            path: source.to_path_buf(),
            source: source_error,
        })?;
        if !metadata.is_file() {
            return Err(GhError::config("component must be a regular file"));
        }
        self.write(target, self.read(source)?)?;
        // Package content never chooses managed-file permissions. Generated
        // configuration is owner-only; manifest-declared helpers remain in the
        // package store and are made executable there.
        Ok(())
    }

    pub(crate) fn component_tree(&mut self, source: &Path, target: &Path) -> Result<(), GhError> {
        // A version renderer can replicate a tree it has just constructed.
        let planned = self
            .writes
            .iter()
            .filter(|write| write.path.starts_with(source))
            .cloned()
            .collect::<Vec<_>>();
        if !planned.is_empty() {
            for mut write in planned {
                write.path = target.join(write.path.strip_prefix(source).unwrap());
                if self
                    .writes
                    .iter()
                    .any(|existing| existing.path == write.path)
                {
                    return Err(GhError::config("component collision"));
                }
                self.writes.push(write);
            }
            return Ok(());
        }
        let metadata = std::fs::symlink_metadata(source).map_err(|source_error| GhError::Io {
            path: source.to_path_buf(),
            source: source_error,
        })?;
        if !metadata.is_dir() {
            return Err(GhError::config("component tree must be a directory"));
        }
        for entry in std::fs::read_dir(source).map_err(|source_error| GhError::Io {
            path: source.to_path_buf(),
            source: source_error,
        })? {
            let entry = entry.map_err(|source_error| GhError::Io {
                path: source.to_path_buf(),
                source: source_error,
            })?;
            let kind = entry.file_type().map_err(|source_error| GhError::Io {
                path: entry.path(),
                source: source_error,
            })?;
            let destination = target.join(entry.file_name());
            if kind.is_dir() {
                self.component_tree(&entry.path(), &destination)?;
            } else {
                self.component_file(&entry.path(), &destination)?;
            }
        }
        Ok(())
    }
}

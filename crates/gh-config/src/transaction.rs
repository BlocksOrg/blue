//! Durable filesystem transactions for managed configuration revisions.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use gh_common::{GhError, PreparedAtomicWrite};
use serde::{Deserialize, Serialize};

use crate::adapters::ReconcilePlan;
use crate::{lock_file, unlock_file, validate_home_path};

thread_local! {
    static LOCK_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static ACTIVE_IDS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

struct TransactionLock {
    file: Option<std::fs::File>,
}

impl TransactionLock {
    fn acquire(home: &Path) -> Result<(Self, bool), GhError> {
        let nested = LOCK_DEPTH.with(|depth| depth.get() > 0);
        if nested {
            LOCK_DEPTH.with(|depth| depth.set(depth.get() + 1));
            return Ok((Self { file: None }, false));
        }
        let path = home.join(".blue-locks/transactions.lock");
        validate_home_path(home, &path, false)?;
        create_owner_only_ancestors(home, path.parent().expect("lock has parent"))?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| GhError::Io {
                path: path.clone(),
                source,
            })?;
        lock_file(&file, "filesystem transaction")?;
        LOCK_DEPTH.with(|depth| depth.set(1));
        Ok((Self { file: Some(file) }, true))
    }
}

impl Drop for TransactionLock {
    fn drop(&mut self) {
        LOCK_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
        if let Some(file) = self.file.as_ref() {
            unlock_file(file);
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TransactionPhase {
    Prepared,
    Applying,
    Committed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct JournalTarget {
    path: PathBuf,
    existed: bool,
    backup: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct TransactionJournal {
    schema_version: u32,
    id: String,
    parent_id: Option<String>,
    phase: TransactionPhase,
    targets: Vec<JournalTarget>,
    staging_directories: Vec<PathBuf>,
    trash_paths: Vec<PathBuf>,
    created_directories: Vec<PathBuf>,
}

pub(crate) struct FileTransaction {
    home: PathBuf,
    id: String,
    root: PathBuf,
    journal: TransactionJournal,
    staging_for_write: Vec<PathBuf>,
    prepared: Vec<Option<PreparedAtomicWrite>>,
    _lock: TransactionLock,
    committed: bool,
    cleanup_complete: bool,
    warnings: Vec<String>,
}

impl FileTransaction {
    pub(crate) fn begin(home: &Path, plan: &ReconcilePlan) -> Result<Self, GhError> {
        let mut paths = plan
            .writes
            .iter()
            .map(|write| &write.path)
            .chain(&plan.remove_paths)
            .chain(&plan.owned_paths)
            .cloned()
            .collect::<Vec<_>>();
        paths.extend(package_state_paths(home));
        Self::begin_paths_and_plan(home, paths, Some(plan))
    }

    pub(crate) fn begin_paths(home: &Path, paths: Vec<PathBuf>) -> Result<Self, GhError> {
        Self::begin_paths_and_plan(home, paths, None)
    }

    fn begin_paths_and_plan(
        home: &Path,
        mut paths: Vec<PathBuf>,
        plan: Option<&ReconcilePlan>,
    ) -> Result<Self, GhError> {
        let (transaction_lock, outermost) = TransactionLock::acquire(home)?;
        if outermost {
            // Load-bearing ordering: recovery runs before the snapshot below, so
            // leftovers retained by a failed cleanup are cleared before this
            // transaction backs anything up.
            recover_incomplete_transactions(home)?;
        }
        for path in &paths {
            validate_home_path(home, path, true)?;
        }
        paths.sort();
        paths.dedup();
        let mut roots = Vec::<PathBuf>::new();
        for path in paths {
            if !roots.iter().any(|root| path.starts_with(root)) {
                roots.push(path);
            }
        }

        let transactions = home.join(".blue-transactions");
        validate_home_path(home, &transactions, false)?;
        create_owner_only_ancestors(home, &transactions)?;
        let (id, root) = create_transaction_root(&transactions)?;
        let backup = root.join("backup");
        gh_common::create_owner_only_dir(&backup)?;

        let mut targets = Vec::new();
        for (index, path) in roots.into_iter().enumerate() {
            let existed = std::fs::symlink_metadata(&path).is_ok();
            let backup_path = backup.join(index.to_string());
            if existed {
                copy_tree(&path, &backup_path)?;
                sync_tree(&backup_path)?;
            }
            targets.push(JournalTarget {
                path,
                existed,
                backup: backup_path,
            });
        }
        sync_tree(&backup)?;

        let parent_id = ACTIVE_IDS.with(|ids| ids.borrow().last().cloned());
        let (staging_for_write, staging_directories, trash_paths, created_directories) = match plan
        {
            Some(plan) => operation_paths(home, &root, plan)?,
            None => (Vec::new(), Vec::new(), Vec::new(), Vec::new()),
        };
        let journal = TransactionJournal {
            schema_version: 2,
            id: id.clone(),
            parent_id,
            phase: TransactionPhase::Prepared,
            targets,
            staging_directories,
            trash_paths,
            created_directories,
        };
        persist_journal(&root, &journal)?;
        ACTIVE_IDS.with(|ids| ids.borrow_mut().push(id.clone()));
        crash_point("journal_persisted");

        Ok(Self {
            home: home.to_path_buf(),
            id,
            root,
            journal,
            staging_for_write,
            prepared: Vec::new(),
            _lock: transaction_lock,
            committed: false,
            cleanup_complete: false,
            warnings: Vec::new(),
        })
    }

    pub(crate) fn apply(&mut self, plan: &ReconcilePlan) -> Result<(), GhError> {
        self.apply_with_fault(plan, None)
    }

    pub(crate) fn apply_with_fault(
        &mut self,
        plan: &ReconcilePlan,
        fail_after: Option<usize>,
    ) -> Result<(), GhError> {
        if !self.prepared.is_empty() {
            return Err(GhError::config(
                "filesystem transaction was already applied",
            ));
        }
        validate_planned_modes(plan)?;

        for directory in &self.journal.created_directories {
            create_owner_only_directory(directory)?;
        }
        for (index, directory) in self.journal.staging_directories.iter().enumerate() {
            gh_common::create_owner_only_dir(directory)?;
            sync_directory(directory.parent().expect("staging directory has parent"))?;
            if index == 0 {
                crash_point("staging_directory_created");
            }
        }
        for (index, write) in plan.writes.iter().enumerate() {
            let staging = self
                .staging_for_write
                .get(index)
                .ok_or_else(|| GhError::config("transaction staging plan was missing"))?;
            self.prepared.push(Some(gh_common::prepare_atomic_in(
                &write.path,
                staging,
                &write.body,
            )?));
            if index == 0 {
                crash_point("first_prepared_file");
            }
        }

        self.journal.phase = TransactionPhase::Applying;
        persist_journal(&self.root, &self.journal)?;
        let mut completed = 0usize;
        let mut trashed = BTreeSet::new();
        for (index, path) in plan.remove_paths.iter().enumerate() {
            if std::fs::symlink_metadata(path).is_ok() {
                let trash = &self.journal.trash_paths[index];
                ensure_unused(trash)?;
                std::fs::rename(path, trash).map_err(|source| GhError::Io {
                    path: path.clone(),
                    source,
                })?;
                sync_directory(path.parent().expect("managed path has parent"))?;
                trashed.insert(path.clone());
            }
            completed += 1;
            if fail_after == Some(completed) {
                return Err(GhError::other("injected reconciliation commit fault"));
            }
        }
        let write_trash_offset = plan.remove_paths.len();
        for (index, write) in plan.writes.iter().enumerate() {
            create_owner_only_ancestors(
                &self.home,
                write
                    .path
                    .parent()
                    .ok_or_else(|| GhError::config("managed write has no parent directory"))?,
            )?;
            if std::fs::symlink_metadata(&write.path).is_ok() && !trashed.contains(&write.path) {
                let trash = &self.journal.trash_paths[write_trash_offset + index];
                ensure_unused(trash)?;
                std::fs::rename(&write.path, trash).map_err(|source| GhError::Io {
                    path: write.path.clone(),
                    source,
                })?;
            }
            self.prepared[index]
                .take()
                .ok_or_else(|| GhError::config("prepared filesystem write was missing"))?
                .commit()?;
            if index == 0 {
                crash_point("first_target_replacement");
            }
            completed += 1;
            if fail_after == Some(completed) {
                return Err(GhError::other("injected reconciliation commit fault"));
            }
        }
        Ok(())
    }

    pub(crate) fn commit(&mut self) -> Result<(), GhError> {
        self.commit_inner(false)
    }

    fn commit_inner(&mut self, inject_cleanup_fault: bool) -> Result<(), GhError> {
        // The journal persist stays fatal: `committed` is only set afterwards,
        // so a failure here still lets `Drop` roll the transaction back.
        self.journal.phase = TransactionPhase::Committed;
        persist_journal(&self.root, &self.journal)?;
        self.committed = true;
        crash_point("committed_journal_persisted");
        // Past this point the apply has durably landed. Cleanup only deletes
        // staging, trash, and empty created directories, so its failure cannot
        // un-land a target and must not be reported as a failed apply. The
        // journal is retained and startup recovery retries the cleanup.
        let cleanup = if inject_cleanup_fault {
            Err(GhError::other("injected transaction cleanup fault"))
        } else {
            cleanup_after_transaction(&self.journal)
                .and_then(|()| remove_transaction_root(&self.root))
        };
        match cleanup {
            Ok(()) => self.cleanup_complete = true,
            Err(error) => {
                tracing::warn!(%error, journal = %self.root.display(), "committed transaction cleanup failed; journal retained for startup recovery");
                self.warnings.push(format!(
                    "configuration was written, but cleaning up transaction leftovers at {} failed: {error}",
                    self.root.display()
                ));
            }
        }
        Ok(())
    }

    /// Non-fatal problems observed while committing. Callers with a reconcile
    /// report surface these; a retained trash sibling is a verbatim copy of the
    /// user's previous config, so it must not be silent.
    pub(crate) fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    #[cfg(test)]
    pub(crate) fn commit_with_cleanup_fault(&mut self) -> Result<(), GhError> {
        self.commit_inner(true)
    }

    fn rollback(&self) -> Result<(), GhError> {
        restore_targets(&self.home, &self.journal.targets)?;
        cleanup_after_transaction(&self.journal)?;
        Ok(())
    }
}

impl Drop for FileTransaction {
    fn drop(&mut self) {
        if !self.committed {
            if let Err(error) = self.rollback() {
                tracing::error!(%error, backup = %self.root.join("backup").display(), "reconciliation rollback failed; backup retained");
            } else if let Err(error) = remove_transaction_root(&self.root) {
                tracing::error!(%error, journal = %self.root.display(), "removing completed rollback journal failed; journal retained");
            }
        } else if !self.cleanup_complete {
            tracing::warn!(journal = %self.root.display(), "committed transaction cleanup incomplete; journal retained for startup recovery");
        }
        ACTIVE_IDS.with(|ids| {
            let mut ids = ids.borrow_mut();
            if ids.last().is_some_and(|id| id == &self.id) {
                ids.pop();
            } else if let Some(index) = ids.iter().position(|id| id == &self.id) {
                ids.remove(index);
            }
        });
    }
}

fn package_state_paths(home: &Path) -> [PathBuf; 2] {
    [
        home.join(".config/blue/package-state.json"),
        home.join(".config/blue/package-state"),
    ]
}

type OperationPaths = (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>);

fn operation_paths(
    home: &Path,
    root: &Path,
    plan: &ReconcilePlan,
) -> Result<OperationPaths, GhError> {
    let mut staging_by_parent = BTreeMap::<PathBuf, PathBuf>::new();
    let mut staging_for_write = Vec::with_capacity(plan.writes.len());
    for write in &plan.writes {
        let parent = plan
            .remove_paths
            .iter()
            .find(|removed| write.path.starts_with(removed))
            .and_then(|removed| removed.parent())
            .or_else(|| write.path.parent())
            .ok_or_else(|| GhError::config("managed write has no parent directory"))?
            .to_path_buf();
        let next = staging_by_parent.len();
        let staging = staging_by_parent
            .entry(parent.clone())
            .or_insert_with(|| transaction_sibling(&parent.join("staging"), root, "stage", next))
            .clone();
        staging_for_write.push(staging);
    }
    let staging_directories = staging_by_parent.values().cloned().collect::<Vec<_>>();
    let mut trash_paths = plan
        .remove_paths
        .iter()
        .enumerate()
        .map(|(index, path)| transaction_sibling(path, root, "remove", index))
        .collect::<Vec<_>>();
    trash_paths.extend(
        plan.writes
            .iter()
            .enumerate()
            .map(|(index, write)| transaction_sibling(&write.path, root, "write", index)),
    );

    let mut required_parents = staging_directories
        .iter()
        .filter_map(|path| path.parent().map(Path::to_path_buf))
        .chain(
            plan.writes
                .iter()
                .filter_map(|write| write.path.parent().map(Path::to_path_buf)),
        )
        .collect::<Vec<_>>();
    required_parents.sort();
    required_parents.dedup();
    let mut created = BTreeSet::new();
    for parent in required_parents {
        collect_missing_ancestors(home, &parent, &mut created)?;
    }
    let mut created_directories = created.into_iter().collect::<Vec<_>>();
    created_directories.sort_by_key(|path| path.components().count());
    Ok((
        staging_for_write,
        staging_directories,
        trash_paths,
        created_directories,
    ))
}

fn collect_missing_ancestors(
    home: &Path,
    path: &Path,
    output: &mut BTreeSet<PathBuf>,
) -> Result<(), GhError> {
    validate_home_path(home, path, false)?;
    let mut current = home.to_path_buf();
    for component in path
        .strip_prefix(home)
        .expect("validated home path")
        .components()
    {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(GhError::config(format!(
                    "managed parent is not a directory: {}",
                    current.display()
                )))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                output.insert(current.clone());
            }
            Err(source) => {
                return Err(GhError::Io {
                    path: current,
                    source,
                })
            }
        }
    }
    Ok(())
}

fn validate_planned_modes(plan: &ReconcilePlan) -> Result<(), GhError> {
    for write in &plan.writes {
        if write.mode != Some(0o600) {
            return Err(GhError::config(format!(
                "managed file {} requested unsupported mode {:?}; transaction files must use 0600",
                write.path.display(),
                write.mode
            )));
        }
    }
    Ok(())
}

fn create_transaction_root(parent: &Path) -> Result<(String, PathBuf), GhError> {
    use rand::RngCore as _;
    for _ in 0..16 {
        let mut nonce = [0_u8; 16];
        rand::rngs::OsRng
            .try_fill_bytes(&mut nonce)
            .map_err(|error| GhError::other(format!("generating transaction name: {error}")))?;
        let id = hex::encode(nonce);
        let candidate = parent.join(&id);
        match gh_common::create_owner_only_dir(&candidate) {
            Ok(()) => return Ok((id, candidate)),
            Err(GhError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(GhError::other("transaction name attempts exhausted"))
}

fn create_owner_only_ancestors(home: &Path, path: &Path) -> Result<(), GhError> {
    if !home.exists() {
        gh_common::create_owner_only_dir(home)?;
    }
    let mut current = home.to_path_buf();
    for component in path
        .strip_prefix(home)
        .map_err(|_| GhError::config("directory escaped user home"))?
        .components()
    {
        current.push(component.as_os_str());
        create_owner_only_directory(&current)?;
    }
    Ok(())
}

fn create_owner_only_directory(path: &Path) -> Result<(), GhError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(GhError::config(format!(
            "managed parent is not a directory: {}",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            gh_common::create_owner_only_dir(path)?;
            sync_directory(path.parent().expect("created directory has parent"))
        }
        Err(source) => Err(GhError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn ensure_unused(path: &Path) -> Result<(), GhError> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(GhError::config(format!(
            "transaction path already exists: {}",
            path.display()
        ))),
        Err(source) => Err(GhError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn transaction_sibling(path: &Path, root: &Path, kind: &str, index: usize) -> PathBuf {
    let nonce = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("transaction");
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("managed");
    path.with_file_name(format!(".{name}.blue-{kind}-{nonce}-{index}"))
}

fn persist_journal(root: &Path, journal: &TransactionJournal) -> Result<(), GhError> {
    let bytes =
        serde_json::to_vec_pretty(journal).map_err(|error| GhError::Serde(error.to_string()))?;
    gh_common::write_atomic(&root.join("journal.json"), bytes)
}

fn validate_journal(home: &Path, root: &Path, journal: &TransactionJournal) -> Result<(), GhError> {
    if journal.schema_version != 2
        || root.file_name().and_then(|name| name.to_str()) != Some(journal.id.as_str())
    {
        return Err(GhError::config(format!(
            "invalid filesystem transaction journal at {}",
            root.display()
        )));
    }
    let backup_root = root.join("backup");
    for target in &journal.targets {
        validate_home_path(home, &target.path, true)?;
        if !target.backup.starts_with(&backup_root) {
            return Err(GhError::config(
                "transaction backup escaped its journal root",
            ));
        }
    }
    for path in journal
        .staging_directories
        .iter()
        .chain(&journal.trash_paths)
        .chain(&journal.created_directories)
    {
        validate_home_path(home, path, true)?;
    }
    Ok(())
}

fn recover_incomplete_transactions(home: &Path) -> Result<(), GhError> {
    let transactions = home.join(".blue-transactions");
    match std::fs::symlink_metadata(&transactions) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(GhError::config(
                "transaction root is not a regular directory",
            ))
        }
        Err(source) => {
            return Err(GhError::Io {
                path: transactions,
                source,
            })
        }
    }
    let mut journals = Vec::new();
    for entry in std::fs::read_dir(&transactions).map_err(|source| GhError::Io {
        path: transactions.clone(),
        source,
    })? {
        let entry = entry.map_err(|source| GhError::Io {
            path: transactions.clone(),
            source,
        })?;
        let root = entry.path();
        let metadata = entry.file_type().map_err(|source| GhError::Io {
            path: root.clone(),
            source,
        })?;
        if !metadata.is_dir() || metadata.is_symlink() {
            // `.DS_Store`, `desktop.ini`, a cloud-sync artefact, or a Windows
            // junction here is not a transaction. Failing would brick every
            // later `blue apply`; the entry is never opened or followed, and
            // `create_transaction_root` uses create-new semantics over 128-bit
            // random names, so it cannot hijack a future transaction root.
            tracing::warn!(entry = %root.display(), "skipping an entry in the transaction root that is not a transaction directory");
            continue;
        }
        let journal_path = root.join("journal.json");
        let bytes = match std::fs::read(&journal_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::remove_dir_all(&root)
                    .map_err(|source| GhError::Io { path: root, source })?;
                continue;
            }
            Err(source) => {
                return Err(GhError::Io {
                    path: journal_path,
                    source,
                })
            }
        };
        let journal: TransactionJournal = serde_json::from_slice(&bytes).map_err(|error| {
            GhError::config(format!(
                "invalid filesystem transaction journal {}: {error}",
                journal_path.display()
            ))
        })?;
        validate_journal(home, &root, &journal)?;
        journals.push((root, journal));
    }
    while !journals.is_empty() {
        let leaf = journals
            .iter()
            .position(|(_, candidate)| {
                !journals
                    .iter()
                    .any(|(_, other)| other.parent_id.as_deref() == Some(candidate.id.as_str()))
            })
            .ok_or_else(|| GhError::config("filesystem transaction journal cycle"))?;
        let (root, journal) = journals.remove(leaf);
        if journal.phase != TransactionPhase::Committed {
            // The crash-consistency boundary: live paths must be back to their
            // pre-transaction content before anything else runs.
            restore_targets(home, &journal.targets)?;
            cleanup_after_transaction(&journal)?;
            remove_transaction_root(&root)?;
            continue;
        }
        // The apply already landed durably. A cleanup that keeps failing here
        // (EACCES, a file a running harness holds open, NFS ESTALE) must not
        // fail `begin` — that would wedge every later `blue apply` before it
        // does any work at all. Warn and retain the journal for the next run.
        if let Err(error) =
            cleanup_after_transaction(&journal).and_then(|()| remove_transaction_root(&root))
        {
            tracing::warn!(%error, journal = %root.display(), "cleaning up a committed transaction journal failed; retained for a later run");
        }
    }
    Ok(())
}

fn cleanup_after_transaction(journal: &TransactionJournal) -> Result<(), GhError> {
    for path in journal
        .staging_directories
        .iter()
        .chain(&journal.trash_paths)
    {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                std::fs::remove_dir_all(path).map_err(|source| GhError::Io {
                    path: path.clone(),
                    source,
                })?;
            }
            Ok(_) => {
                std::fs::remove_file(path).map_err(|source| GhError::Io {
                    path: path.clone(),
                    source,
                })?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(GhError::Io {
                    path: path.clone(),
                    source,
                })
            }
        }
    }
    let mut directories = journal.created_directories.clone();
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        // Non-recursive removal deliberately preserves any concurrent content.
        match std::fs::remove_dir(&directory) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(source) => {
                return Err(GhError::Io {
                    path: directory,
                    source,
                })
            }
        }
    }
    Ok(())
}

fn remove_transaction_root(root: &Path) -> Result<(), GhError> {
    match std::fs::remove_dir_all(root) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(GhError::Io {
            path: root.to_path_buf(),
            source,
        }),
    }
}

fn restore_targets(home: &Path, targets: &[JournalTarget]) -> Result<(), GhError> {
    for target in targets {
        validate_home_path(home, &target.path, true)?;
        match std::fs::symlink_metadata(&target.path) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                std::fs::remove_dir_all(&target.path).map_err(|source| GhError::Io {
                    path: target.path.clone(),
                    source,
                })?;
            }
            Ok(_) => std::fs::remove_file(&target.path).map_err(|source| GhError::Io {
                path: target.path.clone(),
                source,
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(GhError::Io {
                    path: target.path.clone(),
                    source,
                })
            }
        }
        if target.existed {
            copy_tree(&target.backup, &target.path)?;
            sync_tree(&target.path)?;
        }
    }
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), GhError> {
    let metadata = std::fs::symlink_metadata(source).map_err(|source_error| GhError::Io {
        path: source.to_path_buf(),
        source: source_error,
    })?;
    if metadata.file_type().is_symlink() {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|source| GhError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let target = std::fs::read_link(source).map_err(|source_error| GhError::Io {
            path: source.to_path_buf(),
            source: source_error,
        })?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, destination).map_err(|source| GhError::Io {
            path: destination.to_path_buf(),
            source,
        })?;
        #[cfg(not(unix))]
        return Err(GhError::config(
            "symlink snapshots are unsupported on this platform",
        ));
    } else if metadata.is_dir() {
        std::fs::create_dir_all(destination).map_err(|source| GhError::Io {
            path: destination.to_path_buf(),
            source,
        })?;
        for entry in std::fs::read_dir(source).map_err(|source_error| GhError::Io {
            path: source.to_path_buf(),
            source: source_error,
        })? {
            let entry = entry.map_err(|source_error| GhError::Io {
                path: source.to_path_buf(),
                source: source_error,
            })?;
            copy_tree(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|source| GhError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        std::fs::copy(source, destination).map_err(|source| GhError::Io {
            path: destination.to_path_buf(),
            source,
        })?;
    }
    Ok(())
}

fn sync_tree(path: &Path) -> Result<(), GhError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|source| GhError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path).map_err(|source| GhError::Io {
            path: path.to_path_buf(),
            source,
        })? {
            let entry = entry.map_err(|source| GhError::Io {
                path: path.to_path_buf(),
                source,
            })?;
            sync_tree(&entry.path())?;
        }
        sync_directory(path)?;
    } else {
        #[cfg(unix)]
        std::fs::File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(|source| GhError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        #[cfg(windows)]
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .and_then(|file| file.sync_all())
        {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {}
            Err(source) => {
                return Err(GhError::Io {
                    path: path.to_path_buf(),
                    source,
                })
            }
        }
        #[cfg(not(any(unix, windows)))]
        std::fs::File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(|source| GhError::Io {
                path: path.to_path_buf(),
                source,
            })?;
    }
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), GhError> {
    std::fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| GhError::Io {
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), GhError> {
    Ok(())
}

#[cfg(test)]
fn crash_point(name: &str) {
    let depth = ACTIVE_IDS.with(|ids| ids.borrow().len());
    if std::env::var("BLUE_TRANSACTION_CRASH_AT").ok().as_deref() == Some(name)
        && std::env::var("BLUE_TRANSACTION_CRASH_DEPTH")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(1)
            == depth
    {
        std::process::abort();
    }
}

#[cfg(not(test))]
fn crash_point(_name: &str) {}

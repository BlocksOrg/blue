//! Test-owned state only. Windows uses the real account's Known Folders.
use std::path::{Path, PathBuf};

pub struct TestClientPaths {
    pub profile: PathBuf,
    pub config: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
}

pub struct TestProfile {
    pub paths: TestClientPaths,
    pub scratch: tempfile::TempDir,
    #[cfg(windows)]
    owned: Vec<PathBuf>,
    #[cfg(windows)]
    lock: PathBuf,
    #[cfg(windows)]
    job: NativeJob,
}

impl TestProfile {
    pub fn create() -> Self {
        let scratch = tempfile::tempdir().expect("creating test scratch");
        #[cfg(not(windows))]
        {
            let profile = scratch.path().to_path_buf();
            Self {
                paths: TestClientPaths {
                    config: profile.join(".config/blue"),
                    data: profile.join(".config/blue"),
                    cache: profile.join(".cache/blue"),
                    profile,
                },
                scratch,
            }
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::UI::Shell::{
                FOLDERID_LocalAppData, FOLDERID_Profile, FOLDERID_RoamingAppData,
            };
            assert_eq!(
                std::env::var("E2E_SLIM_DISPOSABLE_ACCOUNT").as_deref(),
                Ok("1"),
                "Windows slim requires an explicitly disposable account"
            );
            let job = NativeJob::create();
            let profile = known_folder(&FOLDERID_Profile);
            let roaming = known_folder(&FOLDERID_RoamingAppData);
            let local = known_folder(&FOLDERID_LocalAppData);
            let lock = local.join("blue-e2e-slim.lock");
            // A directory is an atomic cross-process guard, including nextest retries.
            std::fs::create_dir(&lock).expect("one active Windows Home required; use --test-threads 1 (or inspect a stale lock after a crash)");
            let mut owned = vec![roaming.join("Blue"), local.join("Blue")];
            for relative in [
                ".config/blue",
                ".cache/blue",
                ".codex",
                ".claude",
                ".claude.json",
                ".claude.json.backup",
                ".kimi",
                ".kimi-code",
                ".config/opencode",
                ".local/share/opencode",
                ".local/state/opencode",
                ".cache/opencode",
            ] {
                owned.push(profile.join(relative));
            }
            for root in [&roaming, &local] {
                owned.push(root.join("opencode"));
                owned.push(root.join("kimi"));
            }
            if let Some(existing) = owned.iter().find(|p| p.symlink_metadata().is_ok()) {
                let _ = std::fs::remove_dir(&lock);
                panic!(
                    "fresh Windows account required; refusing existing state {}",
                    existing.display()
                );
            }
            Self {
                paths: TestClientPaths {
                    profile,
                    config: roaming.join("Blue"),
                    data: local.join("Blue/Data"),
                    cache: local.join("Blue/Cache"),
                },
                scratch,
                owned,
                lock,
                job,
            }
        }
    }

    pub fn configure(&self, command: &mut assert_cmd::Command) {
        command
            .env("HOME", &self.paths.profile)
            .env("E2E_SLIM_MARKER_DIR", self.markers());
        #[cfg(not(windows))]
        command
            .env("XDG_CONFIG_HOME", self.paths.config.parent().unwrap())
            .env("XDG_CACHE_HOME", self.paths.cache.parent().unwrap())
            .env("XDG_DATA_HOME", self.paths.profile.join(".local/share"))
            .env("XDG_STATE_HOME", self.paths.profile.join(".local/state"));
        #[cfg(windows)]
        for key in [
            "XDG_CONFIG_HOME",
            "XDG_CACHE_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
        ] {
            command.env_remove(key);
        }
        // Agent-specific overrides must not escape this profile.
        for key in [
            "CODEX_HOME",
            "CLAUDE_CONFIG_DIR",
            "KIMI_HOME",
            "OPENCODE_CONFIG",
            "OPENCODE_CONFIG_DIR",
        ] {
            command.env_remove(key);
        }
    }

    pub fn markers(&self) -> PathBuf {
        self.scratch.path().join("component-markers")
    }
    pub fn scratch(&self) -> &Path {
        self.scratch.path()
    }
}

#[cfg(windows)]
fn known_folder(id: &windows_sys::core::GUID) -> PathBuf {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::{System::Com::CoTaskMemFree, UI::Shell::SHGetKnownFolderPath};
    let mut raw = std::ptr::null_mut();
    let status = unsafe { SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut raw) };
    assert!(
        status >= 0 && !raw.is_null(),
        "Known Folder lookup failed: {status}"
    );
    let mut len = 0;
    unsafe {
        while *raw.add(len) != 0 {
            len += 1;
        }
        let result = PathBuf::from(std::ffi::OsString::from_wide(std::slice::from_raw_parts(
            raw, len,
        )));
        CoTaskMemFree(raw.cast());
        result
    }
}

#[cfg(windows)]
impl Drop for TestProfile {
    fn drop(&mut self) {
        // Windows job membership includes grandchildren even after their parent exits.
        // On timeout preserve the reservation, so another case cannot reuse state.
        if !self.job.wait_empty() {
            eprintln!(
                "native children still running; preserving {}",
                self.lock.display()
            );
            if !std::thread::panicking() {
                panic!("native children did not finish");
            }
            return;
        }
        for path in &self.owned {
            let Ok(meta) = path.symlink_metadata() else {
                continue;
            };
            let result = if meta.is_dir() && !meta.file_type().is_symlink() {
                std::fs::remove_dir_all(path)
            } else {
                std::fs::remove_file(path)
            };
            if let Err(error) = result {
                eprintln!(
                    "cannot clean {}: {error}; keeping reservation",
                    path.display()
                );
                if !std::thread::panicking() {
                    panic!("native state cleanup failed");
                }
                return;
            }
        }
        std::fs::remove_dir(&self.lock).expect("releasing native profile");
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;
    #[test]
    fn unix_profiles_are_independent_and_removed() {
        let first = TestProfile::create();
        let second = TestProfile::create();
        assert_ne!(first.paths.profile, second.paths.profile);
        assert_eq!(first.paths.config, first.paths.data);
        assert_eq!(first.paths.cache, first.paths.profile.join(".cache/blue"));
        let root = first.paths.profile.clone();
        drop(first);
        assert!(!root.exists());
        assert!(second.paths.profile.exists());
    }
}

#[cfg(windows)]
struct NativeJob(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
impl NativeJob {
    fn create() -> Self {
        use windows_sys::Win32::System::{
            JobObjects::{AssignProcessToJobObject, CreateJobObjectW},
            Threading::GetCurrentProcess,
        };
        let job = Self(unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) });
        assert!(
            !job.0.is_null(),
            "creating Windows test job: {}",
            std::io::Error::last_os_error()
        );
        assert_ne!(
            unsafe { AssignProcessToJobObject(job.0, GetCurrentProcess()) },
            0,
            "assigning test process to job: {}",
            std::io::Error::last_os_error()
        );
        job
    }
    fn wait_empty(&self) -> bool {
        use windows_sys::Win32::System::JobObjects::{
            JobObjectBasicAccountingInformation, QueryInformationJobObject,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
            let ok = unsafe {
                QueryInformationJobObject(
                    self.0,
                    JobObjectBasicAccountingInformation,
                    (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                    std::mem::size_of_val(&info) as u32,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return false;
            }
            // The nextest test process itself remains in the job until exit.
            if info.ActiveProcesses == 1 {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            eprintln!(
                "waiting for {} native test descendants",
                info.ActiveProcesses - 1
            );
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
}
#[cfg(windows)]
impl Drop for NativeJob {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    #[test]
    fn sequential_native_profiles_remove_owned_state() {
        if std::env::var("E2E_SLIM_DISPOSABLE_ACCOUNT").as_deref() != Ok("1") {
            assert!(
                !crate::required(),
                "native isolation test requires a disposable account"
            );
            return;
        }
        let first = TestProfile::create();
        let config = first.paths.config.clone();
        let codex = first.paths.profile.join(".codex");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::write(codex.join("test-marker"), "first").unwrap();
        drop(first);
        assert!(!config.exists());
        assert!(!codex.exists());
        let second = TestProfile::create();
        assert_eq!(second.paths.config, config);
        assert_ne!(second.paths.config, second.paths.data);
        assert!(second.paths.profile.is_dir());
        drop(second);
    }
}

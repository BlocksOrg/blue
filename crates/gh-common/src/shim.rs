//! The PATH shims `blue shim install` writes, and the matching recognisers.
//!
//! Rendering and validation live together, and in a crate both `gh-cli` and
//! `gh-harness` depend on, because they have to agree exactly: the CLI decides
//! whether a file at a shim path is safe to replace or remove, and detection
//! decides whether a PATH entry is Blue shadowing the real binary. A marker
//! bump that only reached one of them would silently break the other.

use std::path::{Path, PathBuf};

use crate::error::GhError;
use crate::harness::Harness;

/// The marker written by the current shim format.
pub const SHIM_MARKER: &str = "Blue command shim v1";

/// The marker written before the format was versioned. Unix-only — Windows
/// shims did not exist before the current format.
const LEGACY_SHIM_MARKER: &str = "# blue shim";

/// Where the shim for `harness` lives inside `dir`.
pub fn shim_path(dir: &Path, harness: Harness) -> PathBuf {
    #[cfg(windows)]
    {
        dir.join(format!("{}.cmd", harness.key()))
    }
    #[cfg(not(windows))]
    {
        dir.join(harness.key())
    }
}

/// Render the shim for `harness` that dispatches through `exe`.
pub fn render_shim(exe: &Path, harness: Harness) -> Result<String, GhError> {
    let executable = exe.to_str().ok_or_else(|| {
        GhError::config(format!(
            "Blue executable path is not valid Unicode: {}",
            exe.display()
        ))
    })?;
    #[cfg(windows)]
    {
        if executable.contains('%') || executable.contains('"') {
            return Err(GhError::config(
                "Blue executable path cannot be represented safely in a cmd shim",
            ));
        }
        Ok(format!(
            "@rem {SHIM_MARKER}\r\n@\"{executable}\" run {} -- %*\r\n",
            harness.key()
        ))
    }
    #[cfg(not(windows))]
    {
        if executable.contains('\n')
            || executable.contains('"')
            || executable.contains('`')
            || executable.contains('$')
            || executable.contains('\\')
        {
            return Err(GhError::config(
                "Blue executable path cannot be represented safely in a shell shim",
            ));
        }
        Ok(format!(
            "#!/usr/bin/env bash\n# {SHIM_MARKER}\nexec \"{executable}\" run {} -- \"$@\"\n",
            harness.key()
        ))
    }
}

/// Whether `contents` is a shim in the current format for `harness`.
pub fn valid_managed_shim(contents: &str, harness: Harness) -> bool {
    #[cfg(windows)]
    {
        let Some(command) = contents.strip_prefix(&format!("@rem {SHIM_MARKER}\r\n@\"")) else {
            return false;
        };
        let Some(executable) = command.strip_suffix(&format!("\" run {} -- %*\r\n", harness.key()))
        else {
            return false;
        };
        !executable.is_empty()
            && Path::new(executable).is_absolute()
            && !executable.contains('"')
            && !executable.contains('%')
    }
    #[cfg(not(windows))]
    {
        let Some(command) =
            contents.strip_prefix(&format!("#!/usr/bin/env bash\n# {SHIM_MARKER}\nexec \""))
        else {
            return false;
        };
        let Some(executable) =
            command.strip_suffix(&format!("\" run {} -- \"$@\"\n", harness.key()))
        else {
            return false;
        };
        !executable.is_empty() && Path::new(executable).is_absolute()
    }
}

/// Whether `contents` is a shim this tool wrote before the format was
/// versioned. Recognised so an upgrade can replace or remove its own earlier
/// output instead of refusing to touch it — never written.
pub fn legacy_managed_shim(contents: &str, harness: Harness) -> bool {
    #[cfg(windows)]
    {
        let _ = (contents, harness);
        false
    }
    #[cfg(not(windows))]
    {
        let Some(command) = contents.strip_prefix(&format!(
            "#!/usr/bin/env bash\n{LEGACY_SHIM_MARKER}\nexec \""
        )) else {
            return false;
        };
        let Some(executable) =
            command.strip_suffix(&format!("\" run {} -- \"$@\"\n", harness.key()))
        else {
            return false;
        };
        !executable.is_empty() && Path::new(executable).is_absolute()
    }
}

/// Whether `contents` is a Blue shim in any format this tool has written.
pub fn managed_shim(contents: &str, harness: Harness) -> bool {
    valid_managed_shim(contents, harness) || legacy_managed_shim(contents, harness)
}

/// Every shim this tool writes is a few hundred bytes of text. A candidate
/// larger than this is a real binary, and can be ruled out on its size rather
/// than by reading it.
pub const MAX_SHIM_BYTES: u64 = 4096;

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn legacy_shim(exe: &str, harness: Harness) -> String {
        format!(
            "#!/usr/bin/env bash\n# blue shim\nexec \"{exe}\" run {} -- \"$@\"\n",
            harness.key()
        )
    }

    #[test]
    fn current_format_round_trips_and_is_harness_specific() {
        #[cfg(windows)]
        let executable = Path::new(r"C:\Program Files\Blue\blue.exe");
        #[cfg(not(windows))]
        let executable = Path::new("/opt/Blue Tools/blue");
        let rendered = render_shim(executable, Harness::Codex).unwrap();
        assert!(valid_managed_shim(&rendered, Harness::Codex));
        assert!(!valid_managed_shim(&rendered, Harness::Claude));
        assert!(!valid_managed_shim(SHIM_MARKER, Harness::Codex));
        assert!(rendered.len() as u64 <= MAX_SHIM_BYTES);
    }

    #[test]
    #[cfg(unix)]
    fn legacy_format_is_recognised_but_never_rendered() {
        let contents = legacy_shim("/usr/local/bin/blue", Harness::Codex);
        assert!(legacy_managed_shim(&contents, Harness::Codex));
        assert!(!legacy_managed_shim(&contents, Harness::Claude));
        assert!(!valid_managed_shim(&contents, Harness::Codex));
        assert!(managed_shim(&contents, Harness::Codex));

        assert!(!legacy_managed_shim(
            &render_shim(Path::new("/usr/local/bin/blue"), Harness::Codex).unwrap(),
            Harness::Codex
        ));
        assert!(!legacy_managed_shim(
            "#!/usr/bin/env bash\nexit 1\n",
            Harness::Codex
        ));
        // A relative interpreter path is somebody else's script, not ours.
        assert!(!legacy_managed_shim(
            &legacy_shim("blue", Harness::Codex),
            Harness::Codex
        ));
    }
}

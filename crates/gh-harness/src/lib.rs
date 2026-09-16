//! `gh-harness` — the harness registry: PATH [`detect`]ion, the policy [`gate`],
//! and transparent [`launch`]. Deliberately dependency-light (only
//! `gh-common`) so it stays a clean, reusable building block.

pub mod detect;
pub mod gate;
pub mod install;
pub mod inventory;
pub mod launch;

pub use detect::{
    detect, detect_all, detect_at, parse_version, upstream_paths, which, which_all, Detected,
};
pub use gate::ensure_allowed;
pub use inventory::{HarnessInventory, HarnessInventoryEntry};
pub use launch::{
    launch_inherited, terminal_size, PtyEvent, PtySession, RawGuard, TerminalModeGuard,
};

pub use install::{detect_installation, InstallMethod, Installation};

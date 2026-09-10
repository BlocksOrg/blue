//! `gh-harness` — the harness registry: PATH [`detect`]ion, the policy [`gate`],
//! and transparent [`launch`]. Deliberately dependency-light (only
//! `gh-common`) so it stays a clean, reusable building block.

pub mod detect;
pub mod gate;
pub mod inventory;
pub mod launch;

pub use detect::{detect, detect_all, detect_at, parse_version, which, which_all, Detected};
pub use gate::ensure_allowed;
pub use inventory::{HarnessInventory, HarnessInventoryEntry};
pub use launch::{
    launch_inherited, terminal_size, PtyEvent, PtySession, RawGuard, TerminalModeGuard,
};

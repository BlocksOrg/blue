//! `gh-common` — shared primitives for the blue client crates:
//! the [`Harness`] enum, XDG [`paths`], [`write_atomic`], the client
//! [`BlueToml`] config, and the shared [`GhError`].
//!
//! This crate has no I/O beyond the filesystem and no knowledge of the
//! service, gateways, or individual harness config formats — those live in the
//! higher-level crates.

pub mod atomic;
pub mod client_config;
pub mod error;
#[cfg(windows)]
pub mod fs_identity;
pub mod harness;
pub mod network;
pub mod paths;

pub use atomic::{
    create_owner_only_dir, create_owner_only_dir_all, prepare_atomic, prepare_atomic_in,
    write_atomic, write_config_atomic, write_owner_only_new, PreparedAtomicWrite,
};
pub use client_config::{BlueToml, IdentityConfig, ModeConfig, ServiceConfig};
pub use error::{GhError, Result};
pub use harness::{harness_registry, ComponentRules, Harness, HarnessMetadata, InstallInvocation};

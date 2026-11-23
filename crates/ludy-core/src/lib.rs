//! Ludy's engine: everything between the server and the Finder.
//!
//! The portable part is [`sealed_client`], [`cache`], [`sealed_drive`],
//! [`keystore`] and [`patch`]: an end-to-end encrypted drive as a
//! filesystem, with no idea how it is shown to the user. The `mount`
//! feature (on by default) adds the macOS side: [`nfs`] serves a drive over
//! loopback NFS, [`mount`] attaches it, and [`engine`] is what the desktop
//! app and the CLI hold.

pub mod cache;
#[cfg(feature = "mount")]
pub mod engine;
pub mod fs;
pub mod http;
pub mod keystore;
#[cfg(feature = "mount")]
pub mod mount;
#[cfg(feature = "mount")]
pub mod nfs;
pub mod patch;
pub mod rangeset;
pub mod sealed_client;
pub mod sealed_drive;
pub mod stats;
pub mod transfer;

pub use ludy_proto::human;

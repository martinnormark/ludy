//! Ludy's engine: everything between the simulated cloud and the Finder.
//!
//! The portable part is [`client`], [`cache`], [`drive`] and [`patch`]: a
//! drive as a filesystem, with no idea how it is shown to the user. The
//! `mount` feature (on by default) adds the macOS side: [`nfs`] serves a
//! drive over loopback NFS, [`mount`] attaches it, and [`engine`] is what
//! the desktop app and the CLI hold.

pub mod cache;
pub mod client;
pub mod drive;
#[cfg(feature = "mount")]
pub mod engine;
#[cfg(feature = "mount")]
pub mod mount;
#[cfg(feature = "mount")]
pub mod nfs;
pub mod patch;
pub mod rangeset;
pub mod stats;

pub use ludy_proto::human;

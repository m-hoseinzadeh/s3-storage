//! On-disk storage engine implementing the `s3s::S3` trait.
//!
//! Objects are stored as raw files inside per-bucket directories under the data
//! root; small JSON sidecars hold per-object metadata/checksums and multipart
//! upload state. No database is involved — the data root is the only state.
//!
//! This module is adapted from the Apache-2.0 licensed `s3s-fs` reference
//! implementation (https://github.com/Nugine/s3s). The public, deployment-facing
//! behaviour (auth, public/private access, custom-domain routing) is layered on
//! top in the crate's `access`, `host`, and `config` modules.

#[macro_use]
mod error;

mod checksum;
mod fs;
mod s3;
mod utils;

pub use self::fs::FileSystem;

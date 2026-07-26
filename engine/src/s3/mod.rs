//! S3-compatible object storage, for shipping backups off the machine that
//! made them.
//!
//! A backup that only exists on the laptop that took it is one failure away
//! from not existing. This is the part that makes a second copy real.
//!
//! # What is verified, and what is not
//!
//! The signing is checked against the published AWS SigV4 test vector, and the
//! client is exercised end to end against MinIO in the integration suite —
//! upload, read back, list, delete, and multipart. **AWS itself, Cloudflare R2,
//! Backblaze B2 and Wasabi are not exercised by any test here.** They speak the
//! same protocol and are expected to work, but "expected to work" is exactly
//! the kind of claim this project tries not to make quietly, so it is written
//! down instead.

pub mod client;
pub mod sign;
pub mod xml;

pub use client::{ObjectInfo, S3Client, S3Config, S3Error};

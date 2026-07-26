//! Encryption at rest for backup artifacts.
//!
//! A dump is the most concentrated copy of a database that exists: every row,
//! in plaintext, in one file, often on a laptop. Encrypting it is the whole
//! point of taking it seriously.
//!
//! # Shape
//!
//! One X25519 identity per installation, generated on first use. The secret
//! half lives in the OS keychain and never touches the store or a manifest; the
//! public half is recorded so the UI can show it and a manifest can name the
//! recipients an artifact was encrypted to.
//!
//! Additional recipients can be added, which is what lets a team decrypt each
//! other's backups — or a break-glass key held offline decrypt anything.
//!
//! # The failure mode that matters
//!
//! An encrypted backup you cannot decrypt is strictly worse than no backup: it
//! consumes disk, passes every integrity check, and fails only on the day it is
//! needed. The keychain is not a backup of itself — reinstall the OS and it is
//! gone. So [`BackupKey`] is exportable, and the layers above refuse to take a
//! first encrypted backup until the key has been exported at least once.
//!
//! # Ordering
//!
//! Compress, then encrypt. Ciphertext does not compress, so the other order
//! silently produces much larger artifacts.

use std::io::{Read, Write};

use age::x25519;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use specta::Type;

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("{0} is not a valid age recipient (public keys look like `age1...`)")]
    BadRecipient(String),
    #[error("the stored backup key is not a valid age identity")]
    BadIdentity,
    #[error(
        "no backup key is configured; generate one in Settings before taking an encrypted backup"
    )]
    NoIdentity,
    #[error("encryption failed: {0}")]
    Encrypt(String),
    #[error(
        "this artifact cannot be decrypted with the backup key on this machine. It was encrypted \
         to: {recipients}"
    )]
    NoMatchingIdentity { recipients: String },
    #[error("decryption failed: {0}")]
    Decrypt(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// The public half of an installation's backup key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct BackupKey {
    /// `age1...`. Safe to display, log and store.
    pub public: String,
    /// Whether the user has confirmed they have a copy of the secret half.
    pub exported: bool,
}

/// Generate a fresh identity, returning (secret, public).
///
/// The secret is returned rather than stored here so the caller decides where
/// it goes — in practice the OS keychain, never the application database.
pub fn generate_identity() -> (SecretString, String) {
    let identity = x25519::Identity::generate();
    let public = identity.to_public().to_string();
    (
        SecretString::from(identity.to_string().expose_secret().to_owned()),
        public,
    )
}

/// Check that a string is a usable age public key.
///
/// Validated where the user types it, not when a backup runs at 03:00.
pub fn parse_recipient(raw: &str) -> Result<x25519::Recipient, CryptoError> {
    raw.trim()
        .parse::<x25519::Recipient>()
        .map_err(|_| CryptoError::BadRecipient(raw.to_string()))
}

/// Derive the public key from a stored secret, so the two can be checked
/// against each other rather than trusted to have stayed in step.
pub fn public_from_identity(secret: &SecretString) -> Result<String, CryptoError> {
    let identity: x25519::Identity = secret
        .expose_secret()
        .parse()
        .map_err(|_| CryptoError::BadIdentity)?;
    Ok(identity.to_public().to_string())
}

/// Everything an artifact needs to be decryptable later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type, Default)]
pub struct EncryptionInfo {
    /// Public keys this artifact was encrypted to.
    ///
    /// Recorded so a restore on another machine can say *which* key is needed
    /// rather than only that decryption failed.
    pub recipients: Vec<String>,
}

/// Wrap a writer so everything written to it is encrypted.
///
/// The returned writer **must** be finished with [`finish_encryption`]: age's
/// STREAM format ends with a final authenticated chunk, and a stream that is
/// merely dropped produces a file that decrypts to a truncation error.
pub fn encrypting_writer<W: Write>(
    inner: W,
    recipients: &[String],
) -> Result<age::stream::StreamWriter<W>, CryptoError> {
    if recipients.is_empty() {
        return Err(CryptoError::NoIdentity);
    }

    let parsed: Vec<x25519::Recipient> = recipients
        .iter()
        .map(|r| parse_recipient(r))
        .collect::<Result<_, _>>()?;

    let encryptor = age::Encryptor::with_recipients(parsed.iter().map(|r| r as _))
        .map_err(|e| CryptoError::Encrypt(e.to_string()))?;

    encryptor
        .wrap_output(inner)
        .map_err(|e| CryptoError::Encrypt(e.to_string()))
}

/// Close an encrypted stream, returning the underlying writer.
pub fn finish_encryption<W: Write>(writer: age::stream::StreamWriter<W>) -> Result<W, CryptoError> {
    writer
        .finish()
        .map_err(|e| CryptoError::Encrypt(e.to_string()))
}

/// Wrap a reader so everything read from it is decrypted.
pub fn decrypting_reader<'a, R: Read + 'a>(
    inner: R,
    secret: &SecretString,
) -> Result<Box<dyn Read + 'a>, CryptoError> {
    let identity: x25519::Identity = secret
        .expose_secret()
        .parse()
        .map_err(|_| CryptoError::BadIdentity)?;

    let decryptor = age::Decryptor::new(inner).map_err(|e| CryptoError::Decrypt(e.to_string()))?;

    let reader = decryptor
        .decrypt(std::iter::once(&identity as &dyn age::Identity))
        .map_err(|e| match e {
            age::DecryptError::NoMatchingKeys => CryptoError::NoMatchingIdentity {
                recipients: "(recorded in the manifest)".into(),
            },
            other => CryptoError::Decrypt(other.to_string()),
        })?;

    Ok(Box::new(reader))
}

/// Does this file start with an age header?
///
/// Used to catch the mismatch between a manifest that says "encrypted" and a
/// file that is not, either way round — a wrong answer here means either
/// feeding ciphertext to `mysql` or writing plaintext where the user was told
/// there would be none.
pub fn looks_encrypted(path: &std::path::Path) -> bool {
    const MAGIC: &[u8] = b"age-encryption.org/";
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; MAGIC.len()];
    match f.read_exact(&mut buf) {
        Ok(()) => buf == MAGIC,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn roundtrip(plaintext: &[u8], recipients: &[String], secret: &SecretString) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = encrypting_writer(&mut buf, recipients).unwrap();
            w.write_all(plaintext).unwrap();
            finish_encryption(w).unwrap();
        }

        let mut out = Vec::new();
        let mut r = decrypting_reader(Cursor::new(buf), secret).unwrap();
        r.read_to_end(&mut out).unwrap();
        out
    }

    #[test]
    fn an_identity_round_trips_its_own_data() {
        let (secret, public) = generate_identity();
        let data = b"CREATE TABLE users (id INT);";
        assert_eq!(roundtrip(data, &[public], &secret), data);
    }

    #[test]
    fn the_public_key_is_derivable_from_the_secret() {
        // The store and the keychain hold the two halves separately; being able
        // to re-derive means a mismatch is detectable rather than latent.
        let (secret, public) = generate_identity();
        assert_eq!(public_from_identity(&secret).unwrap(), public);
    }

    #[test]
    fn another_key_cannot_read_it() {
        let (_secret_a, public_a) = generate_identity();
        let (secret_b, _public_b) = generate_identity();

        let mut buf = Vec::new();
        let mut w = encrypting_writer(&mut buf, &[public_a]).unwrap();
        w.write_all(b"secret rows").unwrap();
        finish_encryption(w).unwrap();

        // `match` rather than `unwrap_err`: the Ok side is a boxed reader,
        // which has no Debug impl to print.
        match decrypting_reader(Cursor::new(buf), &secret_b) {
            Err(CryptoError::NoMatchingIdentity { .. }) => {}
            Err(other) => panic!("expected NoMatchingIdentity, got {other}"),
            Ok(_) => panic!("another key must not be able to decrypt this"),
        }
    }

    #[test]
    fn several_recipients_can_each_decrypt() {
        // What makes a team key useful: one artifact, several people.
        let (secret_a, public_a) = generate_identity();
        let (secret_b, public_b) = generate_identity();
        let recipients = vec![public_a, public_b];

        let data = b"shared backup";
        assert_eq!(roundtrip(data, &recipients, &secret_a), data);
        assert_eq!(roundtrip(data, &recipients, &secret_b), data);
    }

    #[test]
    fn encrypting_to_nobody_is_refused() {
        // Silently writing plaintext because the recipient list was empty is
        // the worst possible outcome for a feature called "encryption".
        match encrypting_writer(Vec::new(), &[]) {
            Err(CryptoError::NoIdentity) => {}
            Err(other) => panic!("expected NoIdentity, got {other}"),
            Ok(_) => panic!("encrypting to an empty recipient list must be refused"),
        }
    }

    #[test]
    fn a_malformed_recipient_is_rejected_where_it_is_typed() {
        assert!(parse_recipient("not-a-key").is_err());
        assert!(parse_recipient("age1nonsense").is_err());

        let (_s, public) = generate_identity();
        assert!(parse_recipient(&public).is_ok());
        // Pasted keys pick up whitespace.
        assert!(parse_recipient(&format!("  {public}\n")).is_ok());
    }

    #[test]
    fn a_corrupt_identity_is_reported_not_ignored() {
        let bad = SecretString::from("AGE-SECRET-KEY-1NONSENSE".to_string());
        assert!(matches!(
            public_from_identity(&bad),
            Err(CryptoError::BadIdentity)
        ));
    }

    #[test]
    fn a_truncated_stream_does_not_decrypt_to_partial_data() {
        // The property that makes this safe for backups: an artifact cut short
        // by a full disk must fail loudly, not restore half a database.
        let (secret, public) = generate_identity();

        let mut buf = Vec::new();
        let mut w = encrypting_writer(&mut buf, &[public]).unwrap();
        w.write_all(&vec![b'x'; 200_000]).unwrap();
        finish_encryption(w).unwrap();

        buf.truncate(buf.len() / 2);

        let mut out = Vec::new();
        let result = decrypting_reader(Cursor::new(buf), &secret)
            .and_then(|mut r| r.read_to_end(&mut out).map_err(CryptoError::Io));
        assert!(result.is_err(), "a truncated artifact must not read as ok");
    }

    #[test]
    fn large_payloads_cross_the_stream_chunk_boundary() {
        // age chunks at 64 KiB; anything smaller would not exercise the STREAM
        // construction at all, which is the part worth trusting.
        let (secret, public) = generate_identity();
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(roundtrip(&data, &[public], &secret), data);
    }

    #[test]
    fn empty_input_round_trips() {
        let (secret, public) = generate_identity();
        assert_eq!(roundtrip(b"", &[public], &secret), b"");
    }

    #[test]
    fn an_encrypted_file_is_recognisable_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let (_secret, public) = generate_identity();

        let enc = dir.path().join("a.age");
        let mut w = encrypting_writer(std::fs::File::create(&enc).unwrap(), &[public]).unwrap();
        w.write_all(b"rows").unwrap();
        finish_encryption(w).unwrap();
        assert!(looks_encrypted(&enc));

        let plain = dir.path().join("b.sql");
        std::fs::write(&plain, b"CREATE TABLE t (id INT);").unwrap();
        assert!(!looks_encrypted(&plain));

        assert!(!looks_encrypted(&dir.path().join("missing")));
    }

    #[test]
    fn ciphertext_does_not_resemble_the_plaintext() {
        let (_secret, public) = generate_identity();
        let secret_row = b"jane.doe@example.com";

        let mut buf = Vec::new();
        let mut w = encrypting_writer(&mut buf, &[public]).unwrap();
        w.write_all(secret_row).unwrap();
        finish_encryption(w).unwrap();

        assert!(
            !buf.windows(secret_row.len()).any(|w| w == secret_row),
            "plaintext survived into the artifact"
        );
    }
}

// ── Artifact sinks ──────────────────────────────────────────────────────

/// The write end of a backup pipeline: compression, then optional encryption.
///
/// An enum rather than `Box<dyn Write>` so the compressor stays monomorphic on
/// the hot path, and — more importantly — so *finishing* is type-checked. Both
/// layers have a terminal step that writes trailing bytes: gzip its CRC and
/// length, age its final authenticated chunk. A stream that is merely dropped
/// produces a file that looks complete and is not, which is the single most
/// dangerous way for a backup tool to fail.
pub enum ArtifactSink<W: Write> {
    Gz(flate2::write::GzEncoder<W>),
    GzEncrypted(flate2::write::GzEncoder<age::stream::StreamWriter<W>>),
}

impl<W: Write> ArtifactSink<W> {
    /// Compress into `inner`, encrypting too when `recipients` is non-empty.
    pub fn new(inner: W, recipients: &[String]) -> Result<Self, CryptoError> {
        let level = flate2::Compression::default();
        if recipients.is_empty() {
            Ok(ArtifactSink::Gz(flate2::write::GzEncoder::new(
                inner, level,
            )))
        } else {
            // Compress first, encrypt second. Ciphertext is incompressible, so
            // the other order would quietly produce far larger artifacts.
            let encrypted = encrypting_writer(inner, recipients)?;
            Ok(ArtifactSink::GzEncrypted(flate2::write::GzEncoder::new(
                encrypted, level,
            )))
        }
    }

    /// Flush every layer in order and hand back the underlying writer.
    pub fn finish(self) -> Result<W, CryptoError> {
        match self {
            ArtifactSink::Gz(gz) => Ok(gz.finish()?),
            ArtifactSink::GzEncrypted(gz) => finish_encryption(gz.finish()?),
        }
    }

    pub const fn is_encrypted(&self) -> bool {
        matches!(self, ArtifactSink::GzEncrypted(_))
    }
}

impl<W: Write> Write for ArtifactSink<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            ArtifactSink::Gz(w) => w.write(buf),
            ArtifactSink::GzEncrypted(w) => w.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            ArtifactSink::Gz(w) => w.flush(),
            ArtifactSink::GzEncrypted(w) => w.flush(),
        }
    }
}

/// The matching read end: optional decryption, then decompression.
pub fn artifact_reader<'a, R: Read + 'a>(
    inner: R,
    identity: Option<&SecretString>,
) -> Result<Box<dyn Read + 'a>, CryptoError> {
    match identity {
        Some(secret) => Ok(Box::new(flate2::read::GzDecoder::new(decrypting_reader(
            inner, secret,
        )?))),
        None => Ok(Box::new(flate2::read::GzDecoder::new(inner))),
    }
}

#[cfg(test)]
mod sink_tests {
    use super::*;

    fn round_trip(recipients: &[String], secret: Option<&SecretString>, data: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut sink = ArtifactSink::new(&mut buf, recipients).unwrap();
            sink.write_all(data).unwrap();
            sink.finish().unwrap();
        }
        let mut out = Vec::new();
        artifact_reader(std::io::Cursor::new(buf), secret)
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        out
    }

    #[test]
    fn an_unencrypted_sink_is_plain_gzip() {
        let data = b"CREATE TABLE t (id INT);";
        assert_eq!(round_trip(&[], None, data), data);
    }

    #[test]
    fn an_encrypted_sink_round_trips_through_both_layers() {
        let (secret, public) = generate_identity();
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 97) as u8).collect();
        assert_eq!(round_trip(&[public], Some(&secret), &data), data);
    }

    #[test]
    fn compression_happens_before_encryption() {
        // Order matters for size, and it is invisible if you get it wrong —
        // the artifact still restores, it is just far bigger than it should be.
        let (_secret, public) = generate_identity();
        let repetitive = vec![b'a'; 100_000];

        let mut plain = Vec::new();
        let mut s = ArtifactSink::new(&mut plain, &[]).unwrap();
        s.write_all(&repetitive).unwrap();
        s.finish().unwrap();

        let mut encrypted = Vec::new();
        let mut s = ArtifactSink::new(&mut encrypted, &[public]).unwrap();
        s.write_all(&repetitive).unwrap();
        s.finish().unwrap();

        assert!(
            plain.len() < 2_000,
            "gzip should crush this: {}",
            plain.len()
        );
        assert!(
            encrypted.len() < plain.len() + 1_000,
            "encrypting compressed data should add only a header and tag, got {} vs {}",
            encrypted.len(),
            plain.len()
        );
    }

    #[test]
    fn a_sink_reports_whether_it_encrypts() {
        let (_s, public) = generate_identity();
        assert!(!ArtifactSink::new(Vec::new(), &[]).unwrap().is_encrypted());
        assert!(
            ArtifactSink::new(Vec::new(), &[public])
                .unwrap()
                .is_encrypted()
        );
    }

    #[test]
    fn reading_an_encrypted_artifact_without_a_key_fails() {
        // Rather than handing gzip ciphertext and getting a confusing error
        // three layers down.
        let (_secret, public) = generate_identity();
        let mut buf = Vec::new();
        let mut sink = ArtifactSink::new(&mut buf, &[public]).unwrap();
        sink.write_all(b"rows").unwrap();
        sink.finish().unwrap();

        let mut out = Vec::new();
        let result = artifact_reader(std::io::Cursor::new(buf), None)
            .unwrap()
            .read_to_end(&mut out);
        assert!(result.is_err(), "gzip must reject an age header");
    }
}

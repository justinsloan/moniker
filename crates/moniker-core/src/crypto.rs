//! Sealed bundles: the encrypted form of a `Vec<BundleEntry>` exchanged during sync.
//!
//! [`seal`] encrypts entries under the user's sync passphrase; [`open`]
//! reverses it. The passphrase never leaves the machine: only the sealed bytes
//! travel over SSH.
//!
//! Format (version 1, every length fixed):
//!
//! ```text
//! magic "MONIKER1" (8) ‖ salt (16) ‖ nonce (24) ‖ XChaCha20-Poly1305 ciphertext ‖ tag (16)
//! ```
//!
//! * The plaintext is the JSON serialization of the entry list.
//! * The key is Argon2id (version 0x13) of the passphrase bytes and the salt,
//!   with m = 64 MiB, t = 3, p = 1 and a 32-byte output. The parameters are
//!   constants tied to the magic and are never read from the blob, so a
//!   tampered blob cannot pick a weaker (or ruinously expensive) KDF.
//! * The magic is the AEAD associated data. Salt and nonce are not authenticated
//!   directly, but changing either changes the key or the keystream, so the tag
//!   check fails all the same.
//! * Salt and nonce come fresh from the OS CSPRNG on every [`seal`].
//!
//! Secret handling (best effort; Rust can move or copy values in ways that
//! cannot be fully controlled):
//!
//! * The derived key lives in a `Zeroizing<[u8; 32]>` and is written in place
//!   by the KDF, never returned by value. Argon2's 64 MiB working memory holds
//!   key-equivalent state, so it is allocated here and zeroized after use.
//! * The plaintext JSON is written straight into one exact-size, zeroizing
//!   buffer (its length is measured first), which is then encrypted in place;
//!   the buffer refuses to grow, so no reallocation can leave a copy behind.
//! * [`open`] decrypts in place in a zeroizing copy of the ciphertext. The
//!   decoded entries zeroize their own passwords when dropped.
//! * Errors carry no data from the passphrase or the plaintext.

use std::io;

use argon2::{Algorithm, Argon2, Block, Params, Version};
use chacha20poly1305::{AeadInOut, Key, KeyInit, Tag, XChaCha20Poly1305, XNonce};
use zeroize::Zeroizing;

use crate::bundle::BundleEntry;

/// Why [`seal`] or [`open`] failed.
///
/// The variants carry no data: nothing derived from the passphrase or the
/// plaintext ever reaches an error message.
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoError {
    /// Authentication failed: the passphrase differs from the sealing one, or
    /// the bytes were altered or truncated. The two cannot be told apart.
    #[error("wrong sync passphrase, or the sealed bundle is corrupt")]
    WrongKeyOrCorrupt,
    /// Not a sealed bundle in a format this build reads, or (after successful
    /// authentication) not a valid entry list.
    #[error("not a Moniker sealed bundle, or a format version this build cannot read")]
    BadFormat,
    /// The passphrase was empty.
    #[error("sync passphrase must not be empty")]
    EmptyPassphrase,
    /// The entries could not be serialized, e.g. a key file path that is not
    /// valid UTF-8.
    #[error("bundle could not be serialized (every key file path must be valid UTF-8)")]
    Serialize,
    /// The passphrase or the bundle exceeds what Argon2 or XChaCha20-Poly1305
    /// accepts (gigabytes; not reachable in practice).
    #[error("passphrase or bundle too large to seal")]
    TooLarge,
    /// The OS random number generator failed while making a salt or nonce.
    #[error("the operating system's random number generator failed")]
    Random,
}

/// Format magic, also the AEAD associated data. Bump it to change anything
/// in the format, the KDF parameters included.
const MAGIC: &[u8; MAGIC_LEN] = b"MONIKER1";
const MAGIC_LEN: usize = 8;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;
const KEY_LEN: usize = 32;
const HEADER_LEN: usize = MAGIC_LEN + SALT_LEN + NONCE_LEN;

/// Argon2id memory cost in KiB (64 MiB), for format `MONIKER1`.
const KDF_M_COST_KIB: u32 = 64 * 1024;
/// Argon2id iterations, for format `MONIKER1`.
const KDF_T_COST: u32 = 3;
/// Argon2id parallelism, for format `MONIKER1`.
const KDF_P_COST: u32 = 1;
/// Checked at compile time.
const KDF_PARAMS: Params = match Params::new(KDF_M_COST_KIB, KDF_T_COST, KDF_P_COST, Some(KEY_LEN))
{
    Ok(params) => params,
    Err(_) => panic!("invalid Argon2 parameters"),
};

/// Encrypt `entries` under `passphrase`.
///
/// Each call uses a fresh random salt and nonce, so sealing the same input
/// twice gives different bytes. Takes a fraction of a second (the KDF).
pub fn seal(passphrase: &str, entries: &[BundleEntry]) -> Result<Vec<u8>, CryptoError> {
    if passphrase.is_empty() {
        return Err(CryptoError::EmptyPassphrase);
    }
    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut salt).map_err(|_| CryptoError::Random)?;
    getrandom::fill(&mut nonce).map_err(|_| CryptoError::Random)?;
    seal_with(passphrase, entries, &salt, &nonce)
}

/// Decrypt a bundle made by [`seal`] with the same passphrase.
///
/// Input that cannot be a sealed bundle (wrong magic, too short) is rejected
/// with [`CryptoError::BadFormat`] before the KDF runs, so garbage is cheap to
/// refuse. The entries are not validated here; callers must still run
/// [`Profile::validate`](crate::profile::Profile::validate) on them.
pub fn open(passphrase: &str, sealed: &[u8]) -> Result<Vec<BundleEntry>, CryptoError> {
    if passphrase.is_empty() {
        return Err(CryptoError::EmptyPassphrase);
    }
    let parts = Parts::parse(sealed).ok_or(CryptoError::BadFormat)?;

    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    derive_key(passphrase, parts.salt, &mut key)?;

    let mut buf = Zeroizing::new(parts.ciphertext.to_vec());
    cipher(&key)
        .decrypt_inout_detached(
            <&XNonce>::from(parts.nonce),
            MAGIC,
            buf.as_mut_slice().into(),
            <&Tag>::from(parts.tag),
        )
        .map_err(|_| CryptoError::WrongKeyOrCorrupt)?;
    serde_json::from_slice(&buf).map_err(|_| CryptoError::BadFormat)
}

/// The fields of a sealed blob, borrowed from it.
struct Parts<'a> {
    salt: &'a [u8; SALT_LEN],
    nonce: &'a [u8; NONCE_LEN],
    ciphertext: &'a [u8],
    tag: &'a [u8; TAG_LEN],
}

impl<'a> Parts<'a> {
    /// Split a sealed blob. `None` if the magic is wrong or the blob is too
    /// short to hold a header and a tag.
    fn parse(sealed: &'a [u8]) -> Option<Parts<'a>> {
        let (magic, rest) = sealed.split_first_chunk::<MAGIC_LEN>()?;
        if magic != MAGIC {
            return None;
        }
        let (salt, rest) = rest.split_first_chunk::<SALT_LEN>()?;
        let (nonce, rest) = rest.split_first_chunk::<NONCE_LEN>()?;
        let (ciphertext, tag) = rest.split_last_chunk::<TAG_LEN>()?;
        Some(Parts {
            salt,
            nonce,
            ciphertext,
            tag,
        })
    }
}

/// [`seal`] with a caller-chosen salt and nonce. Production code must only
/// reach this through [`seal`]; tests use it to pin the format.
fn seal_with(
    passphrase: &str,
    entries: &[BundleEntry],
    salt: &[u8; SALT_LEN],
    nonce: &[u8; NONCE_LEN],
) -> Result<Vec<u8>, CryptoError> {
    if passphrase.is_empty() {
        return Err(CryptoError::EmptyPassphrase);
    }
    // Pass 1: measure the plaintext without storing it.
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, entries).map_err(|_| CryptoError::Serialize)?;
    let plaintext_len = counter.0;
    let capacity = HEADER_LEN
        .checked_add(plaintext_len)
        .and_then(|n| n.checked_add(TAG_LEN))
        .ok_or(CryptoError::TooLarge)?;

    // Pass 2: serialize into an exact-size buffer that cannot reallocate.
    let mut buf = Zeroizing::new(Vec::with_capacity(capacity));
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(salt);
    buf.extend_from_slice(nonce);
    serde_json::to_writer(NoGrow(&mut buf), entries).map_err(|_| CryptoError::Serialize)?;
    if buf.len() != HEADER_LEN + plaintext_len {
        // The two passes disagreed; never happens for derived serializers.
        return Err(CryptoError::Serialize);
    }
    encrypt_sealed(passphrase, salt, nonce, buf)
}

/// Encrypt `buf` (header ‖ plaintext) in place and append the tag, giving the
/// sealed blob. `buf` should have room for the tag so it does not reallocate.
fn encrypt_sealed(
    passphrase: &str,
    salt: &[u8; SALT_LEN],
    nonce: &[u8; NONCE_LEN],
    mut buf: Zeroizing<Vec<u8>>,
) -> Result<Vec<u8>, CryptoError> {
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    derive_key(passphrase, salt, &mut key)?;

    let tag = cipher(&key)
        .encrypt_inout_detached(
            <&XNonce>::from(nonce),
            MAGIC,
            (&mut buf[HEADER_LEN..]).into(),
        )
        .map_err(|_| CryptoError::TooLarge)?;
    // Only ciphertext is in `buf` from here on.
    buf.extend_from_slice(&tag);
    Ok(std::mem::take(&mut *buf))
}

/// Argon2id(passphrase, salt) with the `MONIKER1` parameters, written into
/// `key`. The KDF's working memory is zeroized before it is freed.
fn derive_key(
    passphrase: &str,
    salt: &[u8; SALT_LEN],
    key: &mut Zeroizing<[u8; KEY_LEN]>,
) -> Result<(), CryptoError> {
    #[cfg(test)]
    KDF_RUNS.with(|n| n.set(n.get() + 1));

    let mut memory = Zeroizing::new(vec![Block::new(); KDF_PARAMS.block_count()]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, KDF_PARAMS)
        .hash_password_into_with_memory(
            passphrase.as_bytes(),
            salt,
            key.as_mut_slice(),
            memory.as_mut_slice(),
        )
        .map_err(|_| CryptoError::TooLarge)
}

/// The AEAD keyed with `key`. It holds its own copy of the key, which it
/// zeroizes on drop (`zeroize` feature).
fn cipher(key: &Zeroizing<[u8; KEY_LEN]>) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new(<&Key>::from(&**key))
}

/// A writer that only counts bytes.
struct Counter(usize);

impl io::Write for Counter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(data.len());
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A writer into a `Vec` that fails rather than grow past its capacity, so
/// secret contents are never left behind in a freed allocation.
struct NoGrow<'a>(&'a mut Vec<u8>);

impl io::Write for NoGrow<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.0.capacity() - self.0.len() < data.len() {
            return Err(io::Error::other("plaintext buffer full"));
        }
        self.0.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
thread_local! {
    static KDF_RUNS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use zeroize::Zeroizing;

    use super::*;
    use crate::profile::{Profile, StartupSequence};

    const PASS: &str = "correct horse battery staple";
    const SECRET: &str = "hunter2-very-secret-password";

    // Fixed canonical UUIDs.
    const ID_A: &str = "00000000-0000-4000-8000-00000000000a";
    const ID_B: &str = "00000000-0000-4000-8000-00000000000b";
    const ID_C: &str = "00000000-0000-4000-8000-00000000000c";

    /// A live entry for `id` with fixed fields.
    fn live(id: &str, password: Option<&str>) -> BundleEntry {
        let mut profile = Profile::new("web", "example.com", "alice");
        profile.id = id.to_string();
        profile.updated_at = 1_760_000_000_000;
        assert_eq!(profile.validate(), Ok(()), "test fixture must be valid");
        BundleEntry {
            profile,
            password: password.map(str::to_string),
        }
    }

    /// A tombstone entry for `id`.
    fn tomb(id: &str) -> BundleEntry {
        BundleEntry {
            profile: live(id, None).profile.tombstone(1_760_000_000_500),
            password: None,
        }
    }

    /// A varied entry list: escapes and non-ASCII in the password, a key
    /// file, a startup sequence, an entry without a password, a tombstone.
    fn sample() -> Vec<BundleEntry> {
        let mut rich = live(ID_A, Some("p\"w\\d \u{e9}\u{1f511}"));
        rich.profile.key_file = Some(PathBuf::from("/home/alice/.ssh/id_ed25519"));
        rich.profile.transcript = true;
        rich.profile.sequences.push(StartupSequence {
            name: "update".to_string(),
            commands: vec!["sudo apt update".to_string(), String::new()],
        });
        vec![rich, live(ID_B, None), tomb(ID_C)]
    }

    fn kdf_runs() -> u32 {
        KDF_RUNS.with(std::cell::Cell::get)
    }

    fn reset_kdf_runs() {
        KDF_RUNS.with(|n| n.set(0));
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn unhex(s: &str) -> Vec<u8> {
        assert!(s.len().is_multiple_of(2), "odd-length hex");
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
            .collect()
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn roundtrip() {
        let entries = sample();
        let sealed = seal(PASS, &entries).unwrap();
        assert_eq!(&sealed[..MAGIC_LEN], b"MONIKER1");
        assert_eq!(open(PASS, &sealed).unwrap(), entries);
    }

    #[test]
    fn wrong_passphrase_fails() {
        let sealed = seal(PASS, &sample()).unwrap();
        assert_eq!(
            open("correct horse battery stapler", &sealed),
            Err(CryptoError::WrongKeyOrCorrupt)
        );
    }

    #[test]
    fn flipped_byte_fails() {
        let sealed = seal(PASS, &[live(ID_A, Some(SECRET))]).unwrap();
        let flipped = |at: usize| {
            let mut copy = sealed.clone();
            copy[at] ^= 0x01;
            open(PASS, &copy)
        };
        // Ciphertext body, last tag byte, salt, nonce: authentication fails.
        for (what, at) in [
            ("ciphertext", HEADER_LEN + 3),
            ("tag", sealed.len() - 1),
            ("salt", MAGIC_LEN),
            ("nonce", MAGIC_LEN + SALT_LEN + 5),
        ] {
            assert_eq!(
                flipped(at),
                Err(CryptoError::WrongKeyOrCorrupt),
                "flipped {what} byte at {at}"
            );
        }
        // The magic is checked before decryption.
        assert_eq!(flipped(0), Err(CryptoError::BadFormat));
    }

    #[test]
    fn garbage_is_bad_format() {
        reset_kdf_runs();
        assert_eq!(open(PASS, b"nope"), Err(CryptoError::BadFormat));
        assert_eq!(open(PASS, b""), Err(CryptoError::BadFormat));
        assert_eq!(kdf_runs(), 0, "garbage must be rejected before the KDF");

        let sealed = seal(PASS, &[]).unwrap();
        reset_kdf_runs();
        // Too short to hold a header and a tag.
        for len in [0, MAGIC_LEN, HEADER_LEN, HEADER_LEN + TAG_LEN - 1] {
            assert_eq!(
                open(PASS, &sealed[..len]),
                Err(CryptoError::BadFormat),
                "truncated to {len} bytes"
            );
        }
        assert_eq!(kdf_runs(), 0, "short input must be rejected before the KDF");

        // Long enough to parse but missing ciphertext bytes: the tag no
        // longer matches, which is indistinguishable from corruption.
        assert_eq!(
            open(PASS, &sealed[..sealed.len() - 1]),
            Err(CryptoError::WrongKeyOrCorrupt)
        );
    }

    #[test]
    fn authentic_non_bundle_plaintext_is_bad_format() {
        let salt = [7u8; SALT_LEN];
        let nonce = [9u8; NONCE_LEN];
        for plaintext in [&b"not json"[..], br#"{"profile":1}"#] {
            let mut buf =
                Zeroizing::new(Vec::with_capacity(HEADER_LEN + plaintext.len() + TAG_LEN));
            buf.extend_from_slice(b"MONIKER1");
            buf.extend_from_slice(&salt);
            buf.extend_from_slice(&nonce);
            buf.extend_from_slice(plaintext);
            let sealed = encrypt_sealed(PASS, &salt, &nonce, buf).unwrap();
            assert_eq!(open(PASS, &sealed), Err(CryptoError::BadFormat));
        }
    }

    #[test]
    fn seals_differ_each_time() {
        let entries = [live(ID_A, Some(SECRET))];
        let one = seal(PASS, &entries).unwrap();
        let two = seal(PASS, &entries).unwrap();
        assert_ne!(one, two);
        // Fresh salt and fresh nonce, not just one of them.
        assert_ne!(
            one[MAGIC_LEN..MAGIC_LEN + SALT_LEN],
            two[MAGIC_LEN..MAGIC_LEN + SALT_LEN]
        );
        assert_ne!(
            one[MAGIC_LEN + SALT_LEN..HEADER_LEN],
            two[MAGIC_LEN + SALT_LEN..HEADER_LEN]
        );
    }

    #[test]
    fn password_not_visible_in_sealed_bytes() {
        let sealed = seal(PASS, &[live(ID_A, Some(SECRET))]).unwrap();
        for needle in [SECRET, PASS, "example.com", "alice", "password", ID_A] {
            assert!(
                !contains(&sealed, needle.as_bytes()),
                "{needle:?} visible in sealed bytes"
            );
        }
    }

    #[test]
    fn empty_passphrase_rejected() {
        reset_kdf_runs();
        assert_eq!(
            seal("", &[live(ID_A, Some(SECRET))]),
            Err(CryptoError::EmptyPassphrase)
        );
        // Even a blob with a valid header.
        let mut blob = b"MONIKER1".to_vec();
        blob.resize(HEADER_LEN + TAG_LEN + 2, 0);
        assert_eq!(open("", &blob), Err(CryptoError::EmptyPassphrase));
        assert_eq!(open("", b"nope"), Err(CryptoError::EmptyPassphrase));
        assert_eq!(
            kdf_runs(),
            0,
            "empty passphrase must be rejected before the KDF"
        );
    }

    #[test]
    fn empty_entry_list_roundtrips() {
        let sealed = seal(PASS, &[]).unwrap();
        // "[]" plus the header and tag.
        assert_eq!(sealed.len(), HEADER_LEN + 2 + TAG_LEN);
        assert_eq!(open(PASS, &sealed).unwrap(), Vec::<BundleEntry>::new());
    }

    #[test]
    fn rejects_wrong_magic_without_running_kdf() {
        let mut sealed = seal(PASS, &[live(ID_A, Some(SECRET))]).unwrap();
        for (i, b) in b"MONIKER2".iter().enumerate() {
            sealed[i] = *b;
        }
        reset_kdf_runs();
        // The correct passphrase does not help: the format is not ours.
        assert_eq!(open(PASS, &sealed), Err(CryptoError::BadFormat));
        assert_eq!(kdf_runs(), 0, "wrong magic must be rejected before the KDF");
    }

    #[test]
    fn tombstones_roundtrip_without_password() {
        let entries = vec![tomb(ID_A), tomb(ID_B)];
        let back = open(PASS, &seal(PASS, &entries).unwrap()).unwrap();
        assert_eq!(back, entries);
        assert!(
            back.iter()
                .all(|e| e.profile.deleted && e.password.is_none())
        );
    }

    #[test]
    fn unserializable_key_file_is_serialize_error() {
        use std::os::unix::ffi::OsStrExt;
        let mut entry = live(ID_A, Some(SECRET));
        entry.profile.key_file = Some(PathBuf::from(std::ffi::OsStr::from_bytes(b"/k\xff")));
        reset_kdf_runs();
        assert_eq!(seal(PASS, &[entry]), Err(CryptoError::Serialize));
        assert_eq!(kdf_runs(), 0, "serialization fails before the KDF");
    }

    /// Pins the wire format: magic, layout, KDF parameters, AEAD and the
    /// JSON encoding of `BundleEntry`. If this fails after a change to
    /// `Profile`, sealing changed: bump the magic or make sure the committed
    /// blob still opens (the `open` half must never stop passing).
    #[test]
    fn format_is_stable() {
        // Generated once from this implementation and cross-checked with
        // libsodium (Argon2id t=3/64 MiB/p=1 + XChaCha20-Poly1305-IETF).
        const SEALED: &str = concat!(
            "4d4f4e494b455231000102030405060708090a0b0c0d0e0fa0a1a2a3a4a5a6a7",
            "a8a9aaabacadaeafb0b1b2b3b4b5b6b766498352ef1da65301e15314fcdc52aa",
            "4274545435bc8119c95cf5082fdb946e8626e72f1177f8a1b905fa9c61b31365",
            "0283388f6e5c22eed3ec01a6f79782527771498673e285f1f1e2b39321896235",
            "373fce5be5470bb6c8694b39fd5f843bf954cfe656dcc8a953f88ed2e6a73cbe",
            "3b467cefcc176f4be4609460a21822c111786e0a11eb5ff31439061807a94222",
            "edb4909dc976b543374a527573e492b6c7eec19ce2c58cc10f74f9e0764beb25",
            "2cc0cbade03c18acbcf91a1cee9bb2e31d2eb7fdeaec8dbef38cd462bd733ea6",
            "6d39cd8f3b84eb5d258da77e93776d140e3f9f6dec0f6ad98ea0656c81c24810",
            "6d68fc4836485660dfd63f52247d32052b13784a36f450daca6b61ba346a1d32",
            "2c32cd9ce6fb0b49e02aad78ad9f8839a6ab29f2442c0c98586a7fccf1c66fb8",
            "a59a1aaa84da4810ff4fe006872627fcc97ce403eea204b6897b5c9de7cab120",
            "0b4e22d87c008787b1d4b65f78210b6b9c598d25d8b39684bd4b5ec5b6f86ebc",
            "dc2583de66e10f46283592402b284911730d281167d6bf62279eccf68a7d74d7",
            "e6c49b34ed54872e82d27b370f0c245a0943fc73f4330a55305af09f5b647924",
            "e4b0147fd84d3e1e1318dfa703feec185df48195cc6115be2299674b1f8c6cae",
            "61d4c74a720dd7a36a2b5a595bb29fac36e934b862d3c5a0026ba08ff904114e",
            "61b16e",
        );
        let mut entry = live(ID_A, Some("s3cret"));
        entry.profile.sequences.push(StartupSequence {
            name: "up".to_string(),
            commands: vec!["apt update".to_string()],
        });
        let entries = vec![entry, tomb(ID_B)];
        let salt: [u8; SALT_LEN] = core::array::from_fn(|i| i as u8);
        let nonce: [u8; NONCE_LEN] = core::array::from_fn(|i| 0xa0 + i as u8);

        let sealed = seal_with(PASS, &entries, &salt, &nonce).unwrap();
        assert_eq!(hex(&sealed), SEALED, "sealed output changed");
        assert_eq!(open(PASS, &unhex(SEALED)).unwrap(), entries);
    }
}

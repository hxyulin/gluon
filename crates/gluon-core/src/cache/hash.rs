//! Hash primitives used by the build cache.
//!
//! Three public helpers live here:
//!
//! - [`sha256_bytes`] — a one-line wrapper over [`sha2::Sha256`]. Exposed as
//!   its own function so cache-key derivation sites don't have to import
//!   `sha2` directly and don't have to remember to convert the `GenericArray`
//!   output into a plain `[u8; 32]`.
//! - [`sha256_file`] — streams a file through the hasher in 64 KiB chunks.
//!   Rustc-produced source trees can contain multi-megabyte files (think
//!   `core/src/intrinsics`), and we hash them on the mtime-fallback path in
//!   [`crate::cache::Cache::is_fresh`]. Loading them whole into memory just
//!   to hash would waste both cycles and RSS; the streaming loop keeps the
//!   peak allocation bounded regardless of file size.
//! - [`hash_argv`] — the canonical cache key for a single `rustc`
//!   invocation. This function is the *only* place the gluon.rustc.v1 hash
//!   algorithm is implemented. [`crate::compile::rustc::RustcCommandBuilder::hash`]
//!   delegates here so the digest stays identical across chunk A2 and A3 —
//!   any change to the encoding would silently invalidate every entry in
//!   every existing on-disk cache.

use crate::error::{Error, Result};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Size of the read buffer used by [`sha256_file`]. 64 KiB is a pragmatic
/// sweet spot: large enough that per-syscall overhead is amortised on modern
/// kernels, small enough to keep the buffer off the hot part of the L1 cache
/// on most CPUs. Not tuned empirically — if profiling ever shows this to be
/// a bottleneck, revisit.
const SHA256_CHUNK: usize = 64 * 1024;

/// Thin wrapper over [`sha2::Sha256::digest`] returning a plain `[u8; 32]`.
pub fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Stream `path` through SHA-256 in fixed-size chunks.
///
/// I/O errors (including "file does not exist") are returned as
/// [`Error::Io`] with the offending path attached, so build diagnostics can
/// point the user at the culprit.
pub fn sha256_file(path: &Path) -> Result<[u8; 32]> {
    let mut file = File::open(path).map_err(|e| Error::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; SHA256_CHUNK];
    loop {
        let n = file.read(&mut buf).map_err(|e| Error::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

/// Canonical rustc invocation cache key.
///
/// See the module docs on [`crate::compile::rustc`] for the full spec. The
/// short version: a domain-separated SHA-256 over the rustc path, canonical
/// argv, env (as a `BTreeMap` — iteration order is stable), and optional
/// cwd. Every variable-length field is length-prefixed with
/// [`u64::to_le_bytes`] to defeat concatenation ambiguity, and the cwd is
/// tagged with a single `0`/`1` byte so `Some("")` and `None` hash
/// differently.
///
/// The algorithm is frozen. Do NOT refactor this function without bumping
/// the `gluon.rustc.v1` domain separator — doing so would silently
/// invalidate every existing cache entry and, worse, allow two different
/// encodings to collide if an old cache were reloaded by a newer binary.
pub fn hash_argv(
    rustc: &Path,
    args: &[OsString],
    env: &BTreeMap<OsString, OsString>,
    cwd: Option<&Path>,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"gluon.rustc.v1\0");

    let rustc_bytes = rustc.as_os_str().as_encoded_bytes();
    hasher.update((rustc_bytes.len() as u64).to_le_bytes());
    hasher.update(rustc_bytes);

    hasher.update((args.len() as u64).to_le_bytes());
    for arg in args {
        let bytes = arg.as_encoded_bytes();
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }

    hasher.update((env.len() as u64).to_le_bytes());
    for (k, v) in env {
        let kb = k.as_encoded_bytes();
        let vb = v.as_encoded_bytes();
        hasher.update((kb.len() as u64).to_le_bytes());
        hasher.update(kb);
        hasher.update((vb.len() as u64).to_le_bytes());
        hasher.update(vb);
    }

    match cwd {
        Some(p) => {
            hasher.update([1u8]);
            let bytes = p.as_os_str().as_encoded_bytes();
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        }
        None => {
            hasher.update([0u8]);
        }
    }

    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// RFC 6234 / Wikipedia known answer for SHA-256("abc").
    const ABC_DIGEST_HEX: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn hex(bytes: &[u8; 32]) -> String {
        let mut s = String::with_capacity(64);
        for b in bytes {
            s.push_str(&format!("{:02x}", b));
        }
        s
    }

    #[test]
    fn sha256_bytes_matches_known_vector() {
        assert_eq!(hex(&sha256_bytes(b"abc")), ABC_DIGEST_HEX);
    }

    #[test]
    fn sha256_bytes_empty_matches_known_vector() {
        // Known answer: SHA-256 of the empty string.
        assert_eq!(
            hex(&sha256_bytes(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_file_matches_sha256_bytes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("abc.txt");
        std::fs::write(&p, b"abc").expect("write");
        assert_eq!(sha256_file(&p).expect("hash"), sha256_bytes(b"abc"));
    }

    #[test]
    fn sha256_file_handles_large_file_streaming() {
        // 200 KiB > 64 KiB chunk, exercising the multi-read loop body.
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("big.bin");
        let mut f = File::create(&p).expect("create");
        let chunk = vec![0xABu8; 1024];
        let mut expected = Vec::with_capacity(200 * 1024);
        for _ in 0..200 {
            f.write_all(&chunk).expect("write");
            expected.extend_from_slice(&chunk);
        }
        f.sync_all().expect("sync");
        drop(f);
        assert_eq!(sha256_file(&p).expect("hash"), sha256_bytes(&expected));
    }

    #[test]
    fn sha256_file_missing_path_returns_io_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("does-not-exist");
        match sha256_file(&p) {
            Err(Error::Io { path, .. }) => assert_eq!(path, p),
            other => panic!("expected Error::Io, got {:?}", other),
        }
    }

    #[test]
    fn hash_argv_length_prefix_collision_resistance() {
        // Without length prefixing, ["foo", "bar"] would hash identically
        // to ["foob", "ar"] because the naïve concatenation matches. The
        // u64 prefix is what keeps them apart.
        let env: BTreeMap<OsString, OsString> = BTreeMap::new();
        let a = hash_argv(
            Path::new("/rustc"),
            &[OsString::from("foo"), OsString::from("bar")],
            &env,
            None,
        );
        let b = hash_argv(
            Path::new("/rustc"),
            &[OsString::from("foob"), OsString::from("ar")],
            &env,
            None,
        );
        assert_ne!(a, b);
    }

    #[test]
    fn hash_argv_env_length_prefix_collision_resistance() {
        // Same concatenation-ambiguity check on the env dimension.
        let mut e1: BTreeMap<OsString, OsString> = BTreeMap::new();
        e1.insert(OsString::from("K"), OsString::from("VV"));
        let mut e2: BTreeMap<OsString, OsString> = BTreeMap::new();
        e2.insert(OsString::from("KV"), OsString::from("V"));
        let a = hash_argv(Path::new("/rustc"), &[], &e1, None);
        let b = hash_argv(Path::new("/rustc"), &[], &e2, None);
        assert_ne!(a, b);
    }

    #[test]
    fn hash_argv_stable_for_identical_inputs() {
        let mut env: BTreeMap<OsString, OsString> = BTreeMap::new();
        env.insert(OsString::from("CARGO"), OsString::from("/bin/cargo"));
        let args = vec![OsString::from("--crate-name"), OsString::from("k")];
        let a = hash_argv(Path::new("/rustc"), &args, &env, Some(Path::new("/tmp")));
        let b = hash_argv(Path::new("/rustc"), &args, &env, Some(Path::new("/tmp")));
        assert_eq!(a, b);
    }
}

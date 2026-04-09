//! Content fingerprint over the declared external-dep set.
//!
//! The fingerprint is the single signal `gluon build` uses to decide
//! whether the current `vendor/` state is in sync with what the model
//! declares. If the fingerprint in `gluon.lock` matches
//! [`fingerprint_external_deps`] applied to the live
//! [`BuildModel::external_deps`], the vendor tree is considered fresh
//! and the synthetic `CrateDef`s in the lock are auto-registered into
//! the model. A mismatch forces the user to re-run `gluon vendor`.
//!
//! # What participates in the hash
//!
//! Only fields that affect `cargo vendor`'s *output* participate:
//!
//! - name
//! - source (incl. git url + ref, crates-io version, path)
//! - features + default_features
//!
//! Fields that affect compilation but not vendoring — `cfg_flags`,
//! `rustc_flags` — are **deliberately excluded**. Changing a cfg flag
//! does not invalidate the vendored tree; the compile-time cache
//! (hashed over the full rustc invocation) already catches it.
//!
//! `span` is also excluded — source-position wobble is not a semantic
//! change.
//!
//! # Determinism
//!
//! Iteration is by name via `Arena::names()`, which walks a
//! `BTreeMap<String, _>` — already sorted. Within each entry,
//! `features` is sorted before hashing. The whole thing goes through
//! a domain-separated SHA-256 so future fingerprint schemes can
//! coexist with v1-format lockfiles if we ever need to migrate.

use gluon_model::{Arena, DepSource, ExternalDepDef, GitRef};
use sha2::{Digest, Sha256};

/// Domain tag. Bumping this invalidates every existing `gluon.lock`
/// and forces a re-vendor on the next build. Only change it if the
/// hash algorithm itself changes; adding a new participating field
/// does not require a bump as long as it extends the encoding
/// append-only.
const DOMAIN: &str = "gluon.vendor.fingerprint.v1";

/// Compute the fingerprint for a `BuildModel::external_deps` arena.
///
/// Returns a string of the form `"sha256:<64 hex chars>"`. The empty
/// arena has a well-defined fingerprint (the SHA-256 of just the
/// domain tag + a zero length prefix) — it is never the empty string,
/// so `gluon.lock::fingerprint` is always comparable.
pub fn fingerprint_external_deps(deps: &Arena<ExternalDepDef>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN.as_bytes());
    hasher.update(b"\0");

    // Collect (name, &dep) pairs in sorted order. `Arena::names()`
    // walks the BTreeMap which is already sorted, but we go through a
    // Vec anyway to make the ordering contract local to this
    // function.
    let mut entries: Vec<(&str, &ExternalDepDef)> = deps
        .names()
        .filter_map(|(name, h)| deps.get(h).map(|d| (name, d)))
        .collect();
    entries.sort_by_key(|(name, _)| *name);

    hasher.update((entries.len() as u64).to_le_bytes());
    for (name, dep) in entries {
        // Name
        write_len_prefixed(&mut hasher, name.as_bytes());

        // Source — tagged with a single byte so CratesIo("1.0") and
        // Path("1.0") can never collide.
        match &dep.source {
            DepSource::CratesIo { version } => {
                hasher.update([0u8]);
                write_len_prefixed(&mut hasher, version.as_bytes());
            }
            DepSource::Git { url, reference } => {
                hasher.update([1u8]);
                write_len_prefixed(&mut hasher, url.as_bytes());
                match reference {
                    GitRef::Rev(s) => {
                        hasher.update([0u8]);
                        write_len_prefixed(&mut hasher, s.as_bytes());
                    }
                    GitRef::Tag(s) => {
                        hasher.update([1u8]);
                        write_len_prefixed(&mut hasher, s.as_bytes());
                    }
                    GitRef::Branch(s) => {
                        hasher.update([2u8]);
                        write_len_prefixed(&mut hasher, s.as_bytes());
                    }
                    GitRef::Default => {
                        hasher.update([3u8]);
                    }
                }
            }
            DepSource::Path { path } => {
                hasher.update([2u8]);
                write_len_prefixed(&mut hasher, path.as_bytes());
            }
        }

        // Features — sort locally so insertion-order jitter doesn't
        // perturb the hash. This is belt-and-braces: the Rhai builder
        // appends in script order, which could reasonably vary
        // between formatting refactors.
        let mut sorted_features: Vec<&String> = dep.features.iter().collect();
        sorted_features.sort();
        hasher.update((sorted_features.len() as u64).to_le_bytes());
        for f in sorted_features {
            write_len_prefixed(&mut hasher, f.as_bytes());
        }

        // default_features — single byte is enough.
        hasher.update([dep.default_features as u8]);
    }

    let digest: [u8; 32] = hasher.finalize().into();
    format!("sha256:{}", hex(&digest))
}

fn write_len_prefixed(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn hex(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use gluon_model::{DepSource, ExternalDepDef, GitRef};

    fn dep(name: &str, source: DepSource) -> ExternalDepDef {
        ExternalDepDef {
            name: name.into(),
            source,
            features: Vec::new(),
            default_features: true,
            cfg_flags: Vec::new(),
            rustc_flags: Vec::new(),
            span: None,
        }
    }

    fn arena_with(entries: Vec<ExternalDepDef>) -> Arena<ExternalDepDef> {
        let mut a = Arena::new();
        for e in entries {
            let name = e.name.clone();
            a.insert(name, e);
        }
        a
    }

    #[test]
    fn empty_arena_has_stable_fingerprint() {
        let fp1 = fingerprint_external_deps(&Arena::new());
        let fp2 = fingerprint_external_deps(&Arena::new());
        assert_eq!(fp1, fp2);
        assert!(fp1.starts_with("sha256:"));
        assert_eq!(fp1.len(), "sha256:".len() + 64);
    }

    #[test]
    fn identical_arenas_hash_identically() {
        let a = arena_with(vec![
            dep(
                "bitflags",
                DepSource::CratesIo {
                    version: "2.11".into(),
                },
            ),
            dep(
                "log",
                DepSource::CratesIo {
                    version: "0.4".into(),
                },
            ),
        ]);
        let b = arena_with(vec![
            dep(
                "bitflags",
                DepSource::CratesIo {
                    version: "2.11".into(),
                },
            ),
            dep(
                "log",
                DepSource::CratesIo {
                    version: "0.4".into(),
                },
            ),
        ]);
        assert_eq!(fingerprint_external_deps(&a), fingerprint_external_deps(&b));
    }

    #[test]
    fn insertion_order_does_not_affect_hash() {
        let a = arena_with(vec![
            dep(
                "bitflags",
                DepSource::CratesIo {
                    version: "2.11".into(),
                },
            ),
            dep(
                "log",
                DepSource::CratesIo {
                    version: "0.4".into(),
                },
            ),
        ]);
        let b = arena_with(vec![
            dep(
                "log",
                DepSource::CratesIo {
                    version: "0.4".into(),
                },
            ),
            dep(
                "bitflags",
                DepSource::CratesIo {
                    version: "2.11".into(),
                },
            ),
        ]);
        assert_eq!(fingerprint_external_deps(&a), fingerprint_external_deps(&b));
    }

    #[test]
    fn version_change_perturbs_hash() {
        let a = arena_with(vec![dep(
            "bitflags",
            DepSource::CratesIo {
                version: "2.11".into(),
            },
        )]);
        let b = arena_with(vec![dep(
            "bitflags",
            DepSource::CratesIo {
                version: "2.12".into(),
            },
        )]);
        assert_ne!(fingerprint_external_deps(&a), fingerprint_external_deps(&b));
    }

    #[test]
    fn source_variant_tagging_prevents_collisions() {
        // A CratesIo("foo") dep must not hash the same as a Path("foo")
        // dep — this is what the single-byte source tag is for.
        let a = arena_with(vec![dep(
            "x",
            DepSource::CratesIo {
                version: "foo".into(),
            },
        )]);
        let b = arena_with(vec![dep("x", DepSource::Path { path: "foo".into() })]);
        assert_ne!(fingerprint_external_deps(&a), fingerprint_external_deps(&b));
    }

    #[test]
    fn git_ref_variants_are_distinguished() {
        let url = "https://example.com/x.git".to_string();
        let tag = arena_with(vec![dep(
            "x",
            DepSource::Git {
                url: url.clone(),
                reference: GitRef::Tag("v1".into()),
            },
        )]);
        let branch = arena_with(vec![dep(
            "x",
            DepSource::Git {
                url: url.clone(),
                reference: GitRef::Branch("v1".into()),
            },
        )]);
        let rev = arena_with(vec![dep(
            "x",
            DepSource::Git {
                url: url.clone(),
                reference: GitRef::Rev("v1".into()),
            },
        )]);
        let default = arena_with(vec![dep(
            "x",
            DepSource::Git {
                url,
                reference: GitRef::Default,
            },
        )]);
        let ftag = fingerprint_external_deps(&tag);
        let fbranch = fingerprint_external_deps(&branch);
        let frev = fingerprint_external_deps(&rev);
        let fdefault = fingerprint_external_deps(&default);
        assert_ne!(ftag, fbranch);
        assert_ne!(ftag, frev);
        assert_ne!(ftag, fdefault);
        assert_ne!(fbranch, frev);
    }

    #[test]
    fn feature_order_does_not_matter() {
        let mut a = dep(
            "x",
            DepSource::CratesIo {
                version: "1".into(),
            },
        );
        a.features = vec!["std".into(), "serde".into()];
        let mut b = dep(
            "x",
            DepSource::CratesIo {
                version: "1".into(),
            },
        );
        b.features = vec!["serde".into(), "std".into()];
        assert_eq!(
            fingerprint_external_deps(&arena_with(vec![a])),
            fingerprint_external_deps(&arena_with(vec![b]))
        );
    }

    #[test]
    fn feature_addition_perturbs_hash() {
        let a = dep(
            "x",
            DepSource::CratesIo {
                version: "1".into(),
            },
        );
        let mut b = a.clone();
        b.features.push("std".into());
        assert_ne!(
            fingerprint_external_deps(&arena_with(vec![a])),
            fingerprint_external_deps(&arena_with(vec![b]))
        );
    }

    #[test]
    fn default_features_toggle_perturbs_hash() {
        let a = dep(
            "x",
            DepSource::CratesIo {
                version: "1".into(),
            },
        );
        let mut b = a.clone();
        b.default_features = false;
        assert_ne!(
            fingerprint_external_deps(&arena_with(vec![a])),
            fingerprint_external_deps(&arena_with(vec![b]))
        );
    }

    #[test]
    fn cfg_flags_and_rustc_flags_do_not_perturb_hash() {
        // These affect compile behavior but not vendor output; the
        // fingerprint must stay stable so editing a cfg flag doesn't
        // force a pointless re-vendor.
        let a = dep(
            "x",
            DepSource::CratesIo {
                version: "1".into(),
            },
        );
        let mut b = a.clone();
        b.cfg_flags = vec!["feature=\"foo\"".into()];
        b.rustc_flags = vec!["-C".into(), "opt-level=3".into()];
        assert_eq!(
            fingerprint_external_deps(&arena_with(vec![a])),
            fingerprint_external_deps(&arena_with(vec![b]))
        );
    }

    #[test]
    fn span_does_not_perturb_hash() {
        let a = dep(
            "x",
            DepSource::CratesIo {
                version: "1".into(),
            },
        );
        let mut b = a.clone();
        b.span = Some(gluon_model::SourceSpan::point("fake.rhai", 1, 1));
        assert_eq!(
            fingerprint_external_deps(&arena_with(vec![a])),
            fingerprint_external_deps(&arena_with(vec![b]))
        );
    }
}

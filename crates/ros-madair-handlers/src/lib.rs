// SPDX-License-Identifier: AGPL-3.0-or-later
//! The single source of the Rós Madair extension-type registry.
//!
//! # Why this crate exists (the silent-wrong-answer trap)
//!
//! Extension datatypes are not known to `alizarin-core`. `reference` — and its
//! `ConceptHierarchical` index class, the thing that makes a field land in
//! `concept_tags` and therefore be *filterable* — is contributed entirely by
//! the `alizarin-clm-core` handler, registered in an [`ExtensionTypeRegistry`].
//!
//! The emitter indexes with one registry; the query compiler plans against
//! another. Before this crate they were two hand-written copies. If they
//! disagreed, nothing failed loudly:
//!
//! * emit *without* the clm handler → `reference` degrades to `DetailOnly` →
//!   no `concept_tags` rows are ever written;
//! * query *with* the clm handler → the field looks head-indexed → the
//!   compiler happily emits SQL → the SQL runs → **zero rows**.
//!
//! The consumer reads "no matches" when the truth is "never indexed". So:
//!
//! 1. there is now exactly ONE [`default_registry`] (emitter, Python binding
//!    and any WASM consumer all call it), and
//! 2. the artifact *declares* the handler set it was emitted with
//!    ([`declared_handlers`] → the manifest's `handlers` block), and the query
//!    side rebuilds its registry from that declaration
//!    ([`registry_from_declarations`]), failing LOUDLY when this build cannot
//!    provide a declared handler rather than quietly answering nothing.
//!
//! # WASM
//!
//! Browser consumers query but never emit. This crate is therefore held to
//! `wasm32-unknown-unknown`: core + handler providers only, no rusqlite, no
//! filesystem. It is the seam that lets a browser get the emitter's registry
//! without the emitter.

use serde::{Deserialize, Serialize};

pub use alizarin_core::extension_type_registry::ExtensionTypeRegistry;

/// The version of the `alizarin-clm-core` actually linked into this build
/// (read from its manifest by `build.rs`, so it cannot drift).
pub const CLM_CORE_VERSION: &str = env!("CLM_CORE_VERSION");

/// The provider crate of the `reference` handler.
pub const CLM_CORE_PROVIDER: &str = "alizarin-clm-core";

/// One extension datatype handler, as declared by the artifact that was
/// emitted with it (manifest `handlers[]`) — the record of *what produced this
/// index*, which is what lets the query side reconstruct the same registry
/// instead of guessing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandlerDecl {
    /// The datatype the handler is registered under, e.g. `"reference"`.
    pub datatype: String,
    /// The crate that provides it, e.g. `"alizarin-clm-core"`.
    pub provider: String,
    /// The provider's version at emit time. Provenance only — a mismatch is
    /// not (yet) an error; an *unknown datatype* is.
    pub version: String,
}

/// The registry Rós Madair uses: the CLM `reference` handler under its
/// datatype name. THE definition — emitter and query side both call this.
pub fn default_registry() -> ExtensionTypeRegistry {
    let mut registry = ExtensionTypeRegistry::new();
    registry.register(
        alizarin_clm_core::DATATYPE_NAME,
        alizarin_clm_core::create_reference_handler(),
    );
    registry
}

/// What [`default_registry`] provides, in declaration form. The emitter writes
/// this into the manifest (invariant I6: the artifact explains its own handler
/// set).
pub fn declared_handlers() -> Vec<HandlerDecl> {
    vec![HandlerDecl {
        datatype: alizarin_clm_core::DATATYPE_NAME.to_string(),
        provider: CLM_CORE_PROVIDER.to_string(),
        version: CLM_CORE_VERSION.to_string(),
    }]
}

/// Describe the registry a caller ACTUALLY emitted with, in declaration form.
///
/// The emitter accepts an arbitrary registry, so the manifest must record that
/// registry, not what the default happens to be — otherwise the manifest would
/// itself be the lie it exists to prevent. Datatypes this crate knows are
/// described exactly; anything else is recorded honestly as an unknown
/// provider (and will then fail loudly at [`registry_from_declarations`], which
/// is correct: we cannot reproduce it).
pub fn describe_registry(registry: &ExtensionTypeRegistry) -> Vec<HandlerDecl> {
    let known = declared_handlers();
    let mut decls: Vec<HandlerDecl> = registry
        .list()
        .into_iter()
        .map(|datatype| {
            known
                .iter()
                .find(|d| d.datatype == datatype)
                .cloned()
                .unwrap_or_else(|| HandlerDecl {
                    datatype: datatype.to_string(),
                    provider: "unknown".to_string(),
                    version: "unknown".to_string(),
                })
        })
        .collect();
    // Deterministic: the manifest is hashed and diffed.
    decls.sort_by(|a, b| a.datatype.cmp(&b.datatype));
    decls
}

/// Build a registry covering EXACTLY the declared handlers — the query-side
/// counterpart of [`declared_handlers`].
///
/// Errs, loudly, if a declared datatype is unknown to this build. That error is
/// the whole point: a snapshot indexed by a handler we cannot reproduce would
/// otherwise compile and return zero rows. Never silently omit a handler.
pub fn registry_from_declarations(decls: &[HandlerDecl]) -> Result<ExtensionTypeRegistry, String> {
    let mut registry = ExtensionTypeRegistry::new();
    for decl in decls {
        match decl.datatype.as_str() {
            d if d == alizarin_clm_core::DATATYPE_NAME => {
                registry.register(
                    alizarin_clm_core::DATATYPE_NAME,
                    alizarin_clm_core::create_reference_handler(),
                );
            }
            other => {
                return Err(format!(
                    "snapshot was emitted with handler '{other}' (provider '{}', version '{}') \
                     which this build cannot provide; querying it would silently return no rows. \
                     Known handlers: [{}]",
                    decl.provider,
                    decl.version,
                    known_datatypes().join(", "),
                ));
            }
        }
    }
    Ok(registry)
}

/// The datatypes this build can provide handlers for.
pub fn known_datatypes() -> Vec<&'static str> {
    vec![alizarin_clm_core::DATATYPE_NAME]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_handlers_match_the_default_registry() {
        let decls = declared_handlers();
        assert_eq!(decls.len(), 1);
        assert_eq!(decls[0].datatype, "reference");
        // Every declaration must be reproducible by this build.
        let registry = match registry_from_declarations(&decls) {
            Ok(r) => r,
            Err(e) => panic!("own declarations must rebuild: {e}"),
        };
        assert!(registry.get("reference").is_some());
        assert!(default_registry().get("reference").is_some());
    }

    #[test]
    fn describe_registry_round_trips_the_default() {
        assert_eq!(describe_registry(&default_registry()), declared_handlers());
    }

    #[test]
    fn version_is_real() {
        assert_ne!(CLM_CORE_VERSION, "unknown");
    }

    #[test]
    fn unknown_declared_handler_is_loud() {
        let err = match registry_from_declarations(&[HandlerDecl {
            datatype: "spatial-hex".into(),
            provider: "some-other-crate".into(),
            version: "9.9.9".into(),
        }]) {
            Ok(_) => panic!("unknown handler must not silently rebuild"),
            Err(e) => e,
        };
        assert!(err.contains("spatial-hex"), "{err}");
        assert!(err.contains("cannot provide"), "{err}");
    }
}

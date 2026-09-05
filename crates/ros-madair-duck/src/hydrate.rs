// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tile-graph **hydration** — the part Parquet does not give: turn a set of
//! already-recovered `StaticTile`s into a schema-aware, label-resolved JSON tree.
//!
//! The `DuckReader` read path resolves a resource to its tiles out of the Parquet
//! `data` column and gathers concept labels from the catalog, then hands both to
//! [`hydrate_tiles_with_labels`], which renders the Arches tile graph through
//! alizarin's Display-mode tree builder. (This used to live in the now-deleted
//! `ros-madair-read` crate; it is alizarin tree-building, not DuckDB, but the
//! duck read path is its only consumer, so it lives here.)

use std::collections::HashMap;

use alizarin_core::graph::{GraphLookup, StaticResourceMetadata};
use alizarin_core::json_conversion::resource_tiles_to_tree_with_context;
use alizarin_core::type_serialization::{
    ExternalResolver, SerializationContext, SerializationOptions,
};
use alizarin_core::StaticTile;
use ros_madair_handlers::default_registry;

/// Hydrate tiles into a schema-aware **display** JSON tree with reference/concept
/// labels resolved, rendered against a language preference chain. Returns the
/// hydration error message as a `String` (the caller wraps it into `DuckError`).
///
/// `labels` is a `uuid -> label` map (the duck read path builds it from the
/// concept catalog). Rendering runs through alizarin's Display-mode tree builder:
/// `concept`/`concept-list` resolve via the built-in serializer, and `reference`
/// through the CLM handler in [`default_registry`] — both driven by the one
/// [`VocabResolver`]. Resolution happens inside the tree build; nothing
/// re-interprets the tree afterward.
///
/// The descriptor (the resource `name`) is re-derived from *these* tiles rather
/// than read from a stored copy — which is what makes composed views correct: a
/// caller that passes the merged tiles gets the merged descriptor.
///
/// `graph` must be the model these tiles belong to, with `build_indices()`
/// already called — the substrate does not carry the schema; the caller ships it.
/// Pass a single-element slice (e.g. `&["en"]`) for one language, or an ordered
/// chain (e.g. `&["gd", "ga", "en"]`) for controlled fallback; an empty slice
/// defaults to `en`.
pub fn hydrate_tiles_with_labels(
    tiles: &[StaticTile],
    uuid: &str,
    graph: &impl GraphLookup,
    labels: &HashMap<String, String>,
    languages: &[&str],
) -> Result<serde_json::Value, String> {
    let metadata = build_metadata(uuid, graph, tiles);
    let resolver = VocabResolver { labels };
    let registry = default_registry();
    // `options` carries mode + the language preference chain; `ctx` carries the
    // resolvers/registry. `resource_resolver` is None — resource-instance display
    // names are a separate concern from concept/reference labels. A flat label per
    // concept, so the resolver ignores language; the chain only selects among
    // i18n string datatypes.
    let options = SerializationOptions::display_seq(languages.iter().copied());
    let ctx = SerializationContext {
        node_config: None,
        external_resolver: Some(&resolver),
        // concept_lookup threads emit-side concept identity; the hydration path
        // resolves concepts via `external_resolver`, so None.
        concept_lookup: None,
        resource_resolver: None,
        extension_registry: Some(&registry),
    };
    resource_tiles_to_tree_with_context(tiles, &metadata, graph, &options, &ctx)
}

/// Build the resource metadata (descriptor re-derived from *these* tiles) — see
/// [`hydrate_tiles_with_labels`] for why the descriptor is recomputed here rather
/// than read from a stored copy.
fn build_metadata(
    uuid: &str,
    graph: &impl GraphLookup,
    tiles: &[StaticTile],
) -> StaticResourceMetadata {
    let descriptors = graph.build_descriptors(tiles);
    StaticResourceMetadata {
        graph_id: graph.graph_id().to_string(),
        name: descriptors.name.clone().unwrap_or_default(),
        descriptors,
        resourceinstanceid: uuid.to_string(),
        publication_id: None,
        principaluser_id: None,
        legacyid: None,
        graph_publication_id: None,
        createdtime: None,
        lastmodified: None,
    }
}

/// Resolves concept / reference list-item UUIDs to display labels from a flat
/// `uuid -> label` map. Language-agnostic: one label per concept, so the
/// collection and language arguments are ignored.
struct VocabResolver<'a> {
    labels: &'a HashMap<String, String>,
}

impl ExternalResolver for VocabResolver<'_> {
    fn resolve_concept(
        &self,
        _collection: &str,
        concept_id: &str,
        _language: &str,
    ) -> Option<String> {
        self.labels.get(concept_id).cloned()
    }
}

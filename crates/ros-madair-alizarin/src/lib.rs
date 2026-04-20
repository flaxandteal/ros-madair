// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! Combined WASM binary — Rós Madair SPARQL engine + alizarin heritage viewer.
//!
//! This crate is the **sole cdylib entry point** for the browser.  It links
//! both `ros-madair-client` (SPARQL query engine) and `alizarin-wasm`
//! (heritage data viewer) and provides glue functions that bridge the two:
//!
//! - [`connect_tile_source`]: creates a [`GrowableTileSource`] from a loaded
//!   [`SparqlStore`] and attaches it to a [`WASMResourceInstanceWrapper`].
//! - [`prefetch_tiles_for_resource`]: fetches a tile file from the CDN and
//!   inserts it into the shared tile source so alizarin can read it via the
//!   Rust fast-path (no JS roundtrip).
//! - [`disconnect_tile_source`]: detaches the tile source from the wrapper.
//!
//! All `#[wasm_bindgen]` items from both dependency crates are automatically
//! included in the final `.wasm` file — no manual re-exports needed.

use std::sync::Arc;

use wasm_bindgen::prelude::*;

// Force the linker to include all wasm_bindgen exports from both crates.
pub use alizarin_wasm;
pub use ros_madair_client;

use alizarin_wasm::instance_wrapper::WASMResourceInstanceWrapper;
use ros_madair_client::SparqlStore;
use ros_madair_core::GrowableTileSource;

// ---------------------------------------------------------------------------
// TileSourceHandle — opaque handle exposed to JS
// ---------------------------------------------------------------------------

/// Opaque JS handle to a [`GrowableTileSource`].
///
/// Created by [`connect_tile_source`], passed to [`prefetch_tiles_for_resource`].
#[wasm_bindgen]
pub struct TileSourceHandle {
    source: Arc<GrowableTileSource>,
}

// ---------------------------------------------------------------------------
// Glue functions
// ---------------------------------------------------------------------------

/// Create a [`GrowableTileSource`] from a loaded [`SparqlStore`] and attach
/// it to a [`WASMResourceInstanceWrapper`].
///
/// The dictionary and resource map are cloned from the store.  Returns an
/// opaque handle that must be passed to [`prefetch_tiles_for_resource`] to
/// populate tile data.
///
/// # Errors
///
/// Returns an error if the store's dictionary or resource map has not been
/// loaded yet (i.e. `loadSummary()` was not called).
#[wasm_bindgen]
pub fn connect_tile_source(
    store: &SparqlStore,
    wrapper: &WASMResourceInstanceWrapper,
) -> Result<TileSourceHandle, JsValue> {
    let dict = store
        .dictionary()
        .ok_or_else(|| JsValue::from_str("Dictionary not loaded — call loadSummary() first"))?
        .clone();
    let rmap = store
        .resource_map()
        .ok_or_else(|| JsValue::from_str("Resource map not loaded — call loadSummary() first"))?
        .clone();

    let base_url = store.base_url().to_string();
    let source = Arc::new(GrowableTileSource::new(base_url, dict, rmap));

    wrapper.set_tile_source(source.clone());

    Ok(TileSourceHandle { source })
}

/// Fetch the tile file for the page that contains `resource_uri` and insert
/// it into the [`GrowableTileSource`].
///
/// No-op if the page has already been fetched.  After this call, alizarin's
/// `load_tiles` will serve data from Rust memory instead of falling back to
/// the JS tile_loader callback.
#[wasm_bindgen]
pub async fn prefetch_tiles_for_resource(
    handle: &TileSourceHandle,
    store: &SparqlStore,
    resource_uri: &str,
) -> Result<(), JsValue> {
    let dict = store
        .dictionary()
        .ok_or_else(|| JsValue::from_str("Dictionary not loaded"))?;
    let rmap = store
        .resource_map()
        .ok_or_else(|| JsValue::from_str("Resource map not loaded"))?;

    let dict_id = dict
        .lookup(resource_uri)
        .ok_or_else(|| JsValue::from_str(&format!("Unknown URI: {}", resource_uri)))?;

    let page_id = rmap
        .page_for(dict_id)
        .ok_or_else(|| JsValue::from_str(&format!("No page for dict_id {}", dict_id)))?;

    // Skip if already fetched.
    if handle.source.has_tile_file(page_id) {
        return Ok(());
    }

    let base_url = store.base_url();
    let tile_url = format!("{}tiles/tile_{:04}.dat", base_url, page_id);

    let bytes = ros_madair_client::fetch::fetch_full(&tile_url)
        .await
        .map_err(|e| JsValue::from_str(&e))?;

    handle.source.insert_tile_file(page_id, bytes);

    Ok(())
}

/// Detach the compiled-in tile source from the wrapper, reverting it to
/// JS-callback-only tile loading.
#[wasm_bindgen]
pub fn disconnect_tile_source(wrapper: &WASMResourceInstanceWrapper) {
    wrapper.clear_tile_source();
}

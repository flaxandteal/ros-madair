// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! HTTP range request layer for fetching page headers and predicate blocks.
//!
//! Uses the Fetch API via web-sys to make range requests to static file hosts.

use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Request, RequestInit, Response};

use ros_madair_core::{
    full_header_size, parse_page_header, parse_tile_content_header, tile_full_header_size,
    PageHeader, TileContentHeader,
};

/// Fetch a byte range from a URL.
async fn fetch_range(url: &str, start: u32, end: u32) -> Result<Vec<u8>, String> {
    let expected_len = (end - start) as usize;
    let opts = RequestInit::new();
    opts.set_method("GET");

    let headers = web_sys::Headers::new().map_err(|e| format!("Headers::new failed: {:?}", e))?;
    headers
        .set("Range", &format!("bytes={}-{}", start, end - 1))
        .map_err(|e| format!("Failed to set Range header: {:?}", e))?;
    opts.set_headers(&headers);

    let request =
        Request::new_with_str_and_init(url, &opts).map_err(|e| format!("Request failed: {:?}", e))?;

    let window = web_sys::window().ok_or("No window object")?;
    let resp_value = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|e| format!("Fetch failed: {:?}", e))?;

    let resp: Response = resp_value
        .dyn_into()
        .map_err(|_| "Response cast failed")?;

    let status = resp.status();
    if !resp.ok() && status != 206 {
        return Err(format!("HTTP {}: {}", status, resp.status_text()));
    }

    let array_buffer = JsFuture::from(
        resp.array_buffer()
            .map_err(|e| format!("arrayBuffer() failed: {:?}", e))?,
    )
    .await
    .map_err(|e| format!("arrayBuffer await failed: {:?}", e))?;

    let uint8_array = js_sys::Uint8Array::new(&array_buffer);
    let bytes = uint8_array.to_vec();

    // Guard: if the server ignored the Range header and returned the full
    // file (HTTP 200 instead of 206), slice out the requested byte range.
    if status == 200 && bytes.len() > expected_len {
        let s = start as usize;
        let e = end as usize;
        if bytes.len() >= e {
            web_sys::console::warn_1(&format!(
                "fetch_range: server returned full file ({} bytes, HTTP 200). Slicing [{}, {}).",
                bytes.len(), s, e
            ).into());
            return Ok(bytes[s..e].to_vec());
        } else {
            return Err(format!(
                "fetch_range: full file {} bytes too small for range [{}, {})",
                bytes.len(), s, e
            ));
        }
    }

    // Server returned partial content but more than expected (Range start to
    // EOF rather than exact range). Truncate to requested length.
    if bytes.len() > expected_len {
        web_sys::console::warn_1(&format!(
            "fetch_range: expected {} bytes, got {} (HTTP {}). Truncating.",
            expected_len, bytes.len(), status
        ).into());
        return Ok(bytes[..expected_len].to_vec());
    }

    if bytes.len() < expected_len {
        return Err(format!(
            "fetch_range: expected {} bytes, got {} (HTTP {})",
            expected_len, bytes.len(), status
        ));
    }

    Ok(bytes)
}

/// Fetch a complete file (no range request).
pub async fn fetch_full(url: &str) -> Result<Vec<u8>, String> {
    let opts = RequestInit::new();
    opts.set_method("GET");

    let request =
        Request::new_with_str_and_init(url, &opts).map_err(|e| format!("Request failed: {:?}", e))?;

    let window = web_sys::window().ok_or("No window object")?;
    let resp_value = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|e| format!("Fetch failed: {:?}", e))?;

    let resp: Response = resp_value
        .dyn_into()
        .map_err(|_| "Response cast failed")?;

    if !resp.ok() {
        return Err(format!("HTTP {}: {}", resp.status(), resp.status_text()));
    }

    let array_buffer = JsFuture::from(
        resp.array_buffer()
            .map_err(|e| format!("arrayBuffer() failed: {:?}", e))?,
    )
    .await
    .map_err(|e| format!("arrayBuffer await failed: {:?}", e))?;

    let uint8_array = js_sys::Uint8Array::new(&array_buffer);
    Ok(uint8_array.to_vec())
}

/// Fetch a page header in a single range request.
///
/// Probes 1024 bytes upfront, which covers all typical headers (max observed
/// ~550 bytes at page_size=2000). Falls back to a second request only if the
/// header is unusually large.
const HEADER_PROBE: u32 = 1024;

pub async fn fetch_page_header(page_url: &str) -> Result<PageHeader, String> {
    let probe = fetch_range(page_url, 0, HEADER_PROBE).await?;
    let header_size = full_header_size(&probe)? as u32;

    if probe.len() >= header_size as usize {
        return parse_page_header(&probe[..header_size as usize]);
    }

    // Rare fallback: header larger than probe
    let header_bytes = fetch_range(page_url, 0, header_size).await?;
    parse_page_header(&header_bytes)
}

/// Fetch the resource metadata section from a page file via a range request.
pub async fn fetch_resource_meta(page_url: &str, offset: u32, size: u32) -> Result<Vec<u8>, String> {
    fetch_range(page_url, offset, offset + size).await
}

/// Fetch specific predicate blocks from a page file.
///
/// Returns record bytes for each requested predicate.
/// Each block's records are 8 bytes (object_val: u32, subject_id: u32).
pub async fn fetch_predicate_blocks(
    page_url: &str,
    header: &PageHeader,
    pred_ids: &[u32],
) -> Result<Vec<(u32, Vec<u8>)>, String> {
    let mut results = Vec::with_capacity(pred_ids.len());

    for &pred_id in pred_ids {
        if let Some((start, end)) = header.predicate_byte_range(pred_id) {
            if start < end {
                let bytes = fetch_range(page_url, start, end).await?;
                results.push((pred_id, bytes));
            }
        }
    }

    Ok(results)
}

// --- Tile content fetching ---

/// Probe size for tile content headers. Covers up to ~84 resources per page
/// in a single request (9 + 12*84 = 1017).
const TILE_HEADER_PROBE: u32 = 1024;

/// Fetch a tile content file header in a single range request.
///
/// Probes 1024 bytes upfront. Falls back to a second request if the header
/// is unusually large (pages with >84 resources).
pub async fn fetch_tile_header(tile_url: &str) -> Result<TileContentHeader, String> {
    let probe = fetch_range(tile_url, 0, TILE_HEADER_PROBE).await?;
    let header_size = tile_full_header_size(&probe)? as u32;

    if probe.len() >= header_size as usize {
        return parse_tile_content_header(&probe[..header_size as usize]);
    }

    // Rare fallback: header larger than probe
    let header_bytes = fetch_range(tile_url, 0, header_size).await?;
    parse_tile_content_header(&header_bytes)
}

/// Fetch a tile blob from a tile content file via a range request.
pub async fn fetch_tile_blob(tile_url: &str, offset: u32, size: u32) -> Result<Vec<u8>, String> {
    fetch_range(tile_url, offset, offset + size).await
}

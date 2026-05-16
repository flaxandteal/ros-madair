# Binary Formats

Byte-level specifications for all Rós Madair index files. All multi-byte
integers are **little-endian (LE)** unless stated otherwise. All strings are
**UTF-8** with **u32 LE length prefix** (no null terminators) unless stated
otherwise.

---

## Index Directory Layout

A complete Rós Madair index contains:

```
{index}/
├── summary.bin              # Page-level quad index (query planner)
├── dictionary.bin           # Term ↔ u32 ID mapping
├── resource_map.bin         # dict_id → page_id routing
├── concept_intervals.bin    # DFS interval encoding for concept hierarchy
├── concept_tree.bin         # SKOS concept tree with labels
├── page_meta.json           # Page inventory metadata
├── resource_names.json      # Resource display names (optional)
├── pages/
│   ├── page_0000.dat        # Per-page query index (predicate-partitioned)
│   ├── page_0001.dat
│   └── ...
└── tiles/
    ├── tile_0000.dat        # Per-page full-fidelity resource data
    ├── tile_0001.dat
    └── ...
```

### Pages vs Tiles

Both directories use the same page numbering. For a given page ID N:

- `pages/page_N.dat` — **query index**: compact fixed-width records for
  SPARQL evaluation. Predicate-partitioned, binary-searchable, designed for
  Range-request fetching.
- `tiles/tile_N.dat` — **render data**: full MessagePack-serialized resource
  blobs for UI display. Fetched whole and cached.

They contain data for the **same set of resources** but serve different
workloads. Neither is derivable from the other without the original source.

### Client Fetch Patterns

| File | Fetch strategy | When |
|------|---------------|------|
| `summary.bin` | Full | Store init |
| `dictionary.bin` | Full | Store init |
| `resource_map.bin` | Full | Store init |
| `concept_intervals.bin` | Full | Store init |
| `concept_tree.bin` | Full | Store init (or lazy) |
| `page_meta.json` | Full | Store init |
| `pages/page_XXXX.dat` | **Range requests** (header probe, then predicate blocks) | Per query |
| `tiles/tile_XXXX.dat` | **Full fetch**, cached per page_id | Per resource display |

---

## Dictionary (`dictionary.bin`)

Sequential term table. IDs are implicit (entry 0 = ID 0, entry 1 = ID 1, ...).
No magic bytes.

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0 | 4 | `term_count` | u32 LE — number of entries |
| 4 | variable | entries | Repeated `term_count` times |

Each entry:

| Offset (relative) | Size | Field | Description |
|--------------------|------|-------|-------------|
| 0 | 4 | `len` | u32 LE — byte length of UTF-8 string |
| 4 | `len` | `term` | UTF-8 encoded URI or literal |

**Lookup**: O(n) sequential scan for string→ID. O(1) for ID→string (seek to
entry by accumulating lengths). Clients typically load into a `Vec<String>` +
`HashMap<String, u32>` at init.

**Typical size:** ~2 MB for 50K resources (Goidelic), ~15 MB for 200K terms
(heritage).

---

## Resource Map (`resource_map.bin`)

Maps dictionary IDs to page IDs. Flat array indexed by dict_id — O(1) lookup.

### Header (8 bytes)

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0 | 4 | `magic` | `b"RMRM"` (`0x524D524D`) |
| 4 | 4 | `entry_count` | u32 LE — must equal `dictionary.term_count` |

### Body

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 8 | 2 × `entry_count` | entries | u16 LE per entry |

Each entry at index `i` gives the page_id for dict_id `i`:

| Value | Meaning |
|-------|---------|
| 0..65534 | Page ID containing this resource |
| `0xFFFF` | Not a resource (predicate, literal, or unmapped term) |

**Lookup**: `page_id = entries[dict_id]` — O(1) array index.

**Total size:** 8 + (entry_count × 2) bytes. For 26K terms: ~52 KB.

---

## Summary Index (`summary.bin`)

Page-level quad index for **query planning** — determines which page files to
fetch without loading any page data.

Magic: `RMSQ` (`0x524D5351`).

### Header (21 bytes)

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0 | 4 | `magic` | `b"RMSQ"` |
| 4 | 1 | `version` | `1` |
| 5 | 4 | `quad_count` | u32 LE — number of quads per section |
| 9 | 4 | `spo_offset` | u32 LE — byte offset of SPO section (always 21) |
| 13 | 4 | `pso_offset` | u32 LE — byte offset of PSO section |
| 17 | 4 | `ops_offset` | u32 LE — byte offset of OPS section |

### Quad Sections

Three sections follow the header, each containing `quad_count` quads of 20
bytes each. The sections contain identical quad data, sorted differently:

| Section | Sort order | Primary use case |
|---------|-----------|-----------------|
| SPO | `(page_s, predicate, page_o)` | Forward traversal from a known page |
| PSO | `(predicate, page_s, page_o)` | Find all pages with a given predicate |
| OPS | `(page_o, predicate, page_s)` | Reverse lookup: which pages link to object O via P |

### SummaryQuad (20 bytes)

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0 | 4 | `page_s` | u32 LE — source page ID |
| 4 | 4 | `predicate` | u32 LE — predicate dictionary ID |
| 8 | 4 | `page_o` | u32 LE — see semantics below |
| 12 | 4 | `edge_count` | u32 LE — number of resource-level edges summarized |
| 16 | 4 | `subject_count` | u32 LE — distinct subjects in page_s with this (pred, page_o) |

**`page_o` semantics** depend on the predicate's datatype:

| Predicate type | `page_o` value | Meaning |
|---|---|---|
| Resource-instance link | Target page ID | Direct page reference |
| Concept/domain-value | `u32::MAX` sentinel | Non-page value; actual ID in page records |
| Boolean/date/geo | Quantized bucket value | Range-query key |

**Lookup**: Binary search (`partition_point`) within the relevant section.

**Total size:** 21 + 3 × (`quad_count` × 20) bytes. For 246 quads (Goidelic):
~15 KB. For 210K quads (heritage): ~12 MB.

---

## Page File (`pages/page_XXXX.dat`)

Predicate-partitioned fixed-width query records. Designed for Range-request
access — clients fetch the header first, then only the predicate blocks
needed for the current query.

Magic: `RMPG` (`0x524D5047`).

### Header (version 3)

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0 | 4 | `magic` | `b"RMPG"` |
| 4 | 1 | `version` | `3` |
| 5 | 2 | `predicate_count` | u16 LE — number of predicate blocks |
| 7 | 4 | `resource_meta_offset` | u32 LE — byte offset to resource metadata section |
| 11 | 4 | `resource_meta_size` | u32 LE — byte length of metadata section |
| 15 | 12 × `predicate_count` | entries | Predicate block descriptors (sorted by `pred_id`) |

**Header size:** 15 + (12 × `predicate_count`) bytes.

Each predicate entry (12 bytes):

| Offset (relative) | Size | Field | Description |
|--------------------|------|-------|-------------|
| 0 | 4 | `pred_id` | u32 LE — predicate dictionary ID |
| 4 | 4 | `offset` | u32 LE — byte offset from file start to block data |
| 8 | 4 | `record_count` | u32 LE — number of 8-byte records in this block |

### Body: Predicate Blocks

Each block contains `record_count` fixed-width records, contiguous at
`offset` bytes from file start.

### PageRecord (8 bytes)

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0 | 4 | `object_val` | u32 LE — dictionary ID or quantized literal |
| 4 | 4 | `subject_id` | u32 LE — subject resource dictionary ID |

Records are sorted by `(object_val, subject_id)` within each predicate
block, enabling binary search.

### Resource Metadata Section

Located at `resource_meta_offset`, length `resource_meta_size` bytes.

| Offset (relative) | Size | Field | Description |
|--------------------|------|-------|-------------|
| 0 | 4 | `entry_count` | u32 LE |
| 4 | variable | entries | Repeated `entry_count` times |

Each metadata entry:

| Offset (relative) | Size | Field | Description |
|--------------------|------|-------|-------------|
| 0 | 4 | `dict_id` | u32 LE — resource dictionary ID |
| 4 | 2 | `name_len` | u16 LE |
| 6 | `name_len` | `name` | UTF-8 display name |
| 6+N | 2 | `slug_len` | u16 LE |
| 8+N | `slug_len` | `slug` | UTF-8 URL slug |
| 8+N+M | 2 | `model_len` | u16 LE |
| 10+N+M | `model_len` | `model` | UTF-8 model identifier |

**Note:** Metadata strings use **u16 LE** length prefix (not u32), since
individual field values are always short.

**Typical page size:** 7–9 KB (Goidelic, ~200 resources per page, default).

### Client Access Pattern

```
1. Range: bytes=0-1023          → parse header, discover predicate offsets
2. Range: bytes=offset-end      → fetch specific predicate block(s) for query
3. (optional) Range for metadata section if resource names needed
```

---

## Tile Content File (`tiles/tile_XXXX.dat`)

Full-fidelity resource data for UI rendering. MessagePack-encoded blobs
indexed by subject_id.

Magic: `RMTL` (`0x524D544C`).

### Header

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0 | 4 | `magic` | `b"RMTL"` |
| 4 | 1 | `version` | `1` or `2` |
| 5 | 4 | `entry_count` | u32 LE — number of resource blobs |
| 9 | 12 × `entry_count` | entries | Blob index (sorted by `subject_id`) |

**Header size:** 9 + (12 × `entry_count`) bytes.

Each entry (12 bytes):

| Offset (relative) | Size | Field | Description |
|--------------------|------|-------|-------------|
| 0 | 4 | `subject_id` | u32 LE — resource dictionary ID |
| 4 | 4 | `blob_offset` | u32 LE — byte offset from file start |
| 8 | 4 | `blob_size` | u32 LE — byte length of blob |

### Body: Blobs

Variable-length MessagePack-encoded data, located at their declared offsets.

**Version 1 blob:** `Vec<StaticTile>` — array of Alizarin tile objects.

**Version 2 blob:** MessagePack map with fields:
- `tiles`: `Vec<StaticTile>` — tile data
- `__cache`: (optional) cached descriptor/relationship data
- `__scopes`: (optional) scope metadata

### Client Access Pattern

```
1. Fetch entire file (typical 1–5 MB, cached per page_id)
2. Parse header (first 9 bytes → entry_count → read full header)
3. Binary search entries by subject_id
4. Slice blob bytes → msgpack decode
```

**No Range requests** — tile files are fetched whole because:
- Header parsing needs the full entry index anyway
- Once fetched, all resources on the same page are available without re-fetch
- Simplifies caching (one HashMap entry per page_id)

**Typical tile size:** 100–500 KB per page (Goidelic: ~250 KB average for 200
resources/page, default).

---

## Concept Intervals (`concept_intervals.bin`)

DFS (Euler tour) interval encoding for hierarchical concept queries. Given a
concept's interval [enter, leave], all descendants have intervals contained
within it.

Magic: `RMCI` (`0x524D4349`).

### Header (12 bytes)

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0 | 4 | `magic` | `b"RMCI"` |
| 4 | 1 | `version` | `1` |
| 5 | 3 | padding | `0x000000` |
| 8 | 4 | `entry_count` | u32 LE — number of concept entries |

### Forward Index (sorted by dict_id)

Starts at offset 12. Each entry (12 bytes):

| Offset (relative) | Size | Field | Description |
|--------------------|------|-------|-------------|
| 0 | 4 | `dict_id` | u32 LE — concept dictionary ID |
| 4 | 4 | `dfs_enter` | u32 LE — DFS entry number |
| 8 | 4 | `dfs_leave` | u32 LE — DFS exit number |

### Reverse Index (sorted by dfs_enter)

Starts at offset 12 + (entry_count × 12). Each entry (8 bytes):

| Offset (relative) | Size | Field | Description |
|--------------------|------|-------|-------------|
| 0 | 4 | `dfs_enter` | u32 LE — DFS entry number |
| 4 | 4 | `dict_id` | u32 LE — concept dictionary ID |

**Total size:** 12 + (entry_count × 20) bytes.

**Lookup:**
- dict_id → interval: binary search forward index by dict_id
- dfs_enter → dict_id: binary search reverse index by dfs_enter
- "Is A descendant of B?": `B.enter < A.enter && A.leave < B.leave`

---

## Concept Tree (`concept_tree.bin`)

SKOS concept hierarchy with labels for browsing UI and label→value_id
resolution.

Magic: `RMCT` (`0x524D4354`).

### Header (16 bytes)

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0 | 4 | `magic` | `b"RMCT"` |
| 4 | 1 | `version` | `1` |
| 5 | 3 | padding | `0x000000` |
| 8 | 4 | `collection_count` | u32 LE |
| 12 | 4 | `entry_count` | u32 LE |
| 16 | 4 | `strings_offset` | u32 LE — byte offset to string table |

### Collection Table

Starts at offset 20. Each collection (44 bytes):

| Offset (relative) | Size | Field | Description |
|--------------------|------|-------|-------------|
| 0 | 36 | `collection_id` | UUID string, null-padded to 36 bytes |
| 36 | 4 | `first_entry` | u32 LE — index into entry table |
| 40 | 4 | `entry_count` | u32 LE — entries in this collection |

### Entry Table

Starts at offset 20 + (collection_count × 44). Each entry (52 bytes):

| Offset (relative) | Size | Field | Description |
|--------------------|------|-------|-------------|
| 0 | 4 | `dfs_enter` | u32 LE |
| 4 | 4 | `dfs_leave` | u32 LE |
| 8 | 2 | `depth` | u16 LE — tree depth |
| 10 | 2 | `label_len` | u16 LE — byte length of label |
| 12 | 36 | `value_id` | UUID string, null-padded to 36 bytes |
| 48 | 4 | `label_offset` | u32 LE — offset into string table |

### String Table

Starts at `strings_offset`. Concatenated UTF-8 label strings, referenced by
(`label_offset`, `label_len`) pairs from entries.

**Hierarchy navigation:** Children have `depth = parent.depth + 1` and
`parent.dfs_enter < child.dfs_enter < child.dfs_leave < parent.dfs_leave`.

---

## Page Metadata (`page_meta.json`)

JSON array — one object per page.

```json
[
  {
    "page_id": 0,
    "graph_id": "449c8695-253e-521b-8994-27701ce22305",
    "resource_count": 200,
    "bbox": null,
    "is_shadow": false
  }
]
```

| Field | Type | Description |
|-------|------|-------------|
| `page_id` | integer | Page identifier, matches filenames (`page_XXXX.dat`, `tile_XXXX.dat`) |
| `graph_id` | string | Arches graph UUID for the resource model |
| `resource_count` | integer | Number of resources assigned to this page |
| `bbox` | `[min_lng, min_lat, max_lng, max_lat]` or `null` | WGS84 bounding box |
| `is_shadow` | boolean | Whether page is a shadow (cross-graph reference target) |

---

## Resource Names (`resource_names.json`)

Optional JSON object mapping resource instance IDs to display names.

```json
{
  "f009f307-3154-79b8-c6de-faae56598962": "focal",
  "4afe6eaa-0822-578b-4e69-d37b6201243d": "teach"
}
```

Keys are resource instance UUIDs; values are display name strings. Used for
descriptor resolution when the full tile data isn't loaded.

---

## Quantized Value Types

The `object_val` field in `PageRecord` and `page_o` field in `SummaryQuad`
encode different types depending on the predicate's datatype:

| Datatype | Quantization | `object_val` meaning |
|----------|-------------|---------------------|
| `concept`, `concept-list` | Dictionary ID | u32 ID of the concept URI |
| `resource-instance`, `resource-instance-list` | Dictionary ID | u32 ID of the resource URI |
| `boolean` | Direct | `0` = false, `1` = true |
| `date` | Days since epoch | u32 day count from 0001-01-01 |
| `geojson-feature-collection` | Hilbert point | 2D Hilbert index of centroid at 16-bit resolution |

For concept predicates in `SummaryQuad`, `page_o` is set to `u32::MAX`
(sentinel) because concept values are not page-routable — the actual concept
dict_id appears in `PageRecord.object_val`.

---

## Page Assignment

Resources are assigned to pages (and therefore to page/tile file pairs) by:

1. Grouping by `graph_id` (resource model)
2. Within each graph, Hilbert-sorting by `(centroid_x, centroid_y,
   concept_type)` — geographic resources cluster spatially; non-geographic
   resources sort by concept membership
3. Slicing into pages of ~200 resources each (configurable via `page_size`)

This ensures:
- Resources of the same type are colocated
- Geographic neighbors share a page (good for bbox queries)
- Deterministic assignment (same input → same page IDs across rebuilds)

---

## Size Budget (Goidelic corpus, 51K resources)

| File | Size | Notes |
|------|------|-------|
| `dictionary.bin` | 1.9 MB | 26K terms |
| `resource_map.bin` | 52 KB | 26K entries × 2 bytes |
| `summary.bin` | 15 KB | 246 quads |
| `concept_intervals.bin` | 5 KB | ~100 concepts |
| `concept_tree.bin` | 9 KB | Tree with labels |
| `page_meta.json` | 9 KB | 27 pages |
| `pages/` (total) | 1.4 MB | 27 files, ~52 KB avg |
| `tiles/` (total) | 67 MB | 27 files, ~2.5 MB avg |
| **Init payload** | ~2 MB | dictionary + resource_map + summary + meta |
| **Per-query payload** | ~0.1–1 KB | Range request for 1–3 predicate blocks |
| **Per-entry payload** | ~2.5 MB | Full tile file (cached, amortized across 2K resources) |

# Binary Formats

Byte-level specifications for all Rós Madair binary formats. All multi-byte
integers are **little-endian (LE)**.

## Dictionary (`dictionary.bin`)

Sequential term table. IDs are implicit (entry 0 = ID 0, entry 1 = ID 1, ...).

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0 | 4 | `term_count` | u32 LE — number of entries |
| 4 | variable | entries | Repeated `term_count` times |

Each entry:

| Offset (relative) | Size | Field | Description |
|--------------------|------|-------|-------------|
| 0 | 4 | `len` | u32 LE — byte length of UTF-8 string |
| 4 | `len` | `term` | UTF-8 encoded URI or literal |

**Typical size:** ~15 MB for 200K terms (heritage dataset).

---

## Summary Index (`summary.bin`)

Magic: `RMSQ` (`0x524D5351`). Three sorted copies of the same quad data for
different access patterns.

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
bytes each. The sections contain identical data, sorted differently:

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
| 8 | 4 | `page_o` | u32 LE — target page ID (resource links), `u32::MAX` sentinel (concept links), or quantized value (literals) |
| 12 | 4 | `edge_count` | u32 LE — number of resource-level edges |
| 16 | 4 | `subject_count` | u32 LE — distinct subjects in page_s |

**Total size:** 21 + 3 × (`quad_count` × 20) bytes.

**Typical size:** ~12 MB for 210K quads (heritage dataset).

---

## Page File (`pages/page_XXXX.dat`)

Magic: `RMPG` (`0x524D5047`). Predicate-partitioned fixed-width records.

### Header

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0 | 4 | `magic` | `b"RMPG"` |
| 4 | 1 | `version` | `2` |
| 5 | 2 | `predicate_count` | u16 LE — number of predicate blocks |
| 7 | 12 × `predicate_count` | entries | Predicate block descriptors |

Each predicate entry (12 bytes):

| Offset (relative) | Size | Field | Description |
|--------------------|------|-------|-------------|
| 0 | 4 | `pred_id` | u32 LE — predicate dictionary ID |
| 4 | 4 | `offset` | u32 LE — byte offset from file start |
| 8 | 4 | `record_count` | u32 LE — number of 8-byte records in this block |

**Header size:** 7 + 12 × `predicate_count` bytes (typically 19–547 bytes).

### Body

Predicate blocks follow the header. Each block contains `record_count`
fixed-width records, sorted by `(object_val, subject_id)`.

### PageRecord (8 bytes)

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0 | 4 | `object_val` | u32 LE — dictionary ID or quantized literal |
| 4 | 4 | `subject_id` | u32 LE — subject resource dictionary ID |

Records are sorted by `(object_val, subject_id)` within each predicate
block, enabling binary search for exact object matches.

**Typical page size:** 100–500 KB (267 KB average for heritage dataset).

---

## Page Metadata (`page_meta.json`)

JSON array — not a binary format, but included here for completeness.

```json
[
  {
    "page_id": 0,
    "resource_count": 2000,
    "graph_id": "22477f01-1c40-11ea-b786-3af9d3b32b71",
    "bbox": {
      "min_x": -8.178,
      "min_y": 53.271,
      "max_x": -5.432,
      "max_y": 55.381
    }
  }
]
```

| Field | Type | Description |
|-------|------|-------------|
| `page_id` | integer | Page identifier, matches filename (`page_XXXX.dat`) |
| `resource_count` | integer | Number of resources assigned to this page |
| `graph_id` | string | Arches graph UUID for the resource model |
| `bbox` | object | Geographic bounding box (WGS84) of resources in this page |

---

## Quantized Value Types

The `object_val` field in `PageRecord` and `page_o` field in `SummaryQuad`
store different types of values depending on the predicate's datatype:

| Datatype | Quantization | `object_val` meaning |
|----------|-------------|---------------------|
| `concept`, `concept-list` | Dictionary ID | u32 ID of the concept URI |
| `resource-instance`, `resource-instance-list` | Dictionary ID | u32 ID of the resource URI |
| `boolean` | Direct | `0` = false, `1` = true |
| `date` | Days since epoch | u32 day count from 0001-01-01 |
| `geojson-feature-collection` | Hilbert point | 2D Hilbert index of centroid at 16-bit resolution |

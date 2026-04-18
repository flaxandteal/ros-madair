# Data Structures

Rós Madair uses three binary formats and one JSON metadata file. All binary
formats use **little-endian** byte order.

## How the Files Connect

The index files share two distinct ID spaces that thread through every
structure:

```
                      ┌──────────────────────────────────┐
                      │          dictionary.bin           │
                      │       term  ↔  dict_id (u32)     │
                      └──────┬────────────────────┬──────┘
                   dict IDs  │                    │  dict IDs
                   flow into │                    │  flow into
                             │                    │
              ┌──────────────▼───┐        ┌───────▼──────────────┐
              │   summary.bin    │        │  pages/page_XXXX.dat │
              │                  │        │                      │
              │  .predicate ◄────┼── dict │  header: pred_id     │
              │  .page_o    ◄────┼── (†)  │  records:            │
              │                  │        │    .subject_id       │
              │  .page_s    ◄────┼── page │    .object_val  (†)  │
              │                  │   IDs  │                      │
              └──────────┬───────┘        └──────────┬───────────┘
                         │  page IDs                 │  file name
                         │  in page_s                │  = page ID
                         │                           │
              ┌──────────▼───────────────────────────▼──┐
              │             page_meta.json               │
              │      page_id → bbox, graph_id, count     │
              └─────────────────────────────────────────┘

              ┌────────────────────────┐
              │    resource_map.bin     │
              │                        │
              │  dict_id  ──►  page_id │  Bridges the two
              │  (0xFFFF = not a       │  ID spaces: given a
              │   resource)            │  resource's dict ID,
              └────────────────────────┘  yields its page.
```

**(†) `page_o` and `object_val` carry different values depending on the
predicate type:**

| Predicate type          | `page_o` in summary             | `object_val` in page record   |
|-------------------------|---------------------------------|-------------------------------|
| resource-instance link  | Page ID of target resource      | Dictionary ID of target URI   |
| concept / domain-value  | Sentinel (`u32::MAX`)           | Dictionary ID of concept URI  |
| boolean / date / geo    | Quantized value (bucket key)    | Quantized value               |

This dual encoding lets the summary route queries for both link-based
traversals (resource → resource) and value-based filters (concept
membership, date ranges, spatial proximity).

## Dictionary (`dictionary.bin`)

A bidirectional mapping between URIs/literals and compact u32 integer IDs.
Every predicate URI, concept URI, and resource URI gets an entry.

**Format:**

```
[term_count: u32 LE]
[entry 0: len (u32 LE) + UTF-8 bytes]
[entry 1: len (u32 LE) + UTF-8 bytes]
...
```

IDs are assigned sequentially (0, 1, 2, ...) in insertion order. The client
loads the entire dictionary at init and uses it to translate between query
URIs and the integer IDs stored in the index.

For the heritage dataset: **205,224 terms, 14.7 MB**.

## Summary Index (`summary.bin`)

The summary is a page-level quad index: it records which (page, predicate,
object) combinations exist, with cardinality counts. It enables query
planning with **zero page fetches**.

**Format:**

```
┌─ Header (21 bytes) ───────────────────────────────┐
│ magic: "RMSQ" (4 bytes)                           │
│ version: 1 (1 byte)                               │
│ quad_count: u32 LE                                │
│ spo_offset: u32 LE                                │
│ pso_offset: u32 LE                                │
│ ops_offset: u32 LE                                │
├─ SPO section (sorted by page_s, pred, page_o) ───┤
│ quad_count × 20-byte quads                        │
├─ PSO section (sorted by pred, page_s, page_o) ───┤
│ quad_count × 20-byte quads                        │
├─ OPS section (sorted by page_o, pred, page_s) ───┤
│ quad_count × 20-byte quads                        │
└───────────────────────────────────────────────────┘
```

Each **SummaryQuad** is 20 bytes:

| Field          | Type  | Meaning                                    |
|----------------|-------|--------------------------------------------|
| `page_s`       | u32   | Page containing the subject resources       |
| `predicate`    | u32   | Predicate dictionary ID                     |
| `page_o`       | u32   | Page ID (resource links), sentinel (concept links), or quantized value (literals) — see [table above](#how-the-files-connect) |
| `edge_count`   | u32   | Number of resource-level edges summarized   |
| `subject_count`| u32   | Distinct subjects in page_s with this (pred, page_o) |

Three sorted copies of the same data support different access patterns:

| Index | Sort order            | Used for                                  |
|-------|-----------------------|-------------------------------------------|
| SPO   | (page_s, pred, page_o)| "What objects does page S link to via P?"  |
| PSO   | (pred, page_s, page_o)| "Which pages have predicate P?"            |
| OPS   | (page_o, pred, page_s)| "Which pages link to object O via P?"      |

All lookups use binary search (`partition_point`) on the sorted arrays.

For the heritage dataset: **210,494 quads, 12.0 MB** (3 × 210,494 × 20 bytes + 21-byte header).

### Three Sort Orders — Worked Example

The same four quads appear in each section, sorted differently. Each
ordering makes a different family of lookups O(log N):

```
Given quads:  (page_s=0, pred=10, page_o=2, edges=5, subj=3)
              (page_s=0, pred=20, page_o=3, edges=10, subj=8)
              (page_s=1, pred=10, page_o=2, edges=3, subj=3)
              (page_s=2, pred=20, page_o=3, edges=7, subj=5)

┌─ SPO (sorted by page_s, pred, page_o) ────────────────────────┐
│                                                                 │
│  page_s  pred  page_o       Query enabled                       │
│  ──────  ────  ──────       ──────────────────────────────────  │
│    0      10     2    ┐                                         │
│    0      20     3    ┘◄─── lookup_s(0): all edges from page 0  │
│    1      10     2          lookup_sp(0, 10): page 0 + pred 10  │
│    2      20     3                                              │
└─────────────────────────────────────────────────────────────────┘

┌─ PSO (sorted by pred, page_s, page_o) ────────────────────────┐
│                                                                 │
│  pred  page_s  page_o       Query enabled                       │
│  ────  ──────  ──────       ──────────────────────────────────  │
│   10     0       2    ┐                                         │
│   10     1       2    ┘◄─── lookup_p(10): all pages w/ pred 10  │
│   20     0       3          lookup_ps(20, 0): pred 20 + page 0  │
│   20     2       3                                              │
└─────────────────────────────────────────────────────────────────┘

┌─ OPS (sorted by page_o, pred, page_s) ────────────────────────┐
│                                                                 │
│  page_o  pred  page_s       Query enabled                       │
│  ──────  ────  ──────       ──────────────────────────────────  │
│    2      10     0    ┐                                         │
│    2      10     1    ┘◄─── lookup_op(2, 10): who links to      │
│    3      20     0              object 2 via pred 10?            │
│    3      20     2          lookup_o(3): who links to object 3? │
└─────────────────────────────────────────────────────────────────┘
```

The OPS ordering is the workhorse for query planning: given a bound
object (e.g., a concept URI) and predicate, it returns the set of
subject pages in O(log N) — without touching any page files.

## Page Files (`pages/page_XXXX.dat`)

Each page file contains all indexed records for the ~2,000 resources
assigned to that page. Records are partitioned by predicate and sorted
within each partition.

**Format:**

```
┌─ Header ──────────────────────────────────────────┐
│ magic: "RMPG" (4 bytes)                           │
│ version: 2 (1 byte)                               │
│ predicate_count: u16 LE (2 bytes)                 │
│ entries[0]: pred_id(u32) offset(u32) count(u32)   │  ← 12 bytes
│ entries[1]: ...                                    │     per entry
│ ...                                                │
├─ Body (predicate blocks) ─────────────────────────┤
│ block 0: [PageRecord × count_0]                   │
│ block 1: [PageRecord × count_1]                   │
│ ...                                                │
└───────────────────────────────────────────────────┘
```

Header size = **7 + 12 × predicate_count** bytes (typically 19–547 bytes).

Each **PageRecord** is 8 bytes:

```
[object_val: u32 LE] [subject_id: u32 LE]
```

Records within each predicate block are sorted by `(object_val, subject_id)`,
enabling binary search for exact object matches.

For the heritage dataset: **86 page files**, averaging 267 KB each (22 MB total).

### Page File Anatomy

A concrete example of how the header maps to predicate blocks in the
body, and how the client fetches only what it needs:

```
page_0042.dat                                     HTTP Range
─────────────                                     requests
                                                  ─────────
byte 0     ┌──────────────────────────────┐
           │ "RMPG"  v2  pred_count: 3    │ ◄──── bytes 0–1023
           │                              │       (1 KB probe)
           │ entry[0]: pred=1   off=43    │
           │           count=150          │
           │ entry[1]: pred=5   off=1243  │
           │           count=80           │
           │ entry[2]: pred=9   off=1883  │
           │           count=200          │
byte 43    ├──────────────────────────────┤
           │ Block for pred 1             │
           │ 150 records × 8 bytes        │ ◄──── bytes 43–1243
           │                              │       (fetched if pred 1
           │  obj_val=2    subj_id=17     │        is in the query)
           │  obj_val=2    subj_id=84     │
           │  obj_val=5    subj_id=3      │
           │  ...        (sorted)         │
byte 1243  ├──────────────────────────────┤
           │ Block for pred 5             │
           │ 80 records × 8 bytes         │       (skipped — not needed)
           │  ...                         │
byte 1883  ├──────────────────────────────┤
           │ Block for pred 9             │ ◄──── bytes 1883–3483
           │ 200 records × 8 bytes        │       (fetched if pred 9
           │  ...        (sorted)         │        is in the query)
byte 3483  └──────────────────────────────┘
```

Records within each block are sorted by `(object_val, subject_id)`.
Binary search locates exact matches in O(log N) — a block of 150
records requires at most 8 comparisons.

### Quantization Strategy

Every record is exactly 8 bytes. The `object_val` encoding depends on
the predicate's alizarin datatype:

```
  alizarin datatype              object_val encoding         precision
  ─────────────────              ───────────────────         ─────────

  concept / concept-list    ───► dictionary.intern(URI)      exact
  resource-instance         ───► dictionary.intern(URI)      exact
  domain-value              ───► dictionary.intern(URI)      exact

  boolean                   ───► 0 or 1                      exact

  date / edtf               ───► days since epoch            day
                                 (offset by 2³¹ so           (pre-epoch
                                  dates sort correctly)       dates ok)

  geojson-feature-collection───► 2D Hilbert(u16, u16)        ~500 m
                                 centroid mapped to           (360°/65536
                                 u16 × u16 grid              ≈ 0.0055°)

  string / number / url     ───► (not indexed — appears
                                  only in full RDF export)
```

All encodings produce sortable u32 values: binary search works uniformly
regardless of the underlying data type.

## Page Metadata (`page_meta.json`)

JSON array of page descriptors. Used at init to map page IDs to file paths
and provide bounding-box information for spatial queries.

```json
[
  {
    "page_id": 0,
    "resource_count": 2000,
    "graph_id": "22477f01-...",
    "bbox": { "min_x": -8.17, "min_y": 53.27, "max_x": -5.43, "max_y": 55.38 }
  },
  ...
]
```

See [Binary Formats](../reference/binary-formats.md) for byte-level
specifications.

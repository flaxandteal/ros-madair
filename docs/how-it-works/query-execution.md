# Query Execution

This page walks through the four phases of query execution in detail, with
two worked examples from the heritage dataset (162,542 resources across
86 pages).

## Query Data Flow

The four phases form a pipeline. Each phase narrows the scope before the
next touches the network, so the client never reads more data than it
needs:

```
 ┌──────────────────────────────────────────────────────────────────┐
 │  SPARQL patterns                                                 │
 │  ?place <monument_type> <concept_A> .                            │
 │  ?place <townland> <townland_T> .                                │
 └──────────────────────────────┬───────────────────────────────────┘
                                │
 ═══════════════════════════════╪═══ ONCE (init) ═══════════════════
 PHASE 1                       │
 Fetch entire files:           │     22 MB total → all in memory
   summary.bin    (12 MB)      │
   dictionary.bin (15 MB)      │
   page_meta.json              │
   resource_map.bin            │
 ═══════════════════════════════╪═══ ZERO network ══════════════════
 PHASE 2 — Plan                │
                               ▼
   dictionary         "monument_type" → pred 22192
     URI → dict_id    "concept_A"     → obj  25110

   summary (OPS)      page_o=25110, pred=22192 → 51 pages
                      page_o=T_id,  pred=town  → 21 pages

   intersect on       ?place shared ──────────→ 21 pages
   shared variable

   subtract cache     (first query = nothing cached)

   output             FetchPlan: 21 pages × 2 predicates each
 ═══════════════════════════════╪═══ SELECTIVE network ═════════════
 PHASE 3 — Fetch               │
                               ▼
   per page:     ┌─ page file ──────────────────────────────┐
                 │  ┌───────────────────────────────────┐   │
     1 KB probe ─┼─►│ header: pred offsets + counts     │   │
                 │  └───────────────────────────────────┘   │
     N KB range ─┼─►│ monument_type block (sorted recs) │   │
                 │  └───────────────────────────────────┘   │
     N KB range ─┼─►│ townland block      (sorted recs) │   │
                 │  └───────────────────────────────────┘   │
                 │    (other predicate blocks — skipped)     │
                 └──────────────────────────────────────────┘
                 ~540 KB total across 21 pages (2% of data)
 ═══════════════════════════════╪═══ IN MEMORY ═════════════════════
 PHASE 4 — Execute             │
                               ▼
   per block:    binary search for object_val == 25110
                   → set S₁ of subject_ids
                 binary search for object_val == T_id
                   → set S₂ of subject_ids

   intersect     S₁ ∩ S₂ → 164 matching subject_ids

   dictionary    subject_id → URI → result set
     dict_id → URI
```

Each subsequent query re-enters at Phase 2. The cache eliminates Phase 3
requests for any (page, predicate) pairs already loaded by earlier
queries.

## Phase 1: Initialisation (once)

```
1. Fetch summary.bin    (12 MB)  → parse into three sorted quad arrays
2. Fetch dictionary.bin (15 MB)  → parse into bidirectional term↔ID map
3. Fetch page_meta.json          → parse page ID list with bounding boxes
```

After init, the client holds the full summary and dictionary in memory.
No page files have been fetched yet.

## Phase 2: Query Planning (per query, no network)

Given triple patterns like:

```sparql
?place <monument_type_n1> <concept_A> .
?place <townland> <townland_T> .
```

The planner:

1. **Translates URIs to dictionary IDs** — e.g., `monument_type_n1 → 22192`,
   `concept_A → 25110`.

2. **Looks up each pattern in the summary index:**
    - `?place monument_type_n1 <concept_A>` → OPS lookup for
      `(page_o=25110, pred=22192)` → returns the set of `page_s` values
      (pages containing resources that have monument type A).
    - `?place townland <townland_T>` → OPS lookup for
      `(page_o=<townland_T_id>, pred=<townland_id>)` → another set of pages.

3. **Intersects page sets** — both patterns share the variable `?place`, so
   only pages appearing in *both* sets can have matching resources. This is
   the most important optimisation for multi-predicate queries.

4. **Collects predicate IDs** needed from each surviving page (both
   `monument_type_n1` and `townland` blocks will be needed).

5. **Reduces by cache** — if some pages were loaded by a previous query,
   the plan is trimmed to only fetch missing (page, predicate) pairs.

Output: a `FetchPlan` listing specific pages and the predicate blocks
needed from each.

## Phase 3: Fetch (per query, selective network)

For each page in the plan:

1. **Header probe**: one HTTP Range request for the first 1,024 bytes.
   This always covers the full page header (max observed: 547 bytes).
   The header reveals the byte offset and record count of every predicate
   block in the file.

2. **Predicate block fetch**: one Range request per needed predicate.
   Fetches only the relevant slice of the file — e.g., bytes 189,759–193,983
   for the `monument_type_n1` block, skipping all other predicates.

So each page costs **2 HTTP requests** (1 header + 1 predicate block for
single-predicate queries, or 1 + N for N predicates). The client never
downloads an entire page file.

## Phase 4: Execution (per query, in-memory)

For each pattern, the executor scans all loaded predicate blocks:

- **Bound object** (`?place monument_type_n1 <concept_A>`): binary search
  within the sorted records for `object_val == 25110`. Returns matching
  `subject_id` values.
- **Variable object** (`?place monument_type_n1 ?type`): collect all
  `subject_id` values from the block (no filtering).

Result sets from multiple patterns are **intersected** — only resources
appearing in every set are returned. Subject IDs are resolved back to
URIs via the dictionary.

---

## Worked Examples

### Example 1: Single Predicate — "All monuments of type A"

```
Query:  ?place <monument_type_n1> <concept_43bf...>
Data:   162,542 resources across 86 pages
Result: 6,897 matching resources
```

**Planning:**

- Dictionary lookup: `monument_type_n1 → 22192`, `concept_43bf → 25110`
- OPS index lookup: `(page_o=25110, pred=22192)` → **51 pages** (out of 86)

**Fetching:**

- 51 header probes × 1,024 bytes = ~51 KB
- 51 predicate block fetches = ~488 KB
- **Total: ~539 KB across 102 HTTP requests**
- That's roughly **2.3% of the 22 MB** total page data

**Execution:**

- Binary search each block for `object_val == 25110`
- Union results across pages → 6,897 subject IDs
- Resolve to URIs via dictionary

### Example 2: Two-Predicate Intersection — "Type A in townland T"

```
Query:  ?place <monument_type_n1> <concept_A> .
        ?place <townland> <townland_T> .
Result: 164 matching resources
```

**Planning:**

- Pattern 1 (monument type A): OPS lookup → **51 pages**
- Pattern 2 (townland T): OPS lookup → **21 pages**
- Both share variable `?place` → intersect → **21 pages**

**Fetching:**

- 21 pages × 2 predicates × (1 header + 1 block) = **42 HTTP requests**
- Without intersection, this would be 144 requests (72 + 72)

**Execution:**

- Binary search monument_type blocks for concept A → set S₁
- Binary search townland blocks for townland T → set S₂
- Intersect S₁ ∩ S₂ → 164 results

The intersection at the planning stage avoided fetching 30 pages that
had monument type A but not townland T — a **71% reduction** in requests.

---

## Cache Behaviour

The page cache tracks which (page, predicate) pairs have been loaded. When
a subsequent query needs a page/predicate that's already in memory, the
fetch plan skips it entirely.

For example, after running "all monuments of type A" (which loads the
`monument_type_n1` block from 51 pages), a follow-up query for
"type A in townland T" only needs to fetch the `townland` blocks from 21
pages — the `monument_type_n1` blocks are already cached.

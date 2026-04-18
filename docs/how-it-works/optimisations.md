# Optimisations

## Summary

| Technique | What it saves | Impact |
|-----------|--------------|--------|
| **Summary index** | Avoids scanning all pages to find which ones are relevant | Planning is O(log N) lookups with no network |
| **Page-set intersection** | Multi-predicate queries only fetch pages satisfying all predicates | 60–70% fewer pages for 2+ predicate queries |
| **HTTP Range requests** | Fetch only needed predicate blocks, not whole page files | Transfers ~2% of total data for typical queries |
| **1 KB header probe** | Single request covers the full page header | Eliminates 1 HTTP request per page (was 3, now 2) |
| **Predicate partitioning** | Records grouped by predicate within each page | Only fetch relevant predicate blocks |
| **Sorted records + binary search** | O(log N) lookup within each predicate block | Fast execution even on large blocks |
| **Page cache** | Track which (page, predicate) pairs are already loaded | Subsequent queries reuse data, zero redundant fetches |
| **Hilbert page assignment** | Co-locate geographically and semantically similar resources | Queries on local areas touch fewer pages |
| **Dictionary encoding** | 4-byte IDs instead of full URIs in all index structures | ~10× smaller records and summaries |

## Summary Index

The summary index is a page-level quad index loaded in full at init time. For
each (page, predicate, object) combination in the dataset, it stores the count
of matching edges and distinct subjects. Three sorted copies of the same data
(SPO, PSO, OPS) support different access patterns with O(log N) binary search.

This is what makes zero-fetch query *planning* possible — the planner can
identify exactly which pages contain relevant data without touching any page
files.

## Page-Set Intersection

For multi-predicate queries sharing a variable (e.g., `?place`), the planner
intersects the page sets returned by each pattern's summary lookup. Only pages
appearing in *all* sets need to be fetched.

**Example:** A query for "monuments of type A in townland T":

```
  Summary lookup: ?place <monument_type> <concept_A>
  ────────────────────────────────────────────────────────────
  Pages with type A:     [0  1  2  3  5  7  8 10 11 ... ]  51 pages

  Summary lookup: ?place <townland> <townland_T>
  ────────────────────────────────────────────────────────────
  Pages with townland T: [1  3 12 15 18 21 24 28 30 ... ]  21 pages

  Intersection (both share ?place):
  ────────────────────────────────────────────────────────────
  Fetch only:            [1  3 15 18 28 .................]  21 pages

  Skipped:  30 pages that have type A but NOT townland T ──► 71% fewer
```

Without intersection: 72 pages (union). With intersection: 21 pages — a
**71% reduction**.

## HTTP Range Requests

Page files are structured with a header followed by predicate-partitioned
blocks. The client uses HTTP Range requests (`Range: bytes=start-end`) to
fetch only the parts it needs:

1. First 1,024 bytes for the header (reveals block offsets)
2. Specific byte ranges for needed predicate blocks

A page file averaging 267 KB might yield only 5–10 KB of relevant predicate
data per query. The server must support `206 Partial Content` responses — most
production servers (nginx, Apache, S3, CloudFront) do this natively.

```
  page_0042.dat (267 KB on disk)

  byte 0        ┌───────────────────────────────┐
                │         Header (43 B)         │◄── Range: bytes=0-1023
                │  pred=1  off=43     count=150 │    (1 KB probe — one
                │  pred=5  off=1243   count=80  │     request covers the
                │  pred=9  off=1883   count=200 │     whole header)
  byte 43       ├ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┤
                │ Block: pred 1  (1200 B)       │    (not needed — skipped)
  byte 1243     ├ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┤
                │ Block: pred 5  (640 B)        │◄── Range: bytes=1243-1882
  byte 1883     ├ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┤    (query needs pred 5)
                │ Block: pred 9  (1600 B)       │    (not needed — skipped)
  byte 3483     └───────────────────────────────┘

  Transferred:  1 KB + 640 B  =  ~1.6 KB   (0.6% of the 267 KB file)
```

Each predicate block is independently addressable because the header
records the exact byte offset and record count. The client picks only
the blocks it needs — everything else stays on the server.

## Header Probe Optimisation

The initial header fetch uses a 1,024-byte probe. Since all observed page
headers fit within 547 bytes, this single request always covers the full
header — eliminating what would otherwise be a second round-trip to fetch
remaining header data.

This reduces per-page requests from 3 (header size probe + full header +
predicate data) to 2 (header probe + predicate data).

## Page Cache

The cache tracks loaded (page ID, predicate ID) pairs. When the planner
produces a fetch plan, the cache removes entries that have already been
loaded by previous queries. This makes interactive query sessions
progressively faster — each new query benefits from data loaded by all
prior queries.

## Hilbert Page Assignment

Resources are sorted by a 3D Hilbert space-filling curve before being
sliced into pages. The three dimensions are:

1. **Longitude** (normalised to [0, 1024))
2. **Latitude** (normalised to [0, 1024))
3. **Concept bucket** (hash of concept URIs, scaled to [0, 1024))

The Hilbert curve preserves locality in all three dimensions simultaneously,
so resources that are geographically close *and* share similar concept types
end up on the same page. This means a query targeting a specific geographic
area and concept type will touch fewer pages than random assignment.

```
  Page assignment: random vs Hilbert

  Random assignment              Hilbert assignment
  (resources scattered)          (resources clustered)

  ┌───────────────────────┐      ┌───────────────────────┐
  │ ·₁ ·₃    ·₂  ·₁      │      │ ·₀ ·₀    ·₁  ·₁      │
  │    ·₂  ·₁        ·₃  │      │    ·₀  ·₀        ·₁  │
  │       ·₁  ·₃  ·₂     │      │       ·₀  ·₁  ·₁     │
  │ ·₃       ·₁          │      │ ·₂       ·₂          │
  │    ·₂  ·₃     ·₁ ·₂  │      │    ·₂  ·₂     ·₃ ·₃  │
  │ ·₁        ·₃  ·₂     │      │ ·₂        ·₃  ·₃     │
  └───────────────────────┘      └───────────────────────┘

  Query for NE quadrant:         Query for NE quadrant:
  touches pages 1,2,3 (all)      touches page 1 only
```

The subscript numbers represent page IDs. With Hilbert assignment,
nearby resources share pages, so spatial queries touch fewer pages.

## Trade-offs

### Page Size

Page size is the main tuning parameter. Larger pages mean:

- Fewer pages, so fewer HTTP requests per query
- More false positives (irrelevant records loaded per page)
- Larger individual Range responses

The current default of **2,000 resources/page** is a sweet spot for the
heritage dataset (162K resources). It gives 86 pages with an average of
267 KB each. A typical single-predicate query touches ~51 pages with
~102 HTTP requests and ~540 KB transferred.

At the previous default of 200 resources/page, the same query required
~558 HTTP requests — a **5.5× increase** — because the target concept was
spread across 186 out of 282 pages.

```
  Page size trade-off (same dataset, same query)

  page_size    pages    pages     HTTP        bytes per    total
               total    touched   requests    request      transfer
  ──────────   ─────    ───────   ────────    ─────────    ────────
     200        282      186       558         ~1 KB        ~560 KB
    2000         86       51       102         ~5 KB        ~540 KB
   20000          9        9        18        ~50 KB        ~450 KB

  ◄── more pages, more requests ──────── fewer pages, larger blocks ──►
      finer spatial precision              coarser precision, more
      but diminishing returns              false positives per page
```

### Summary Index Size

The summary index is the main memory cost. At 12 MB for 160K resources,
it scales linearly and must be loaded in full. For datasets exceeding ~1M
resources, a hierarchical or range-based summary might be needed.

### Client-Side Computation

No server-side computation means the client does all the work. This is
the design goal — the server is just a static file host — but it means
query latency depends on the client's network and CPU.

# BOT Quick Reference for Arches Modelling (Aonach Mór subset)

Scope: the Building Topology Ontology (BOT) classes and properties the
Aonach Mór resilience demo uses for within-building topology — the
structural layer underneath CIDOC-CRM's asset-level view of a Building.

BOT is a lightweight W3C Community Group Report ontology published by
the W3C Linked Building Data CG. It does **not** try to cover every
aspect of BIM; it models the topological skeleton (site / building /
storey / space / element) and lets richer domain ontologies plug in on
top. For the demo we use BOT to carry keyholder scope, occupied-rooms
context, and shelter capacity tiles that need a within-building address.

## Namespace

| Prefix | URI |
|--------|-----|
| `bot`  | `https://w3id.org/bot#` |

## Core classes

| Class | Use for | Notes |
|-------|---------|-------|
| `bot:Zone`     | **Abstract super-class** — anything that is spatially bounded on a building | Parent of Site, Building, Storey, Space |
| `bot:Site`     | A site on which one or more buildings sit | Typically the plot boundary |
| `bot:Building` | A physical building — root of the topology for indoor queries | |
| `bot:Storey`   | A level of a building | |
| `bot:Space`    | A room or other bounded space within a building | |
| `bot:Element`  | A physical building element (wall, door, column, …) | Rarely bound in this demo |
| `bot:Interface` | An interface between two elements or spaces | Not bound in this demo |

For the Aonach Mór demo only `bot:Site`, `bot:Building`, `bot:Storey` and
`bot:Space` are actively used. `bot:Element` is available if a later
iteration wants to model keyholder access to specific doors.

## Core properties

### Topology (containment)

| Property | Domain → Range | Use for |
|----------|---------------|---------|
| `bot:hasBuilding`     | `bot:Site` → `bot:Building`  | Site contains buildings |
| `bot:hasStorey`       | `bot:Building` → `bot:Storey` | Building contains storeys |
| `bot:hasSpace`        | `bot:Zone` → `bot:Space`     | Any zone contains spaces |
| `bot:hasSubZone`      | `bot:Zone` → `bot:Zone`      | Generic containment |
| `bot:containsZone`    | `bot:Zone` → `bot:Zone`      | Non-topological containment |
| `bot:containsElement` | `bot:Zone` → `bot:Element`   | Zone contains elements |

### Adjacency

| Property | Domain → Range | Use for |
|----------|---------------|---------|
| `bot:adjacentZone`       | `bot:Zone` → `bot:Zone`    | Two zones are adjacent |
| `bot:adjacentElement`    | `bot:Zone` → `bot:Element` | Zone borders an element |
| `bot:hasInterface`       | `bot:Element` → `bot:Interface` | Element has an interface |
| `bot:interfaceOf`        | `bot:Interface` → `bot:Element` | Inverse |

### 3D/asset shortcuts

| Property | Domain → Range | Use for |
|----------|---------------|---------|
| `bot:has3DModel`         | `bot:Zone` → URL         | External 3D model reference |
| `bot:hasSimple3DModel`   | `bot:Zone` → URL         | Simplified 3D model reference |

## Common Arches modelling patterns

### Building with topological layer

The Aonach Mór Building resource model's primary root is
`crm:E22_Human-Made_Object` (see `cidoc-crm.md`). A `bot:Building`
tile is attached alongside to carry storey / space containment for
within-building queries.

```
Building (crm:E22_Human-Made_Object)
  ─P53_has_former_or_current_location─► Site (resource-instance: E53_Place)
  ─P168_place_is_defined_by───────────► E94_Space_Primitive (geojson: footprint)
  ─P2_has_type────────────────────────► E55_Type (concept: GEMTaxonomy)
  ─bot:hasStorey──────────────────────► Storey (semantic)
    Storey ─bot:hasSpace──────────────► Space (semantic)
      Space ─P2_has_type──────────────► E55_Type (concept: ServiceRole)
      Space ─P3_has_note──────────────► E62_String (string: room label)
```

### Site → Building containment

```
Site (crm:E53_Place)
  ─bot:hasBuilding─► Building (resource-instance-list)
```

### Keyholder scope pattern (stretch)

If a later iteration wants to model that a specific keyholder has access
to specific rooms only:

```
Person ─crm:P14_carried_out_by~─► KeyholderActivity (semantic)
  KeyholderActivity ─bot:containsZone─► Space (resource-instance)
```

## Crosswalk

| BOT class | CIDOC-CRM equivalent | Notes |
|-----------|---------------------|-------|
| `bot:Site`     | `crm:E53_Place`              | Site = place |
| `bot:Building` | `crm:E24_Physical_Human-Made_Thing` or `crm:E22_Human-Made_Object` | Building = human-made thing |
| `bot:Storey`   | `crm:E26_Physical_Feature`   | Feature of a building |
| `bot:Space`    | `crm:E26_Physical_Feature`   | Feature of a storey |
| `bot:Element`  | `crm:E22_Human-Made_Object`  | Wall / door as an object |

## Out of scope for Aonach Mór v1

- IFC interop, 3D model carrying, BIM export — not needed for exposure
  queries
- `bot:Element` binding on real building elements — the building
  footprint + GEM taxonomy class carries enough detail for hazard
  modelling
- Within-storey partitioning beyond "named space" — rooms are
  enumerated only where a specific keyholder scope or shelter capacity
  needs it

## Sources

Upstream: `https://w3id.org/bot` (W3C Linked Building Data CG Draft).
Shipped under the W3C Community Group Report terms. Staged as `bot.ttl`
(upstream Turtle) and `bot.rdf` (converted RDF/XML).

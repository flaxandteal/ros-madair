# GeoSPARQL Quick Reference for Arches Modelling (Aonach Mór subset)

Scope: the GeoSPARQL 1.1 classes and properties the Aonach Mór resilience
demo binds onto geographic tiles (Site, Building footprint, HazardFootprint,
Sensor location). GeoSPARQL is the OGC standard for spatial data in RDF;
the Aonach Mór demo ships a **static** build, so GeoSPARQL acts as a
vocabulary for the geometry tiles only — intersections are precomputed
at build time by `11_exposure.py`, not evaluated at query time.

## Namespace prefixes

| Prefix    | URI |
|-----------|-----|
| `geo`     | `http://www.opengis.net/ont/geosparql#` |
| `geof`    | `http://www.opengis.net/def/function/geosparql/` |
| `sf`      | `http://www.opengis.net/ont/sf#` |

GeoSPARQL 1.1 also defines function extensions for topological and metric
SPARQL queries (`geof:sfIntersects`, `geof:distance`, …). These are
**not** used in the static demo's query path — precomputed join tables
stand in for them.

## Core classes

### Core hierarchy

| Class | Use for | Notes |
|-------|---------|-------|
| `geo:SpatialObject` | **Top of the spatial hierarchy** | Anything with spatial semantics |
| `geo:Feature`       | **Geographic feature** — thing with an identity that has a geometry | Buildings, sites, sensors, footprints |
| `geo:Geometry`      | **The geometric shape** attached to a feature | Separate class so a feature can carry multiple representations |
| `geo:FeatureCollection` | Collection of features | |
| `geo:GeometryCollection` | Collection of geometries | |

### Simple Features geometry subclasses (from `sf:`)

Used to type concrete geometries.

| Class | Use for |
|-------|---------|
| `sf:Point`            | Point geometry (sensors, centroids) |
| `sf:LineString`       | Linear geometry (river reaches, roads) |
| `sf:Polygon`          | Polygon (building footprint, ward, hazard footprint) |
| `sf:MultiPoint`       | Collection of points |
| `sf:MultiLineString`  | Collection of lines (river network) |
| `sf:MultiPolygon`     | Collection of polygons (ward with islands, building complex) |
| `sf:GeometryCollection` | Mixed |

## Core properties

### Feature ↔ Geometry

| Property | Domain → Range | Use for |
|----------|---------------|---------|
| `geo:hasGeometry`        | `geo:Feature` → `geo:Geometry` | Feature carries one or more geometries |
| `geo:hasDefaultGeometry` | `geo:Feature` → `geo:Geometry` | The default geometry for queries |
| `geo:hasBoundingBox`     | `geo:Feature` → `geo:Geometry` | Precomputed bbox for fast filtering |
| `geo:hasCentroid`        | `geo:Feature` → `geo:Geometry` | Representative point |
| `geo:hasArea`            | `geo:Feature` → `xsd:double`   | Precomputed area |

### Geometry serialisation

| Property | Domain → Range | Use for |
|----------|---------------|---------|
| `geo:asWKT`     | `geo:Geometry` → `geo:wktLiteral`     | Well-Known Text |
| `geo:asGeoJSON` | `geo:Geometry` → `geo:geoJSONLiteral` | GeoJSON literal |
| `geo:asGML`     | `geo:Geometry` → `geo:gmlLiteral`     | GML |
| `geo:asKML`     | `geo:Geometry` → `geo:kmlLiteral`     | KML |

**In Aonach Mór we use `geo:asGeoJSON`** because Alizarin's
`geojson-feature-collection` datatype stores GeoJSON natively. WKT is
left on the side only for interoperability with Arches deployments that
expect it.

### Topological relations (for reference — not used at runtime)

| Property | Relation |
|----------|----------|
| `geo:sfIntersects`   | Intersects (Simple Features) |
| `geo:sfContains`     | Contains |
| `geo:sfWithin`       | Within |
| `geo:sfEquals`       | Equals |
| `geo:sfDisjoint`     | Disjoint |
| `geo:sfTouches`      | Touches |
| `geo:sfOverlaps`     | Overlaps |
| `geo:sfCrosses`      | Crosses |

**These are not evaluated in the static demo.** The `Exposure` resource
model is a materialised relation that records precomputed
`(Building × HazardFootprint)` intersections. The build pipeline (Stage
11) computes them once; the query engine only ever joins resources, not
geometries.

### Metric relations (for reference — not used at runtime)

| Property | Range |
|----------|-------|
| `geo:hasSerialization`  | literal |
| `geo:coordinateDimension` | `xsd:integer` |
| `geo:spatialDimension`  | `xsd:integer` |
| `geo:isEmpty`           | `xsd:boolean` |
| `geo:isSimple`          | `xsd:boolean` |

## Common Arches modelling patterns

### Feature with geometry tile

```
<Any resource with a geometry> (geo:Feature)
  ─P168_place_is_defined_by───► E94_Space_Primitive (geojson-feature-collection)
  ─geo:hasGeometry────────────► E94_Space_Primitive (geojson-feature-collection: alternative form)
  ─geo:hasCentroid────────────► E94_Space_Primitive (geojson-feature-collection: single point)
```

In practice the demo uses CIDOC-CRM's `P168_place_is_defined_by` as the
primary binding and attaches GeoSPARQL as a crosswalk. The
geojson-feature-collection node is the single source of truth for
geometry; GeoSPARQL properties are added only on resources that need to
round-trip into SPARQL-endpoint deployments.

### Site with containment and geometry

```
Site (crm:E53_Place, also geo:Feature)
  ─P87_is_identified_by────────► E44_Place_Appellation (string: ward name)
  ─P168_place_is_defined_by────► E94_Space_Primitive (geojson: polygon)
  ─geo:hasArea─────────────────► (number: m²)
  ─P89_falls_within────────────► Site (resource-instance: parent admin)
```

### HazardFootprint geometry

```
HazardFootprint
  ─P168_place_is_defined_by────► E94_Space_Primitive (geojson: polygon)
  ─geo:hasArea─────────────────► (number: m²)
  ─prov:wasGeneratedBy─────────► HazardModel (resource-instance)
  ─P2_has_type─────────────────► E55_Type (concept: HazardType)
  ─P2_has_type─────────────────► E55_Type (concept: IntensityBand)
```

## CRS

The Aonach Mór build reprojects every input into a single local
transverse-Mercator CRS. GeoJSON literals are stored in **geographic
coordinates (EPSG:4326)** for portability; any metric properties like
`geo:hasArea` are computed in the local projected CRS and cached as
numbers so the query path does not need a geodesy library.

## Crosswalk

| GeoSPARQL class / property | CIDOC-CRM equivalent |
|---------------------------|---------------------|
| `geo:Feature`       | `crm:E53_Place` (when the feature represents a place) or `crm:E1_CRM_Entity` generally |
| `geo:Geometry`      | `crm:E94_Space_Primitive` |
| `geo:hasGeometry`   | `crm:P168_place_is_defined_by` (when the feature is a place) |
| `geo:asGeoJSON`     | native payload of `E94_Space_Primitive` |

## Out of scope for Aonach Mór v1

- Runtime GeoSPARQL function evaluation (`geof:*`) — handled by
  precomputed joins
- Multi-CRS geometries per feature — single CRS per layer, one
  reprojection step at build time
- `geo:hasSerialization` multi-format round-tripping — GeoJSON only

## Sources

Upstream: `https://opengeospatial.github.io/ogc-geosparql/geosparql11/`
(GeoSPARQL 1.1, OGC 22-047r1). Shipped under the OGC Document Licence.
Staged as `geosparql_v1_1.ttl` (upstream Turtle) and `geosparql_v1_1.rdf`
(converted RDF/XML).

# Aonach Mór — Stage 1 geographic sources

All files in this directory are raw Chennai geographic data fetched from OpenStreetMap via the Overpass API and written as plain GeoJSON in EPSG:4326. No renaming is applied — Stage 2 reads these files, runs the Chennai → Aonach Mór label substitution, and writes the results to `../renamed/`.

- **Fetched on**: 2026-04-11
- **Fetched by**: `example/aonach_mor/build/01_geography.py`
- **Endpoint(s)**: `https://overpass-api.de/api/interpreter`, `https://overpass.kumi.systems/api/interpreter`, `https://overpass.private.coffee/api/interpreter`
- **Licence**: Open Data Commons Open Database License (ODbL),
  © OpenStreetMap contributors

## Layers

| Layer | File | Feature count | Description |
|-------|------|---------------|-------------|
| `gcc_boundary` | `gcc_boundary.geojson` | 1 | Greater Chennai Corporation administrative boundary (admin_level=6). |
| `ward_boundaries` | `ward_boundaries.geojson` | 186 | GCC ward boundaries (admin_level=10) within the study area bounding box. Stage 2 matches seed points to polygons to resolve the 13 iconic wards. |
| `coastline` | `coastline.geojson` | 33 | OSM natural=coastline ways clipped to the study bbox — the Bay of Bengal shoreline. |
| `rivers` | `rivers.geojson` | 61 | Adyar, Cooum and Buckingham Canal waterways plus any other OSM-tagged river/canal features in the bbox. |
| `roads_primary` | `roads_primary.geojson` | 2157 | Motorway / trunk / primary road network for the study bbox. |
| `neighbourhoods` | `neighbourhoods.geojson` | 633 | `place` features (suburb / neighbourhood / quarter / locality) within the bbox — the point or polygon labels for the 13 iconic neighbourhood names. |

## Local-only artefacts

| File | Description |
|------|-------------|
| `crs.json` | Aonach Mór local transverse-Mercator CRS parameters (proj4 + metadata). GeoJSON stays in EPSG:4326; this CRS is used by later stages for metric area and distance computations. |
| `bbox.geojson` | Study-area bounding box polygon computed from `iconic_wards.json`. Used as the spatial filter for every Overpass query. |
| `iconic_wards.json` | The 13 iconic Chennai wards with their cluster classification and seed points for ward polygon resolution. |

## Known limitations

- Multi-polygon assembly handles outer rings only; inner rings (holes) are dropped. Enclaved wards are therefore slightly over-covered in the demo.
- Outer rings that Overpass returns as multiple ways are emitted as separate closed polygons rather than being stitched into a single ring. The stitched version is a Stage 2 responsibility.
- Building footprints are **not** fetched at Stage 1 — they come from Overture Maps at Stage 4 (`04_buildings.py`).
- Bhuvan (ISRO) and Survey of India sources listed in `ATTRIBUTION.md` are aspirational — Stage 1 uses the OSM-only path. A future iteration can add an authenticated fetch for NDSAP / NGP-2021 data without changing the on-disk format.

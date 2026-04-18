#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 Flax & Teal Limited
"""Stage 1 — Chennai geographic ingest for the Aonach Mór demo.

Pulls the geographic frame on which every downstream stage hangs:
coastline, major rivers, Greater Chennai Corporation boundary, ward
boundaries, neighbourhood labels, and primary/trunk road network. Outputs
go to ``example/aonach_mor/geo/raw/`` as plain GeoJSON so later stages
are format-agnostic. **No renaming happens at this stage** — the raw
Chennai labels (ward numbers, river names, suburb names) are preserved
so reviewers can trace every downstream label back to its upstream
source.

Source: OpenStreetMap via the public Overpass API (`overpass-api.de`).
OSM is ODbL; attribution is already recorded in
``example/aonach_mor/ATTRIBUTION.md``.

The plan names Overture Maps and Bhuvan / Survey of India as the
preferred sources for building footprints and authoritative admin
boundaries. Those both have significant integration costs (Overture
needs a DuckDB + S3 client; Bhuvan and SoI need account-gated HTTPS
downloads), so Stage 1 ships with the OSM-only path and leaves hooks
for the others to be plugged in later without breaking the on-disk
format.

Run from the repository root:

    python example/aonach_mor/build/01_geography.py             # fetch all
    python example/aonach_mor/build/01_geography.py --only coastline,rivers
    python example/aonach_mor/build/01_geography.py --dry-run   # show plan
    python example/aonach_mor/build/01_geography.py --offline   # no network
    python example/aonach_mor/build/01_geography.py --force     # re-fetch

Env overrides:

    AONACH_GEO_DIR   target dir (default: example/aonach_mor/geo)
    OVERPASS_URL     Overpass endpoint (default: overpass-api.de)
    NETWORK=0        equivalent to --offline

The script is idempotent: re-running is safe. On-disk files are left
alone unless --force is passed or the layer is missing.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import json
import os
import sys
import textwrap
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable

# --------------------------------------------------------------------------- #
# Configuration                                                               #
# --------------------------------------------------------------------------- #

REPO_ROOT = Path(__file__).resolve().parents[3]
AONACH_DIR = REPO_ROOT / "example" / "aonach_mor"
GEO_DIR = Path(
    os.environ.get("AONACH_GEO_DIR", str(AONACH_DIR / "geo"))
).resolve()
RAW_DIR = GEO_DIR / "raw"

# Overpass endpoints. The public `overpass-api.de` instance is our
# primary but it gets rate-limited and 504s under load, so we fall back
# to two mirrors run by community operators. Set ``OVERPASS_URL`` to
# override the list with a single endpoint (useful for pinning to a
# self-hosted Overpass during CI).
OVERPASS_ENDPOINTS = (
    [os.environ["OVERPASS_URL"]]
    if "OVERPASS_URL" in os.environ
    else [
        "https://overpass-api.de/api/interpreter",
        "https://overpass.kumi.systems/api/interpreter",
        "https://overpass.private.coffee/api/interpreter",
    ]
)
USER_AGENT = "RosMadair-AonachMor/0.1 (+https://github.com/flaxandteal/RosMadair)"
HTTP_TIMEOUT = 180  # seconds — Overpass queries can be slow
MAX_RETRIES = 3

# Local transverse-Mercator CRS for Aonach Mór. Not an EPSG-registered
# CRS; this is a project-internal choice. The origin sits on Chennai's
# roughly-central meridian and low-latitude, with a Mercator scale
# factor that keeps metric distortion below ~0.03% across the study
# area. Stored as a proj4 string for consumption by pyproj (in later
# stages); Stage 1 itself does not reproject — GeoJSON outputs stay in
# EPSG:4326 for portability, matching the GeoSPARQL vocabulary
# convention (``vocab/geosparql/REFERENCE.md``).
LOCAL_TM_CRS = {
    "name": "Aonach Mór local TM",
    "proj4": (
        "+proj=tmerc +lat_0=13.0 +lon_0=80.25 +k=0.9996 "
        "+x_0=500000 +y_0=1000000 +ellps=WGS84 +datum=WGS84 "
        "+units=m +no_defs"
    ),
    "wkt_note": (
        "Not registered with EPSG. Project-internal CRS with TM "
        "centred on Chennai's central meridian. Downstream area / "
        "distance computations use this; stored GeoJSON remains in "
        "EPSG:4326."
    ),
    "center_lon_lat": [80.25, 13.0],
    "lon_of_origin": 80.25,
    "lat_of_origin": 13.0,
    "scale_factor": 0.9996,
    "false_easting_m": 500000,
    "false_northing_m": 1000000,
    "ellipsoid": "WGS84",
}


@dataclass(frozen=True)
class IconicWard:
    """One of the 13 "iconic" Chennai wards the demo focuses on.

    ``cluster`` corresponds to entries in the ``ClusterType`` concept
    collection (coastal, riverine, commercial, port, heritage,
    peri-urban). ``seed_lon``/``seed_lat`` are representative points
    inside the ward — Stage 1 uses them to pick the right polygon out
    of the GCC ward layer once it has been fetched, and Stage 2's
    renaming pipeline attaches the Aonach Mór alias to the polygon
    that contains the seed.
    """

    key: str
    chennai_label: str
    cluster: str
    seed_lon: float
    seed_lat: float


# Seed points were chosen as "obviously in this neighbourhood" locations;
# they are deliberately approximate — Stage 1 does not need precise
# ward coordinates, only something that falls inside the right polygon.
# Picked from public-domain reference maps; no coordinates are
# sensitive or personally identifiable.
ICONIC_WARDS: list[IconicWard] = [
    IconicWard("marina", "Marina Beach", "coastal", 80.2830, 13.0500),
    IconicWard("besant_nagar", "Besant Nagar", "coastal", 80.2670, 12.9985),
    IconicWard("foreshore_estate", "Foreshore Estate", "coastal", 80.2800, 13.0380),
    IconicWard("kotturpuram", "Kotturpuram", "riverine", 80.2440, 13.0175),
    IconicWard("saidapet", "Saidapet", "riverine", 80.2270, 13.0220),
    IconicWard("t_nagar", "T. Nagar", "commercial", 80.2340, 13.0410),
    IconicWard("nungambakkam", "Nungambakkam", "commercial", 80.2410, 13.0580),
    IconicWard("royapuram", "Royapuram", "port", 80.2940, 13.1010),
    IconicWard("ennore_fringe", "Ennore fringe", "port", 80.3220, 13.2150),
    IconicWard("triplicane", "Triplicane", "heritage", 80.2760, 13.0570),
    IconicWard("mylapore", "Mylapore", "heritage", 80.2680, 13.0330),
    IconicWard("velachery", "Velachery", "peri-urban", 80.2200, 12.9770),
    IconicWard("omr_edge", "OMR corridor edge", "peri-urban", 80.2410, 12.9340),
]


def _study_area_bbox(wards: list[IconicWard], padding_deg: float = 0.04) -> tuple[float, float, float, float]:
    """Return ``(south, west, north, east)`` padded bbox covering all seeds.

    Overpass's bbox filter uses ``(south, west, north, east)`` ordering,
    so we return it in that order to avoid confusion at the call site.
    """
    lons = [w.seed_lon for w in wards]
    lats = [w.seed_lat for w in wards]
    return (
        min(lats) - padding_deg,
        min(lons) - padding_deg,
        max(lats) + padding_deg,
        max(lons) + padding_deg,
    )


STUDY_BBOX = _study_area_bbox(ICONIC_WARDS)


def _log(msg: str) -> None:
    print(f"[01_geography] {msg}", file=sys.stderr)


def _is_offline(flag: bool) -> bool:
    return flag or os.environ.get("NETWORK", "1") == "0"


# --------------------------------------------------------------------------- #
# Overpass client                                                             #
# --------------------------------------------------------------------------- #


class OverpassError(RuntimeError):
    pass


def _post_once(endpoint: str, query: str) -> dict[str, Any]:
    data = query.encode("utf-8")
    req = urllib.request.Request(
        endpoint,
        data=data,
        method="POST",
        headers={
            "User-Agent": USER_AGENT,
            "Content-Type": "application/x-www-form-urlencoded",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=HTTP_TIMEOUT) as resp:
            body = resp.read()
    except urllib.error.URLError as exc:
        raise OverpassError(f"{endpoint}: {exc}") from exc
    try:
        return json.loads(body)
    except json.JSONDecodeError as exc:
        raise OverpassError(
            f"{endpoint}: non-JSON response ({len(body)} bytes)"
        ) from exc


def _overpass_query(query: str, *, dry_run: bool) -> dict[str, Any] | None:
    """POST an OverpassQL query and return the parsed JSON response.

    Returns ``None`` when called with ``dry_run=True`` after logging the
    query text. Tries each endpoint in ``OVERPASS_ENDPOINTS`` up to
    ``MAX_RETRIES`` times, then raises ``OverpassError``.
    """
    if dry_run:
        _log("dry-run query:\n" + textwrap.indent(query.strip(), "    "))
        return None

    errors: list[str] = []
    for attempt in range(MAX_RETRIES):
        for endpoint in OVERPASS_ENDPOINTS:
            try:
                return _post_once(endpoint, query)
            except OverpassError as exc:
                msg = f"attempt {attempt + 1}: {exc}"
                errors.append(msg)
                _log(f"overpass: {msg}")
    raise OverpassError(
        "all Overpass endpoints failed:\n  " + "\n  ".join(errors)
    )


# --------------------------------------------------------------------------- #
# Overpass → GeoJSON conversion                                               #
# --------------------------------------------------------------------------- #
#
# Overpass's `out geom` format is close to, but not quite, GeoJSON. We
# handle the three shapes we actually query for:
#
#   - `node` — emit as GeoJSON Point
#   - `way` — emit as LineString, or Polygon when the first and last
#     coordinates coincide
#   - `relation` with `type=multipolygon` — assemble outer rings into a
#     MultiPolygon (inner rings are ignored in v1; this over-covers
#     enclaved wards but keeps the converter small enough to not need
#     a geometry dependency)
#
# The converter deliberately skips any element it does not understand so
# the rest of the layer is not lost on one bad record.


def _way_to_coords(way: dict[str, Any]) -> list[list[float]]:
    geom = way.get("geometry") or []
    return [[pt["lon"], pt["lat"]] for pt in geom]


def _is_closed(coords: list[list[float]]) -> bool:
    return len(coords) >= 4 and coords[0] == coords[-1]


def _element_to_feature(element: dict[str, Any]) -> dict[str, Any] | None:
    kind = element.get("type")
    tags = element.get("tags") or {}
    properties: dict[str, Any] = {
        "osm_type": kind,
        "osm_id": element.get("id"),
        **tags,
    }

    if kind == "node":
        return {
            "type": "Feature",
            "properties": properties,
            "geometry": {
                "type": "Point",
                "coordinates": [element["lon"], element["lat"]],
            },
        }

    if kind == "way":
        coords = _way_to_coords(element)
        if len(coords) < 2:
            return None
        if _is_closed(coords):
            return {
                "type": "Feature",
                "properties": properties,
                "geometry": {"type": "Polygon", "coordinates": [coords]},
            }
        return {
            "type": "Feature",
            "properties": properties,
            "geometry": {"type": "LineString", "coordinates": coords},
        }

    if kind == "relation":
        outers: list[list[list[float]]] = []
        for member in element.get("members") or []:
            if member.get("type") != "way":
                continue
            if member.get("role") not in ("outer", ""):
                continue
            coords = _way_to_coords(member)
            if len(coords) >= 2:
                if not _is_closed(coords):
                    # Overpass splits some outer rings across multiple
                    # member ways. A proper implementation would stitch
                    # them on matching endpoints; v1 just emits each
                    # open ring as a separate LineString-in-Polygon by
                    # closing it artificially. Note this in SOURCES.md
                    # so reviewers know shapes are approximate.
                    coords = coords + [coords[0]]
                outers.append(coords)
        if not outers:
            return None
        if len(outers) == 1:
            return {
                "type": "Feature",
                "properties": properties,
                "geometry": {"type": "Polygon", "coordinates": outers},
            }
        return {
            "type": "Feature",
            "properties": properties,
            "geometry": {
                "type": "MultiPolygon",
                "coordinates": [[ring] for ring in outers],
            },
        }

    return None


def _overpass_to_geojson(response: dict[str, Any]) -> dict[str, Any]:
    elements = response.get("elements") or []
    features: list[dict[str, Any]] = []
    for el in elements:
        feat = _element_to_feature(el)
        if feat is not None:
            features.append(feat)
    return {
        "type": "FeatureCollection",
        "metadata": {
            "osm3s": response.get("osm3s"),
            "generator": response.get("generator"),
            "element_count": len(elements),
            "feature_count": len(features),
        },
        "features": features,
    }


# --------------------------------------------------------------------------- #
# Layer definitions                                                           #
# --------------------------------------------------------------------------- #


@dataclass
class Layer:
    """One Overpass-driven GeoJSON layer."""

    key: str
    filename: str
    description: str
    query: Callable[[], str]

    def path(self) -> Path:
        return RAW_DIR / self.filename


def _bbox_clause() -> str:
    s, w, n, e = STUDY_BBOX
    return f"{s},{w},{n},{e}"


def _q_coastline() -> str:
    return textwrap.dedent(
        f"""\
        [out:json][timeout:180];
        (
          way["natural"="coastline"]({_bbox_clause()});
        );
        out geom;
        """
    )


def _q_rivers() -> str:
    # Buckingham Canal, Cooum and Adyar are the three headline
    # waterways running through the study area. We grab both the
    # relation-mapped river objects (where present) and tagged ways so
    # the fetch is robust to OSM modelling variation.
    bbox = _bbox_clause()
    return textwrap.dedent(
        f"""\
        [out:json][timeout:180];
        (
          relation["waterway"~"river|canal"]["name"~"Adyar|Cooum|Buckingham"]({bbox});
          way["waterway"~"river|canal"]["name"~"Adyar|Cooum|Buckingham"]({bbox});
          way["waterway"="river"]({bbox});
          way["waterway"="canal"]({bbox});
        );
        out geom;
        """
    )


def _q_gcc_boundary() -> str:
    # Bbox-bound the GCC query so Overpass does not have to do an
    # unbounded admin-level index scan — the public instance rate-limits
    # global searches and 504s under load.
    return textwrap.dedent(
        f"""\
        [out:json][timeout:180];
        (
          relation["boundary"="administrative"]["admin_level"="6"]["name"="Chennai"]({_bbox_clause()});
          relation["boundary"="administrative"]["admin_level"="5"]["name"~"Chennai"]({_bbox_clause()});
        );
        out geom;
        """
    )


def _q_ward_boundaries() -> str:
    # Chennai's admin_level=10 carries the municipal ward boundaries
    # (numbered 1..200). We take everything in the bbox; Stage 2 picks
    # out the ones whose centroids contain a seed point.
    return textwrap.dedent(
        f"""\
        [out:json][timeout:240];
        (
          relation["boundary"="administrative"]["admin_level"="10"]({_bbox_clause()});
        );
        out geom;
        """
    )


def _q_neighbourhood_labels() -> str:
    # place=suburb / neighbourhood / quarter for the 13 iconic names.
    # These are often point features, sometimes polygons — our
    # converter handles both.
    bbox = _bbox_clause()
    return textwrap.dedent(
        f"""\
        [out:json][timeout:120];
        (
          node["place"~"suburb|neighbourhood|quarter|locality"]({bbox});
          way["place"~"suburb|neighbourhood|quarter|locality"]({bbox});
          relation["place"~"suburb|neighbourhood|quarter|locality"]({bbox});
        );
        out geom;
        """
    )


def _q_roads_primary() -> str:
    # Primary and trunk highway network only. Stage 1 deliberately
    # skips residential and service roads — they balloon the file size
    # for little analytical value at the demo scale.
    return textwrap.dedent(
        f"""\
        [out:json][timeout:180];
        (
          way["highway"~"motorway|trunk|primary"]({_bbox_clause()});
        );
        out geom;
        """
    )


LAYERS: list[Layer] = [
    Layer(
        "gcc_boundary",
        "gcc_boundary.geojson",
        "Greater Chennai Corporation administrative boundary "
        "(admin_level=6).",
        _q_gcc_boundary,
    ),
    Layer(
        "ward_boundaries",
        "ward_boundaries.geojson",
        "GCC ward boundaries (admin_level=10) within the study area "
        "bounding box. Stage 2 matches seed points to polygons to "
        "resolve the 13 iconic wards.",
        _q_ward_boundaries,
    ),
    Layer(
        "coastline",
        "coastline.geojson",
        "OSM natural=coastline ways clipped to the study bbox — the "
        "Bay of Bengal shoreline.",
        _q_coastline,
    ),
    Layer(
        "rivers",
        "rivers.geojson",
        "Adyar, Cooum and Buckingham Canal waterways plus any other "
        "OSM-tagged river/canal features in the bbox.",
        _q_rivers,
    ),
    Layer(
        "roads_primary",
        "roads_primary.geojson",
        "Motorway / trunk / primary road network for the study bbox.",
        _q_roads_primary,
    ),
    Layer(
        "neighbourhoods",
        "neighbourhoods.geojson",
        "`place` features (suburb / neighbourhood / quarter / "
        "locality) within the bbox — the point or polygon labels for "
        "the 13 iconic neighbourhood names.",
        _q_neighbourhood_labels,
    ),
]


# --------------------------------------------------------------------------- #
# Local-only outputs                                                          #
# --------------------------------------------------------------------------- #


def _write_crs_json() -> Path:
    path = RAW_DIR / "crs.json"
    path.write_text(json.dumps(LOCAL_TM_CRS, indent=2) + "\n", encoding="utf-8")
    return path


def _write_bbox_geojson() -> Path:
    s, w, n, e = STUDY_BBOX
    # Closed ring, counter-clockwise winding.
    ring = [
        [w, s],
        [e, s],
        [e, n],
        [w, n],
        [w, s],
    ]
    feature = {
        "type": "Feature",
        "properties": {
            "name": "Aonach Mór study area bbox",
            "description": (
                "Axis-aligned bounding box of the 13 iconic Chennai "
                "wards, padded by ~0.04 degrees. Used as the spatial "
                "filter for all Stage 1 Overpass queries."
            ),
            "source": "computed from ICONIC_WARDS in 01_geography.py",
        },
        "geometry": {"type": "Polygon", "coordinates": [ring]},
    }
    collection = {
        "type": "FeatureCollection",
        "features": [feature],
    }
    path = RAW_DIR / "bbox.geojson"
    path.write_text(json.dumps(collection, indent=2) + "\n", encoding="utf-8")
    return path


def _write_iconic_wards_manifest() -> Path:
    manifest = {
        "description": (
            "The 13 'iconic' Chennai wards the Aonach Mór demo focuses "
            "on, each with a representative seed point used in Stage 2 "
            "to look up the containing ward polygon. Cluster tags refer "
            "to the ClusterType concept collection in "
            "vocab/skos/ClusterType.xml."
        ),
        "wards": [
            {
                "key": w.key,
                "chennai_label": w.chennai_label,
                "cluster": w.cluster,
                "seed_lon": w.seed_lon,
                "seed_lat": w.seed_lat,
            }
            for w in ICONIC_WARDS
        ],
    }
    path = RAW_DIR / "iconic_wards.json"
    path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    return path


# --------------------------------------------------------------------------- #
# SOURCES.md                                                                  #
# --------------------------------------------------------------------------- #


def _write_sources_md(fetched: dict[str, int | None]) -> Path:
    """Write a per-layer provenance note.

    ``fetched`` maps layer key → feature count (or ``None`` if the
    layer was skipped or fetched offline).
    """
    today = _dt.date.today().isoformat()
    lines: list[str] = [
        "# Aonach Mór — Stage 1 geographic sources",
        "",
        "All files in this directory are raw Chennai geographic data "
        "fetched from OpenStreetMap via the Overpass API and written "
        "as plain GeoJSON in EPSG:4326. No renaming is applied — "
        "Stage 2 reads these files, runs the Chennai → Aonach Mór "
        "label substitution, and writes the results to "
        "`../renamed/`.",
        "",
        f"- **Fetched on**: {today}",
        f"- **Fetched by**: `example/aonach_mor/build/01_geography.py`",
        "- **Endpoint(s)**: "
        + ", ".join(f"`{url}`" for url in OVERPASS_ENDPOINTS),
        "- **Licence**: Open Data Commons Open Database License (ODbL),",
        "  © OpenStreetMap contributors",
        "",
        "## Layers",
        "",
        "| Layer | File | Feature count | Description |",
        "|-------|------|---------------|-------------|",
    ]
    for layer in LAYERS:
        count = fetched.get(layer.key)
        count_str = str(count) if count is not None else "—"
        lines.append(
            f"| `{layer.key}` | `{layer.filename}` | {count_str} | "
            f"{layer.description} |"
        )

    lines.extend(
        [
            "",
            "## Local-only artefacts",
            "",
            "| File | Description |",
            "|------|-------------|",
            "| `crs.json` | Aonach Mór local transverse-Mercator CRS "
            "parameters (proj4 + metadata). GeoJSON stays in "
            "EPSG:4326; this CRS is used by later stages for metric "
            "area and distance computations. |",
            "| `bbox.geojson` | Study-area bounding box polygon "
            "computed from `iconic_wards.json`. Used as the spatial "
            "filter for every Overpass query. |",
            "| `iconic_wards.json` | The 13 iconic Chennai wards with "
            "their cluster classification and seed points for ward "
            "polygon resolution. |",
            "",
            "## Known limitations",
            "",
            "- Multi-polygon assembly handles outer rings only; inner "
            "rings (holes) are dropped. Enclaved wards are therefore "
            "slightly over-covered in the demo.",
            "- Outer rings that Overpass returns as multiple ways are "
            "emitted as separate closed polygons rather than being "
            "stitched into a single ring. The stitched version is a "
            "Stage 2 responsibility.",
            "- Building footprints are **not** fetched at Stage 1 — "
            "they come from Overture Maps at Stage 4 "
            "(`04_buildings.py`).",
            "- Bhuvan (ISRO) and Survey of India sources listed in "
            "`ATTRIBUTION.md` are aspirational — Stage 1 uses the "
            "OSM-only path. A future iteration can add an "
            "authenticated fetch for NDSAP / NGP-2021 data without "
            "changing the on-disk format.",
            "",
        ]
    )

    path = RAW_DIR / "SOURCES.md"
    path.write_text("\n".join(lines), encoding="utf-8")
    return path


# --------------------------------------------------------------------------- #
# Per-layer fetch                                                             #
# --------------------------------------------------------------------------- #


def fetch_layer(layer: Layer, *, force: bool, dry_run: bool, offline: bool) -> int | None:
    """Fetch one layer. Returns the feature count (or ``None`` if skipped)."""
    target = layer.path()
    if target.is_file() and not force:
        if dry_run:
            _log(f"{layer.key}: already present, skipping (dry-run)")
            return None
        try:
            data = json.loads(target.read_text(encoding="utf-8"))
            count = len(data.get("features") or [])
            _log(f"{layer.key}: already present ({count} features)")
            return count
        except json.JSONDecodeError:
            _log(f"{layer.key}: existing file is not valid JSON, refetching")

    query = layer.query()
    if offline:
        _log(f"{layer.key}: offline, skipping Overpass fetch")
        return None

    try:
        response = _overpass_query(query, dry_run=dry_run)
    except OverpassError as exc:
        _log(f"{layer.key}: FAILED — {exc}")
        return None

    if response is None:  # dry-run
        return None

    geojson = _overpass_to_geojson(response)
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(json.dumps(geojson, indent=2) + "\n", encoding="utf-8")
    count = len(geojson["features"])
    _log(f"{layer.key}: wrote {count} features to {target.relative_to(REPO_ROOT)}")
    return count


# --------------------------------------------------------------------------- #
# CLI                                                                         #
# --------------------------------------------------------------------------- #


def _select_layers(only: set[str] | None) -> list[Layer]:
    if not only:
        return list(LAYERS)
    keys = {layer.key for layer in LAYERS}
    bad = only - keys
    if bad:
        raise SystemExit(
            f"Unknown layer(s): {', '.join(sorted(bad))}. "
            f"Available: {', '.join(sorted(keys))}"
        )
    return [layer for layer in LAYERS if layer.key in only]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Stage 1 — ingest Chennai geographic frame from OSM.",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="Re-fetch layers even if they are already on disk",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Show the Overpass queries without issuing them",
    )
    parser.add_argument(
        "--offline",
        action="store_true",
        help="Skip all network calls; only write local-computable artefacts",
    )
    parser.add_argument(
        "--only",
        default=None,
        help=(
            "Comma-separated layer keys to fetch. "
            f"Available: {', '.join(layer.key for layer in LAYERS)}"
        ),
    )
    args = parser.parse_args(argv)

    offline = _is_offline(args.offline)
    only = (
        {key.strip() for key in args.only.split(",") if key.strip()}
        if args.only
        else None
    )
    selected = _select_layers(only)

    RAW_DIR.mkdir(parents=True, exist_ok=True)

    # Local-computable outputs are written regardless of network state.
    crs_path = _write_crs_json()
    _log(f"crs.json: wrote {crs_path.relative_to(REPO_ROOT)}")
    bbox_path = _write_bbox_geojson()
    _log(f"bbox.geojson: wrote {bbox_path.relative_to(REPO_ROOT)}")
    wards_path = _write_iconic_wards_manifest()
    _log(f"iconic_wards.json: wrote {wards_path.relative_to(REPO_ROOT)}")

    if args.dry_run:
        _log(f"dry-run plan for {len(selected)} layer(s):")

    fetched: dict[str, int | None] = {}
    for layer in selected:
        fetched[layer.key] = fetch_layer(
            layer, force=args.force, dry_run=args.dry_run, offline=offline
        )

    # Layers that were not in the selection still get listed in
    # SOURCES.md so the provenance table is complete; we just do not
    # know their feature count.
    for layer in LAYERS:
        fetched.setdefault(layer.key, None)

    if not args.dry_run:
        sources_path = _write_sources_md(fetched)
        _log(f"SOURCES.md: wrote {sources_path.relative_to(REPO_ROOT)}")

    successes = sum(1 for v in fetched.values() if v is not None)
    _log(f"done — {successes}/{len(LAYERS)} layers have on-disk content")
    return 0


if __name__ == "__main__":
    sys.exit(main())

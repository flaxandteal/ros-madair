#!/usr/bin/env python3
"""
Stage 2 — Apply the Chennai→Aonach Mór renaming dictionary to staged geo files.

Input : geo/raw/*.geojson + geo/raw/iconic_wards.json
Dict  : renaming/chennai_to_aonach_mor.json
Output: geo/processed/*.geojson + geo/processed/iconic_wards.json
Report: geo/processed/_rename_report.json

Behaviour:
  - For every feature, walk its `properties` dict:
      * Keep only keys on the property_whitelist.
      * Rename `name` via direct lookup in wards/waterways/roads/city.
      * If not found, apply the deterministic fallback generator
        (picks an Irish adjective+noun from fallback_stems, seeded by a
        stable hash of the OSM id).
      * Features with no `name` in the source get no synthetic name.
  - Stable feature_id = first 10 chars of sha1('<osm_type>:<osm_id>').
    This lets downstream stages reference features without leaking OSM ids.
  - Coordinates are left untouched in this stage. The rebranded-CRS
    projection is applied in a later stage (per README).
  - Reports every distinct unmapped source name so the dictionary can be
    extended in future revisions if a label turns out to matter.

Usage:
    python 02_rename.py            # defaults to aonach_mor/geo/{raw,processed}
"""

from __future__ import annotations

import hashlib
import json
import re
import sys
import unicodedata
from collections import Counter
from pathlib import Path


HERE = Path(__file__).resolve().parent
ROOT = HERE.parent  # aonach_mor/
GEO_RAW = ROOT / "geo" / "raw"
GEO_OUT = ROOT / "geo" / "processed"
BUILD_LOG = ROOT / "geo" / "_build_logs"
DICT_PATH = ROOT / "renaming" / "chennai_to_aonach_mor.json"


def load_dictionary() -> dict:
    with DICT_PATH.open("r", encoding="utf-8") as f:
        return json.load(f)


def stable_hash(osm_type: str, osm_id: int | str) -> str:
    h = hashlib.sha1(f"{osm_type}:{osm_id}".encode("utf-8")).hexdigest()
    return h[:10]


def slugify(label: str) -> str:
    """Stable slug for a fictional label (used as iconic-ward key)."""
    ascii_form = unicodedata.normalize("NFKD", label).encode("ascii", "ignore").decode("ascii")
    return re.sub(r"[^a-z0-9]+", "_", ascii_form.lower()).strip("_")


def fallback_label(stable: str, stems: dict, kind: str) -> str:
    """Deterministically synthesise an Irish-flavoured label from a hash.

    `kind` picks the template: "road" → "Slí <adj> <n>",
    "water" → "Sruth <adj> <n>", else "<noun> <adj> <n>".
    """
    nouns = stems["nouns"]
    adjectives = stems["adjectives"]
    h = int(stable, 16)
    adj = adjectives[h % len(adjectives)]
    noun = nouns[(h >> 8) % len(nouns)]
    suffix = (h >> 16) % 100
    if kind == "road":
        return f"Slí {adj} {suffix:02d}"
    if kind == "water":
        return f"Sruth {adj} {suffix:02d}"
    return f"{noun} {adj} {suffix:02d}"


def classify_feature(properties: dict) -> str:
    """Return one of: 'road', 'water', 'ward', 'neighbourhood', 'city', 'other'."""
    if "highway" in properties:
        return "road"
    if "waterway" in properties:
        return "water"
    if properties.get("boundary") == "administrative":
        al = str(properties.get("admin_level", ""))
        if al == "9" or properties.get("type") == "boundary" and al in {"9", "10"}:
            return "ward"
        if al in {"4", "5", "6", "7", "8"}:
            return "city"
        return "ward"
    if properties.get("place") in {"suburb", "neighbourhood", "quarter", "city_block"}:
        return "neighbourhood"
    return "other"


def rename_name(
    source_name: str | None,
    kind: str,
    osm_type: str,
    osm_id: str | int,
    rd: dict,
    stems: dict,
    report: Counter,
) -> str | None:
    """Resolve a source `name` to its Aonach Mór label, or None if no name."""
    if not source_name:
        return None

    # Direct lookups by category first (most specific) then catch-alls.
    if kind == "water":
        mapped = rd["waterways"].get(source_name)
        if mapped:
            return mapped
    elif kind == "road":
        mapped = rd["roads"].get(source_name)
        if mapped:
            return mapped
    elif kind in {"ward", "neighbourhood", "city", "other"}:
        # Wards and city can appear anywhere; try city first, then wards.
        mapped = rd["city"].get(source_name) or rd["wards"].get(source_name)
        if mapped:
            return mapped

    # Not in any direct dictionary → generate a deterministic fallback.
    report[source_name] += 1
    stable = stable_hash(osm_type, osm_id)
    return fallback_label(stable, stems, kind if kind in {"road", "water"} else "other")


def clean_properties(src: dict, whitelist: set[str]) -> dict:
    """Keep only whitelisted keys, drop everything else (including name:*, wikidata, ref, is_in, osm_id, etc.)."""
    return {k: v for k, v in src.items() if k in whitelist}


def process_feature_collection(
    path: Path,
    rd: dict,
    whitelist: set[str],
    stems: dict,
    unmapped: Counter,
    classify_override: str | None = None,
) -> tuple[dict, dict]:
    with path.open("r", encoding="utf-8") as f:
        fc = json.load(f)

    out_features = []
    stats = Counter()
    for feat in fc.get("features", []):
        props = feat.get("properties", {}) or {}
        osm_type = props.get("osm_type") or "way"
        osm_id = props.get("osm_id") or "0"
        kind = classify_override or classify_feature(props)
        new_props = clean_properties(props, whitelist)
        source_name = props.get("name")
        new_name = rename_name(source_name, kind, osm_type, osm_id, rd, stems, unmapped)
        if new_name is not None:
            new_props["name"] = new_name
        new_props["feature_id"] = stable_hash(osm_type, osm_id)
        out_features.append(
            {
                "type": "Feature",
                "properties": new_props,
                "geometry": feat.get("geometry"),
            }
        )
        stats[kind] += 1

    out = {
        "type": "FeatureCollection",
        "metadata": {
            "source": path.name,
            "license_note": "Derived via the Aonach Mór rename pipeline; all upstream labels substituted or stripped.",
        },
        "features": out_features,
    }
    return out, dict(stats)


def process_iconic_wards(rd: dict) -> tuple[list[dict], Counter]:
    src = GEO_RAW / "iconic_wards.json"
    with src.open("r", encoding="utf-8") as f:
        d = json.load(f)
    unmapped: Counter = Counter()
    out_wards = []
    for w in d["wards"]:
        label = rd["wards"].get(w["chennai_label"])
        if not label:
            unmapped[w["chennai_label"]] += 1
            stable = stable_hash("ward", w["key"])
            label = fallback_label(stable, rd["fallback_stems"], "other")
        # Keys in the output are slugs of the fictional label, not the source key,
        # so downstream references never carry the raw source identifier.
        out_wards.append(
            {
                "key": slugify(label),
                "aonach_mor_label": label,
                "cluster": w["cluster"],
                "seed_lon": w["seed_lon"],
                "seed_lat": w["seed_lat"],
            }
        )
    return out_wards, unmapped


def main() -> int:
    rd = load_dictionary()
    whitelist = set(rd["property_whitelist"]["keys"])
    stems = rd["fallback_stems"]

    GEO_OUT.mkdir(parents=True, exist_ok=True)
    BUILD_LOG.mkdir(parents=True, exist_ok=True)

    unmapped: Counter = Counter()
    per_file_stats: dict[str, dict] = {}

    geo_files = [
        ("coastline.geojson", None),
        ("gcc_boundary.geojson", "city"),
        ("ward_boundaries.geojson", "ward"),
        ("neighbourhoods.geojson", "neighbourhood"),
        ("rivers.geojson", "water"),
        ("roads_primary.geojson", "road"),
        ("bbox.geojson", None),
    ]

    for name, override in geo_files:
        src = GEO_RAW / name
        if not src.exists():
            print(f"  skip (missing): {name}", file=sys.stderr)
            continue
        out, stats = process_feature_collection(src, rd, whitelist, stems, unmapped, override)
        dst = GEO_OUT / name
        with dst.open("w", encoding="utf-8") as f:
            json.dump(out, f, ensure_ascii=False, indent=2)
        per_file_stats[name] = {
            "features": sum(stats.values()),
            "by_kind": stats,
        }
        print(f"  wrote {dst.relative_to(ROOT)}  ({per_file_stats[name]['features']} features)")

    # iconic_wards.json is a plain JSON, handled separately.
    wards, ward_unmapped = process_iconic_wards(rd)
    (GEO_OUT / "iconic_wards.json").write_text(
        json.dumps(
            {
                "description": "Focus wards for the Aonach Mór resilience demo. Labels are fictional; seed coordinates stay in raw lon/lat for use by downstream spatial-join stages until the final CRS rebranding step.",
                "wards": wards,
            },
            ensure_ascii=False,
            indent=2,
        ),
        encoding="utf-8",
    )
    print(f"  wrote {(GEO_OUT / 'iconic_wards.json').relative_to(ROOT)}  ({len(wards)} iconic wards)")
    for k, v in ward_unmapped.items():
        unmapped[k] += v

    # Pass the crs.json through, scrubbing any textual reference to the
    # source geography out of the free-text note fields.
    crs_src = GEO_RAW / "crs.json"
    if crs_src.exists():
        crs = json.loads(crs_src.read_text(encoding="utf-8"))
        crs["wkt_note"] = (
            "Not registered with EPSG. Project-internal transverse-Mercator CRS "
            "centred on the source geography's central meridian. Downstream area / "
            "distance computations use this; stored GeoJSON remains in EPSG:4326 "
            "until the CRS-rebranding stage."
        )
        (GEO_OUT / "crs.json").write_text(
            json.dumps(crs, ensure_ascii=False, indent=2), encoding="utf-8"
        )

    report = {
        "per_file_stats": per_file_stats,
        "unmapped_source_names": {
            "count_distinct": len(unmapped),
            "count_total_occurrences": sum(unmapped.values()),
            "top_50": unmapped.most_common(50),
        },
    }
    (BUILD_LOG / "rename_report.json").write_text(
        json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8"
    )
    print(
        f"\nreport: {len(unmapped)} distinct source labels handled via fallback, "
        f"{sum(unmapped.values())} total occurrences."
    )
    print(f"  see {(BUILD_LOG / 'rename_report.json').relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

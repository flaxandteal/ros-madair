#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 Flax & Teal Limited
"""Stage 0 (step 20) — SKOS concept collections for Aonach Mór.

Generates the 17 controlled vocabularies listed in plan Section 6.5 as
Arches-compatible SKOS XML files under ``vocab/skos/``. These are the
concept collections the resource models bind onto (HazardType,
AdminDivision, ClusterType, HeritageStatus, ServiceRole, …).

Format follows the existing project convention established by
``vocab/skos/riskine_enums.xml``:

  - URIs under ``https://rosmadair.example.org/aonach_mor/vocab/``
  - ``skos:ConceptScheme`` per collection
  - ``skos:Concept`` per entry, linked with ``skos:inScheme``
  - ``skos:prefLabel @xml:lang`` (English primary; Irish ``ga`` + Tamil
    ``ta`` as secondary labels only on heritage-relevant collections, per
    plan Section 11)
  - ``skos:notation`` ordinal for stable column order

Run from the repository root:

    python example/aonach_mor/build/20_skos_collections.py
    python example/aonach_mor/build/20_skos_collections.py --force
    python example/aonach_mor/build/20_skos_collections.py --only HazardType,SensorType

The script is idempotent; existing files are overwritten only when
``--force`` is passed or the on-disk content differs from what the script
would write.

GEM Building Taxonomy is produced from a hand-picked subset embedded in
this file; a future iteration can read a vendored CSV from
``vocab/gem_taxonomy/source/`` if a fuller taxonomy is required.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import os
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterable
from xml.sax.saxutils import escape as _xml_escape

# --------------------------------------------------------------------------- #
# Configuration                                                               #
# --------------------------------------------------------------------------- #

REPO_ROOT = Path(__file__).resolve().parents[3]
AONACH_DIR = REPO_ROOT / "example" / "aonach_mor"
VOCAB_DIR = Path(
    os.environ.get("AONACH_VOCAB_DIR", str(AONACH_DIR / "vocab"))
).resolve()
SKOS_DIR = VOCAB_DIR / "skos"

BASE_URI = "https://rosmadair.example.org/aonach_mor/vocab"


def _log(msg: str) -> None:
    print(f"[20_skos] {msg}", file=sys.stderr)


# --------------------------------------------------------------------------- #
# Label helpers                                                               #
# --------------------------------------------------------------------------- #

# A concept label is either a plain English string or a dict mapping
# language tags to strings. Heritage-relevant collections use the dict
# form to exercise the multilingual column syntax; everything else stays
# English-only.
Label = str | dict[str, str]


@dataclass
class Concept:
    slug: str
    label: Label
    notation: str | int | None = None


@dataclass
class Collection:
    name: str
    english_label: str
    description: str
    concepts: list[Concept] = field(default_factory=list)
    # Optional extra languages on the ConceptScheme title (used for
    # HeritageStatus so the scheme itself carries multilingual metadata).
    scheme_labels: dict[str, str] = field(default_factory=dict)


def _labels(label: Label) -> list[tuple[str, str]]:
    if isinstance(label, str):
        return [("en", label)]
    return list(label.items())


# --------------------------------------------------------------------------- #
# Data — the 17 collections                                                   #
# --------------------------------------------------------------------------- #

# Heritage-relevant collection — full trilingual (English + Irish + Tamil).
# Irish anchors the Aonach Mór fictional framing; Tamil lands with Chennai
# reviewers.
HERITAGE_STATUS = Collection(
    name="HeritageStatus",
    english_label="Heritage status",
    description=(
        "Listing / protection status of a heritage asset. Trilingual "
        "labels exercise the multilingual column syntax on heritage "
        "resources (plan Section 11)."
    ),
    scheme_labels={
        "en": "Heritage status",
        "ga": "Stádas oidhreachta",
        "ta": "மரபு நிலை",
    },
    concepts=[
        Concept(
            "listed",
            {"en": "Listed", "ga": "Liostaithe", "ta": "பட்டியலிடப்பட்டது"},
            notation=1,
        ),
        Concept(
            "candidate",
            {"en": "Candidate", "ga": "Iarrthóir", "ta": "வேட்பாளர்"},
            notation=2,
        ),
        Concept(
            "non_listed",
            {
                "en": "Non-listed",
                "ga": "Neamhliostaithe",
                "ta": "பட்டியலிடப்படவில்லை",
            },
            notation=3,
        ),
    ],
)

HAZARD_TYPE = Collection(
    name="HazardType",
    english_label="Hazard type",
    description=(
        "Kinds of hazard the demo carries hazard footprints for. "
        "Matches plan Section 6.5."
    ),
    concepts=[
        Concept("flood", "Flood", 1),
        Concept("cyclone", "Cyclone", 2),
        Concept("earthquake", "Earthquake", 3),
        Concept("wind", "Wind", 4),
        Concept("heat", "Heat", 5),
        Concept("crowding", "Crowding", 6),
        Concept("surge", "Storm surge", 7),
        Concept("landslide", "Landslide", 8),
    ],
)

ADMIN_DIVISION = Collection(
    name="AdminDivision",
    english_label="Administrative division",
    description=(
        "Tiers of administrative geography used by Site resources. "
        "Concept labels are generic; individual Site instances carry "
        "the renamed Aonach Mór labels as free-text appellations."
    ),
    concepts=[
        Concept("ward", "Ward", 1),
        Concept("zone", "Zone", 2),
        Concept("corporation", "Corporation / municipality", 3),
        Concept("state", "State", 4),
    ],
)

CLUSTER_TYPE = Collection(
    name="ClusterType",
    english_label="Cluster type",
    description="Urban cluster categories used to tag buildings and sites.",
    concepts=[
        Concept("cbd", "Central business district", 1),
        Concept("residential", "Residential", 2),
        Concept("industrial", "Industrial", 3),
        Concept("heritage", "Heritage", 4),
        Concept("coastal", "Coastal", 5),
        Concept("port", "Port", 6),
        Concept("peri_urban", "Peri-urban", 7),
    ],
)

SERVICE_ROLE = Collection(
    name="ServiceRole",
    english_label="Service role",
    description=(
        "Critical-service role a building fills during a rehearsal — "
        "emergency-coordinator view lights these up by type."
    ),
    concepts=[
        Concept("shelter", "Shelter", 1),
        Concept("hospital", "Hospital", 2),
        Concept("school", "School", 3),
        Concept("fire", "Fire station", 4),
        Concept("police", "Police station", 5),
        Concept("eoc", "Emergency operations centre", 6),
        Concept("muster", "Muster point", 7),
        Concept("substation", "Electrical substation", 8),
        Concept("water", "Water / pumping station", 9),
    ],
)

SECTOR = Collection(
    name="Sector",
    english_label="Economic sector",
    description=(
        "Economic sector — ISIC rev 4 section/division subset relevant "
        "to urban coverage. Not a full ISIC import; the demo ships only "
        "enough entries to give Organisation resources a plausible mix."
    ),
    concepts=[
        Concept("agri", "A — Agriculture, forestry and fishing", 1),
        Concept("mining", "B — Mining and quarrying", 2),
        Concept("manufacturing", "C — Manufacturing", 3),
        Concept("utilities", "D — Electricity, gas, steam, air-conditioning", 4),
        Concept("water", "E — Water supply, sewerage, waste", 5),
        Concept("construction", "F — Construction", 6),
        Concept("wholesale_retail", "G — Wholesale and retail trade", 7),
        Concept("transport", "H — Transportation and storage", 8),
        Concept("hospitality", "I — Accommodation and food service", 9),
        Concept("ict", "J — Information and communication", 10),
        Concept("finance", "K — Financial and insurance activities", 11),
        Concept("real_estate", "L — Real estate activities", 12),
        Concept("professional", "M — Professional, scientific, technical", 13),
        Concept("admin", "N — Administrative and support service", 14),
        Concept("public_admin", "O — Public administration and defence", 15),
        Concept("education", "P — Education", 16),
        Concept("health", "Q — Human health and social work", 17),
        Concept("arts", "R — Arts, entertainment and recreation", 18),
        Concept("other_services", "S — Other service activities", 19),
    ],
)

SIZE_BAND = Collection(
    name="SizeBand",
    english_label="Organisation size band",
    description="Headcount-based size bands for Organisation resources.",
    concepts=[
        Concept("micro", "Micro (1–9)", 1),
        Concept("small", "Small (10–49)", 2),
        Concept("medium", "Medium (50–249)", 3),
        Concept("large", "Large (250–999)", 4),
        Concept("very_large", "Very large (1000+)", 5),
    ],
)

SEVERITY_BAND = Collection(
    name="SeverityBand",
    english_label="Severity band",
    description="Generic severity/impact banding shared across hazard types.",
    concepts=[
        Concept("low", "Low", 1),
        Concept("medium", "Medium", 2),
        Concept("high", "High", 3),
        Concept("very_high", "Very high", 4),
    ],
)

INTENSITY_BAND = Collection(
    name="IntensityBand",
    english_label="Intensity band",
    description=(
        "Hazard-type-specific intensity bandings. Concepts encode both "
        "the hazard family and the band so a single column on "
        "HazardFootprint can resolve either cyclone wind-speeds or "
        "flood water depths."
    ),
    concepts=[
        # Flood — water depth bands
        Concept("flood_nuisance", "Flood — nuisance (<0.3 m)", 1),
        Concept("flood_shallow", "Flood — shallow (0.3–1.0 m)", 2),
        Concept("flood_deep", "Flood — deep (1.0–3.0 m)", 3),
        Concept("flood_extreme", "Flood — extreme (>3.0 m)", 4),
        # Wind / cyclone — Saffir-Simpson-ish
        Concept("wind_tropical_storm", "Wind — tropical storm", 10),
        Concept("wind_cat1", "Wind — Cat 1 cyclone", 11),
        Concept("wind_cat2", "Wind — Cat 2 cyclone", 12),
        Concept("wind_cat3", "Wind — Cat 3 cyclone", 13),
        Concept("wind_cat4", "Wind — Cat 4 cyclone", 14),
        Concept("wind_cat5", "Wind — Cat 5 cyclone", 15),
        # Earthquake — PGA bands (g)
        Concept("eq_weak", "Earthquake — weak (PGA <0.05 g)", 20),
        Concept("eq_moderate", "Earthquake — moderate (0.05–0.15 g)", 21),
        Concept("eq_strong", "Earthquake — strong (0.15–0.3 g)", 22),
        Concept("eq_severe", "Earthquake — severe (>0.3 g)", 23),
        # Heat — daily max temp bands
        Concept("heat_warm", "Heat — warm (<35 °C)", 30),
        Concept("heat_hot", "Heat — hot (35–40 °C)", 31),
        Concept("heat_very_hot", "Heat — very hot (40–45 °C)", 32),
        Concept("heat_extreme", "Heat — extreme (>45 °C)", 33),
        # Crowding — persons/m² bands
        Concept("crowd_sparse", "Crowd — sparse (<2 p/m²)", 40),
        Concept("crowd_moderate", "Crowd — moderate (2–4 p/m²)", 41),
        Concept("crowd_dense", "Crowd — dense (4–6 p/m²)", 42),
        Concept("crowd_critical", "Crowd — critical (>6 p/m²)", 43),
    ],
)

CONFIDENCE_BAND = Collection(
    name="ConfidenceBand",
    english_label="Confidence band",
    description=(
        "Qualitative confidence rating attached to Observations and "
        "HazardFootprints by the HazardModel that produced them."
    ),
    concepts=[
        Concept("low", "Low", 1),
        Concept("medium", "Medium", 2),
        Concept("high", "High", 3),
    ],
)

COVERAGE_TYPE = Collection(
    name="CoverageType",
    english_label="Insurance coverage type",
    description=(
        "Line-of-business breakdown for InsuranceCoverage tiles. "
        "Bound alongside `riskine:Coverage`, which carries much finer "
        "granularity in the upstream Riskine enums."
    ),
    concepts=[
        Concept("property", "Property / buildings", 1),
        Concept("business_interruption", "Business interruption", 2),
        Concept("contents", "Contents / inventory", 3),
        Concept("liability", "Public / third-party liability", 4),
    ],
)

LIMIT_BAND = Collection(
    name="LimitBand",
    english_label="Policy limit band",
    description=(
        "Log-banded policy / coverage limit ranges, stored as concepts "
        "so the insurer view can aggregate without de-anonymising "
        "individual policies."
    ),
    concepts=[
        Concept("band_under_10k", "Under 10 k", 1),
        Concept("band_10k_100k", "10 k – 100 k", 2),
        Concept("band_100k_1m", "100 k – 1 m", 3),
        Concept("band_1m_10m", "1 m – 10 m", 4),
        Concept("band_10m_100m", "10 m – 100 m", 5),
        Concept("band_over_100m", "Over 100 m", 6),
    ],
)

SENSOR_TYPE = Collection(
    name="SensorType",
    english_label="Sensor type",
    description=(
        "SOSA sensor-kind classifier. Bound to the Sensor resource "
        "model's `sosa:Sensor` root alongside a free-text model number."
    ),
    concepts=[
        Concept("gauge", "Water-level gauge", 1),
        Concept("anemometer", "Anemometer", 2),
        Concept("seismometer", "Seismometer", 3),
        Concept("thermometer", "Thermometer", 4),
        Concept("hygrometer", "Hygrometer", 5),
        Concept("air_quality", "Air-quality monitor", 6),
        Concept("people_counter", "People counter", 7),
    ],
)

OBSERVABLE_PROPERTY = Collection(
    name="ObservableProperty",
    english_label="Observable property",
    description=(
        "SOSA observable-property classifier — what a given observation "
        "actually measures. Paired one-to-many with SensorType."
    ),
    concepts=[
        Concept("water_level", "Water level (m)", 1),
        Concept("wind_speed", "Wind speed (m/s)", 2),
        Concept("peak_ground_acceleration", "Peak ground acceleration (g)", 3),
        Concept("temperature", "Temperature (°C)", 4),
        Concept("pm2_5", "PM2.5 concentration (µg/m³)", 5),
        Concept("head_count", "Head count (persons)", 6),
    ],
)

EVENT_TYPE = Collection(
    name="EventType",
    english_label="Scenario event type",
    description=(
        "Scenario-rehearsal event category. Ties a named rehearsal to "
        "its hazard family for cross-scenario aggregation."
    ),
    concepts=[
        Concept("cyclone_landfall", "Cyclone landfall", 1),
        Concept("pluvial_flood", "Pluvial flood", 2),
        Concept("coastal_flood", "Coastal flood", 3),
        Concept("earthquake_event", "Earthquake", 4),
        Concept("heatwave", "Heatwave", 5),
        Concept("mass_gathering", "Mass-gathering incident", 6),
    ],
)

METHOD = Collection(
    name="Method",
    english_label="Hazard-model method",
    description=(
        "Modelling technique used by a HazardModel activity. Bound to "
        "the HazardModel resource via `crm:P2_has_type`."
    ),
    concepts=[
        Concept("hydraulic", "Hydraulic / hydrodynamic", 1),
        Concept("wind_field", "Wind field", 2),
        Concept("seismic", "Seismic ground-motion", 3),
        Concept("thermal", "Thermal / heat-exposure", 4),
        Concept("crowd", "Crowd dynamics", 5),
    ],
)

# GEM Building Taxonomy v3 — hand-picked subset. The full taxonomy is a
# multi-axis faceted code system; the demo only needs a dozen or so of
# the primary material+system codes to classify buildings onto clusters.
GEM_TAXONOMY = Collection(
    name="GEMTaxonomy",
    english_label="GEM Building Taxonomy v3 subset",
    description=(
        "Hand-picked subset of GEM Building Taxonomy v3 primary "
        "material+lateral-load-resisting-system codes. Not a full "
        "taxonomy import; reviewers who need the canonical definitions "
        "should consult the GEM specification document."
    ),
    concepts=[
        Concept("cr_lfinf", "CR/LFINF — Reinforced concrete, infilled frame", 1),
        Concept("cr_lfm", "CR/LFM — Reinforced concrete, moment frame", 2),
        Concept("cr_lwal", "CR/LWAL — Reinforced concrete, shear wall", 3),
        Concept("cr_lpb", "CR/LPB — Reinforced concrete, precast", 4),
        Concept("mur_adb", "MUR+ADO — Unreinforced masonry, adobe block", 5),
        Concept("mur_clbrs", "MUR+CLBRS — Unreinforced masonry, clay brick", 6),
        Concept("mur_stdre", "MUR+STDRE — Unreinforced masonry, stone dressed", 7),
        Concept("mcf_clbrs", "MCF+CLBRS — Confined masonry, clay brick", 8),
        Concept("mr_clbrs", "MR+CLBRS — Reinforced masonry, clay brick", 9),
        Concept("s_lfm", "S/LFM — Steel, moment frame", 10),
        Concept("s_lfbr", "S/LFBR — Steel, braced frame", 11),
        Concept("w_lwal", "W/LWAL — Wood, shear wall", 12),
        Concept("w_lpb", "W/LPB — Wood, post and beam", 13),
        Concept("e_lwal", "E/LWAL — Earthen, load-bearing wall", 14),
        Concept("mix", "MIX — Mixed / hybrid structural system", 15),
        Concept("unk", "UNK — Unknown / not assessed", 99),
    ],
)

COLLECTIONS: list[Collection] = [
    HAZARD_TYPE,
    ADMIN_DIVISION,
    CLUSTER_TYPE,
    GEM_TAXONOMY,
    HERITAGE_STATUS,
    SERVICE_ROLE,
    SECTOR,
    SIZE_BAND,
    SEVERITY_BAND,
    INTENSITY_BAND,
    CONFIDENCE_BAND,
    COVERAGE_TYPE,
    LIMIT_BAND,
    SENSOR_TYPE,
    OBSERVABLE_PROPERTY,
    EVENT_TYPE,
    METHOD,
]


# --------------------------------------------------------------------------- #
# XML rendering                                                               #
# --------------------------------------------------------------------------- #

XML_HEADER = """<?xml version="1.0" encoding="UTF-8"?>
<rdf:RDF
  xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
  xmlns:rdfs="http://www.w3.org/2000/01/rdf-schema#"
  xmlns:skos="http://www.w3.org/2004/02/skos/core#"
  xmlns:dcterms="http://purl.org/dc/terms/">
"""

XML_FOOTER = "</rdf:RDF>\n"


def _scheme_uri(collection: Collection) -> str:
    return f"{BASE_URI}/{collection.name}"


def _concept_uri(collection: Collection, concept: Concept) -> str:
    return f"{BASE_URI}/{collection.name}/{concept.slug}"


def render_collection(collection: Collection, *, generated_on: str) -> str:
    scheme_uri = _scheme_uri(collection)
    scheme_labels = collection.scheme_labels or {"en": collection.english_label}

    lines: list[str] = [XML_HEADER.rstrip("\n")]
    lines.append(
        f"  <!-- Aonach Mór controlled vocabulary: {collection.name}. "
        f"Generated on {generated_on} by "
        f"example/aonach_mor/build/20_skos_collections.py. "
        "Do not hand-edit. -->"
    )
    lines.append(f"  <skos:ConceptScheme rdf:about=\"{_xml_escape(scheme_uri)}\">")
    for lang, label in scheme_labels.items():
        lines.append(
            f"    <skos:prefLabel xml:lang=\"{lang}\">"
            f"{_xml_escape(label)}"
            "</skos:prefLabel>"
        )
    lines.append(
        f"    <dcterms:description xml:lang=\"en\">"
        f"{_xml_escape(collection.description)}"
        "</dcterms:description>"
    )
    lines.append("  </skos:ConceptScheme>")

    for idx, concept in enumerate(collection.concepts, start=1):
        concept_uri = _concept_uri(collection, concept)
        lines.append(
            f"  <skos:Concept rdf:about=\"{_xml_escape(concept_uri)}\">"
        )
        lines.append(
            f"    <skos:inScheme rdf:resource=\"{_xml_escape(scheme_uri)}\"/>"
        )
        for lang, label in _labels(concept.label):
            lines.append(
                f"    <skos:prefLabel xml:lang=\"{lang}\">"
                f"{_xml_escape(label)}"
                "</skos:prefLabel>"
            )
        notation = concept.notation if concept.notation is not None else idx
        lines.append(
            f"    <skos:notation>{_xml_escape(str(notation))}</skos:notation>"
        )
        lines.append("  </skos:Concept>")

    lines.append(XML_FOOTER.rstrip("\n"))
    return "\n".join(lines) + "\n"


# --------------------------------------------------------------------------- #
# CLI                                                                         #
# --------------------------------------------------------------------------- #


def _write_if_changed(path: Path, content: str, *, force: bool) -> bool:
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.is_file() and not force:
        existing = path.read_text(encoding="utf-8")
        if existing == content:
            _log(f"{path.name}: unchanged")
            return False
    path.write_text(content, encoding="utf-8")
    _log(f"{path.name}: wrote {len(content):,} bytes")
    return True


def _select(collections: Iterable[Collection], only: set[str] | None) -> list[Collection]:
    if not only:
        return list(collections)
    names = {c.name.lower() for c in collections}
    bad = only - names
    if bad:
        raise SystemExit(
            f"Unknown collection(s): {', '.join(sorted(bad))}. "
            f"Available: {', '.join(sorted(names))}"
        )
    return [c for c in collections if c.name.lower() in only]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Stage 0 step 20 — generate SKOS concept collections."
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="Rewrite output files even if unchanged",
    )
    parser.add_argument(
        "--only",
        default=None,
        help=(
            "Comma-separated collection names to generate "
            "(case-insensitive). Default: all."
        ),
    )
    parser.add_argument(
        "--out",
        default=None,
        help=f"Override output directory (default: {SKOS_DIR})",
    )
    args = parser.parse_args(argv)

    out_dir = Path(args.out).resolve() if args.out else SKOS_DIR
    only = (
        {name.strip().lower() for name in args.only.split(",") if name.strip()}
        if args.only
        else None
    )

    selected = _select(COLLECTIONS, only)
    generated_on = _dt.date.today().isoformat()

    for collection in selected:
        xml = render_collection(collection, generated_on=generated_on)
        path = out_dir / f"{collection.name}.xml"
        _write_if_changed(path, xml, force=args.force)

    _log(f"Generated {len(selected)} of {len(COLLECTIONS)} collections.")
    return 0


if __name__ == "__main__":
    sys.exit(main())

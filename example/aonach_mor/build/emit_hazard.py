#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# © Flax & Teal Limited. Aonach Mór — hazard synthesis for the REAL ros-madair-emit
# path (companion to emit_buildings.py).
#
# Emits THREE related resource models in one deterministic pass:
#   HazardModel    (sosa:Procedure)  — a reusable hazard computation procedure
#   ScenarioEvent  (CRM E5_Event)    — a named scenario, time bracket, models used
#   HazardFootprint(prov:Entity)     — a spatialised hazard output: REAL polygon
#                                      geometry placed off the actual geography
#                                      (storm-surge on the coastline, flood along
#                                      rivers, the rest over ward polygons), linked
#                                      produced_by_model → HazardModel and
#                                      scenario → ScenarioEvent.
#
# Scale is FIXED and decoupled from the ×N building replicas: ~15 models, ~15
# scenarios, ~94 footprints (a handful of real hazards however big the stock).
#
# The believability mechanism is geographic: footprint geometry is built from the
# real ward_boundaries / coastline / rivers GeoJSON, and every footprint is TAGGED
# with the set of wards (and zones) its polygon covers — the join key a downstream
# exposure generator reads to attach buildings to footprints by WARD/ZONE
# (intersection is ward/zone-based, NOT per-building point-in-polygon). The ward
# names are reported STRIPPED ("Cill Úr 51" → "Cill Úr"), matching the key
# emit_buildings writes into each building's `site`.
#
#   python example/aonach_mor/build/emit_hazard.py --out <data_dir>
# writes <data_dir>/graphs/<each graphid>.json and
#        <data_dir>/resources/{hazard_model,scenario_event,hazard_footprint}/*.json

import argparse
import csv
import json
import math
import random
import uuid
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent  # example/aonach_mor
GEO = ROOT / "geo/processed"
MODELS = ROOT / "graphs/resource_models"
DOCS = ROOT.parent.parent.parent / "ros-madair-docs"  # footprints.csv (ward → zone)

NS = uuid.NAMESPACE_URL
HM_GRAPHID = str(uuid.uuid5(NS, "aonach-mor/resource_models/hazard_model"))
SE_GRAPHID = str(uuid.uuid5(NS, "aonach-mor/resource_models/scenario_event"))
HF_GRAPHID = str(uuid.uuid5(NS, "aonach-mor/resource_models/hazard_footprint"))

CRM = "http://www.cidoc-crm.org/cidoc-crm/"


# ── graph JSON (same minimal shape + id scheme as emit_buildings.build_graph) ──
def _nid(graphid, alias):
    return str(uuid.uuid5(uuid.UUID(graphid), alias))


def build_graph(graphid, model_key, root_name, root_oc):
    """Read <model>/nodes.csv and emit the minimal graph JSON emit consumes.
    Node/edge/nodegroup ids are computed EXACTLY as emit_buildings.build_graph:
    nid = uuid5(graphid, alias); edge = uuid5(graphid, 'edge:'+alias);
    nodegroup_id = nid(top_ancestor(alias))."""
    rows = list(csv.DictReader((MODELS / model_key / "nodes.csv").open()))
    by_alias = {r["alias"]: r for r in rows}

    def top_ancestor(alias):
        r = by_alias[alias]
        while r["parent_alias"]:
            r = by_alias[r["parent_alias"]]
        return r["alias"]

    def nid(a):
        return _nid(graphid, a)

    root_alias = model_key
    root = {"nodeid": nid(root_alias), "name": root_name, "alias": root_alias,
            "datatype": "semantic", "graph_id": graphid, "istopnode": True,
            "is_collector": False, "isrequired": False, "issearchable": True,
            "exportable": False, "sortorder": 0, "ontologyclass": root_oc}
    nodes = [root]
    edges = []
    nodegroups = {}
    for r in rows:
        alias = r["alias"]
        ng_alias = top_ancestor(alias)
        is_top = not r["parent_alias"]
        nodes.append({
            "nodeid": nid(alias), "name": r["name"], "alias": alias,
            "datatype": r["datatype"], "graph_id": graphid, "istopnode": False,
            "is_collector": is_top, "isrequired": r["required"] == "true",
            "issearchable": r["searchable"] == "true", "exportable": r["exportable"] == "true",
            "sortorder": int(r["sortorder"]), "ontologyclass": r["ontology_class"],
            "nodegroup_id": nid(ng_alias),
        })
        parent = r["parent_alias"] or root_alias
        edges.append({"edgeid": str(uuid.uuid5(uuid.UUID(graphid), f"edge:{alias}")),
                      "domainnode_id": nid(parent), "rangenode_id": nid(alias),
                      "graph_id": graphid, "ontologyproperty": r["parent_property"]})
        if is_top:
            card = "n" if r["datatype"].endswith("-list") else "1"
            nodegroups[ng_alias] = {"nodegroupid": nid(ng_alias), "cardinality": card,
                                    "parentnodegroup_id": None}
    return {"graph": [{"graphid": graphid, "name": root_name, "root": root,
                       "nodes": nodes, "nodegroups": list(nodegroups.values()), "edges": edges}]}


def _tile(graphid, rid, ng_alias, data, tag, gid):
    return {"tileid": str(uuid.uuid5(uuid.UUID(graphid), f"tile:{gid}:{tag}")),
            "nodegroup_id": _nid(graphid, ng_alias), "parenttile_id": None,
            "resourceinstance_id": rid, "sortorder": 0, "provisionaledits": None, "data": data}


def _resource(graphid, rid, name, description, tiles):
    return {
        "resourceinstance": {"resourceinstanceid": rid, "graph_id": graphid, "name": name,
            "legacyid": None,
            "descriptors": {"en": {"name": name, "description": description, "map_popup": ""}}},
        "tiles": tiles,
    }


# ── geometry helpers (no shapely): point-in-ring + polyline buffer ───────────
def _point_in_ring(pt, ring):
    x, y = pt
    inside = False
    n = len(ring)
    j = n - 1
    for i in range(n):
        xi, yi = ring[i]
        xj, yj = ring[j]
        if ((yi > y) != (yj > y)) and (x < (xj - xi) * (y - yi) / (yj - yi + 1e-18) + xi):
            inside = not inside
        j = i
    return inside


def _fc_area_sqkm(fc):
    """Planar area of a GeoJSON FeatureCollection's Polygon rings, in km² — the shoelace
    formula on lng/lat with a latitude correction (standing in for GeoSPARQL geof:area).
    Sums exterior rings across features."""
    import math
    total = 0.0
    for f in fc.get("features", []):
        g = f.get("geometry", {})
        polys = g["coordinates"] if g.get("type") == "MultiPolygon" else [g.get("coordinates", [])]
        for poly in polys:
            if not poly:
                continue
            ring = poly[0]
            lat0 = sum(p[1] for p in ring) / len(ring)
            kx = 111.320 * math.cos(math.radians(lat0)); ky = 110.574  # km per degree
            s = 0.0
            for i in range(len(ring) - 1):
                x1, y1 = ring[i]; x2, y2 = ring[i + 1]
                s += (x1 * kx) * (y2 * ky) - (x2 * kx) * (y1 * ky)
            total += abs(s) / 2.0
    return round(total, 4)


def _centroid(coords):
    xs = [c[0] for c in coords]
    ys = [c[1] for c in coords]
    return (sum(xs) / len(xs), sum(ys) / len(ys))


def _clean_ring(ring):
    """Normalise a source exterior ring to valid GeoJSON: drop consecutive dupes,
    close it (first == last), and reject degenerates (<4 positions / <3 distinct
    vertices) — the real ward_boundaries data carries a few. Returns None if degen."""
    pts = [tuple(p) for p in ring]
    out = [pts[0]]
    for p in pts[1:]:
        if p != out[-1]:
            out.append(p)
    if out and out[0] != out[-1]:
        out.append(out[0])
    if len(out) < 4 or len({p for p in out}) < 3:
        return None
    return out


def _buffer_line(line, half_width):
    """Build a closed ribbon polygon ring around an ordered LineString by offsetting
    each vertex along the averaged perpendicular of its adjacent segments. Returns a
    single exterior ring (lng/lat). Latitude-corrected so the ribbon width is even."""
    coslat = math.cos(math.radians(line[len(line) // 2][1]))
    coslat = coslat if abs(coslat) > 1e-6 else 1e-6

    def seg_normal(a, b):
        dx = (b[0] - a[0]) * coslat
        dy = b[1] - a[1]
        L = math.hypot(dx, dy) or 1e-12
        # left normal of the direction vector
        return (-dy / L, dx / L)

    normals = []
    for i in range(len(line)):
        a = line[max(i - 1, 0)]
        b = line[min(i + 1, len(line) - 1)]
        normals.append(seg_normal(a, b))
    left, right = [], []
    for (x, y), (nx, ny) in zip(line, normals):
        ox = nx * half_width / coslat
        oy = ny * half_width
        left.append((x + ox, y + oy))
        right.append((x - ox, y - oy))
    return left + right[::-1] + [left[0]]


# ── ward universe: real polygons + building-ward flag + dominant zone ─────────
def _strip_ward(w):
    p = w.rsplit(" ", 1)
    return p[0] if len(p) == 2 and p[1].isdigit() else w


def load_wards():
    """Return list of ward dicts from the REAL ward_boundaries.geojson:
       {full, strip, rings:[exterior ring,...], centroid, is_bld, zone}.
    `is_bld`/`zone` come from footprints.csv (the 43 wards buildings actually
    occupy and their dominant zone) so footprint coverage lands on buildings."""
    # building wards + dominant zone from the shared footprint seed
    ward_zone = {}
    zcount = {}
    try:
        for r in csv.DictReader((DOCS / "scripts/seed/footprints.csv").open()):
            zcount.setdefault(r["ward"], {}).setdefault(r["zone"], 0)
            zcount[r["ward"]][r["zone"]] += 1
    except FileNotFoundError:
        pass
    bld_full = set(zcount)
    for full, zs in zcount.items():
        ward_zone[full] = max(zs, key=zs.get)

    fc = json.loads((GEO / "ward_boundaries.geojson").read_text())
    wards = []
    for f in fc["features"]:
        full = f["properties"]["name"]
        g = f["geometry"]
        polys = g["coordinates"] if g["type"] == "MultiPolygon" else [g["coordinates"]]
        rings = [r for r in (_clean_ring(p[0]) for p in polys) if r]  # valid exterior rings
        if not rings:
            continue
        allpts = [pt for r in rings for pt in r]
        wards.append({
            "full": full, "strip": _strip_ward(full), "rings": rings,
            "centroid": _centroid(allpts), "is_bld": full in bld_full,
            "zone": ward_zone.get(full),
        })
    return wards


def _ward_contains(ward, pt):
    return any(_point_in_ring(pt, r) for r in ward["rings"])


def _nearest_ward(wards, pt, bld_only=True):
    pool = [w for w in wards if w["is_bld"]] if bld_only else wards
    pool = pool or wards
    return min(pool, key=lambda w: (w["centroid"][0] - pt[0]) ** 2 + (w["centroid"][1] - pt[1]) ** 2)


def _coverage(wards, polygon_rings, anchor_pt):
    """Wards whose representative point falls inside the footprint polygon(s);
    guaranteed non-empty via the nearest building-ward to the anchor."""
    hit = [w for w in wards if any(_point_in_ring(w["centroid"], r) for r in polygon_rings)]
    if not hit:
        hit = [_nearest_ward(wards, anchor_pt)]
    return hit


def _line_features(path):
    fc = json.loads((GEO / path).read_text())
    out = []
    for f in fc["features"]:
        g = f["geometry"]
        if g["type"] == "LineString" and len(g["coordinates"]) >= 4:
            out.append((f["properties"].get("name"), [tuple(c) for c in g["coordinates"]]))
    return out


# ── hazard domain vocab (labels straight from the SKOS collections) ───────────
HAZARD_TYPES = ["Flood", "Storm surge", "Cyclone", "Wind", "Earthquake", "Heat", "Crowding"]
METHOD_FOR = {
    "Flood": "Hydraulic / hydrodynamic", "Storm surge": "Hydraulic / hydrodynamic",
    "Cyclone": "Wind field", "Wind": "Wind field", "Earthquake": "Seismic ground-motion",
    "Heat": "Thermal / heat-exposure", "Crowding": "Crowd dynamics",
}
# IntensityBand labels per hazard family (storm surge is water depth → Flood bands).
INTENSITY = {
    "Flood": ["Flood — nuisance (<0.3 m)", "Flood — shallow (0.3–1.0 m)",
              "Flood — deep (1.0–3.0 m)", "Flood — extreme (>3.0 m)"],
    "Storm surge": ["Flood — shallow (0.3–1.0 m)", "Flood — deep (1.0–3.0 m)",
                    "Flood — extreme (>3.0 m)"],
    "Cyclone": ["Wind — Cat 1 cyclone", "Wind — Cat 2 cyclone", "Wind — Cat 3 cyclone",
                "Wind — Cat 4 cyclone", "Wind — Cat 5 cyclone"],
    "Wind": ["Wind — tropical storm", "Wind — Cat 1 cyclone", "Wind — Cat 2 cyclone"],
    "Earthquake": ["Earthquake — weak (PGA <0.05 g)", "Earthquake — moderate (0.05–0.15 g)",
                   "Earthquake — strong (0.15–0.3 g)", "Earthquake — severe (>0.3 g)"],
    "Heat": ["Heat — warm (<35 °C)", "Heat — hot (35–40 °C)", "Heat — very hot (40–45 °C)",
             "Heat — extreme (>45 °C)"],
    "Crowding": ["Crowd — sparse (<2 p/m²)", "Crowd — moderate (2–4 p/m²)",
                 "Crowd — dense (4–6 p/m²)", "Crowd — critical (>6 p/m²)"],
}
SEVERITY = ["Low", "Medium", "High", "Very high"]
CONFIDENCE = ["Low", "Medium", "High"]
EVENT_TYPE_HAZARDS = {  # ScenarioEvent event_type → the hazard families it generates
    "Cyclone landfall": ["Cyclone", "Wind", "Storm surge"],
    "Pluvial flood": ["Flood"],
    "Coastal flood": ["Storm surge", "Flood"],
    "Earthquake": ["Earthquake"],
    "Heatwave": ["Heat"],
    "Mass-gathering incident": ["Crowding"],
}
WARD_HAZARDS = {"Cyclone", "Wind", "Earthquake", "Heat", "Crowding"}  # over ward polygons
# how many wards a ward-anchored footprint spans, by hazard family
WARD_SPAN = {"Cyclone": (6, 12), "Wind": (4, 9), "Earthquake": (3, 7),
             "Heat": (3, 6), "Crowding": (1, 2)}

FACILITIES = ["Oifig na Síne", "Lárionad Pleanála", "Foireann Teicniúil", "Aonad Tubaiste"]


def _date(rng, y0, y1):
    import datetime
    start = datetime.date(rng.randint(y0, y1), rng.randint(1, 12), rng.randint(1, 28))
    end = start + datetime.timedelta(days=rng.randint(0, 6))
    return start.isoformat(), end.isoformat()


# ── 1. HazardModels ──────────────────────────────────────────────────────────
def gen_models(rng):
    # 2 models per hazard family (×7 = 14) + one extra calibration variant = 15.
    specs = []
    for ht in HAZARD_TYPES:
        specs.append((ht, "v1.0"))
        specs.append((ht, "v2.1"))
    specs.append(("Flood", "v3.0-calib"))
    resources, meta = [], []
    for i, (ht, ver) in enumerate(specs, start=1):
        rid = str(uuid.uuid5(uuid.UUID(HM_GRAPHID), f"hazard_model:{i}"))
        method = METHOD_FOR[ht]
        name = f"{ht} model {ver}"
        rp = rng.choice([10, 25, 50, 100, 200, 500])
        res_m = rng.choice([5, 10, 25, 50])
        pnote = (f"{method} solver at {res_m} m resolution; {rp}-year return period; "
                 f"calibrated against the {ver} baseline.")
        note = f"Reviewer: parameters frozen for the Aonach Mór resilience demo ({ver})."
        tiles = [
            _tile(HM_GRAPHID, rid, "name", {_nid(HM_GRAPHID, "name"): name}, "name", i),
            _tile(HM_GRAPHID, rid, "version", {_nid(HM_GRAPHID, "version"): ver}, "ver", i),
            _tile(HM_GRAPHID, rid, "hazard_type", {_nid(HM_GRAPHID, "hazard_type"): ht}, "ht", i),
            _tile(HM_GRAPHID, rid, "method", {_nid(HM_GRAPHID, "method"): method}, "meth", i),
            _tile(HM_GRAPHID, rid, "parameters_note",
                  {_nid(HM_GRAPHID, "parameters_note"): pnote}, "pn", i),
            _tile(HM_GRAPHID, rid, "notes", {_nid(HM_GRAPHID, "notes"): note}, "note", i),
        ]
        resources.append(_resource(HM_GRAPHID, rid, name, pnote, tiles))
        meta.append({"rid": rid, "hazard_type": ht, "name": name})
    return resources, meta


# ── 2. ScenarioEvents ──────────────────────────────────────────────────────────
SCENARIO_NAMES = [
    ("Cyclone landfall", "Stoirm Bhealtaine"), ("Cyclone landfall", "Hairicín Samhna"),
    ("Coastal flood", "Tuile Chósta an Fhómhair"), ("Coastal flood", "Borradh Mara na Nollag"),
    ("Pluvial flood", "Tuile Bháistí an Earraigh"), ("Pluvial flood", "Tuile Abhann Thuaidh"),
    ("Pluvial flood", "Maidhm Bháistí na Cathrach"), ("Earthquake", "Crith Talún Chnoc an Chaisil"),
    ("Earthquake", "Crith Talún Imeall Eanach"), ("Heatwave", "Tonn Teasa an tSamhraidh"),
    ("Heatwave", "Tonn Teasa Iúil"), ("Mass-gathering incident", "Féile Cé an Rí"),
    ("Mass-gathering incident", "Cruinniú Baile Thoir"), ("Cyclone landfall", "Stoirm Dhéanach"),
    ("Coastal flood", "Borradh Mara Trá Theas"),
]


def gen_scenarios(rng, model_meta):
    by_haz = {}
    for m in model_meta:
        by_haz.setdefault(m["hazard_type"], []).append(m)
    resources, meta = [], []
    for i, (etype, nm) in enumerate(SCENARIO_NAMES, start=1):
        rid = str(uuid.uuid5(uuid.UUID(SE_GRAPHID), f"scenario:{i}"))
        sid = f"SCN-{i:03d}"
        start, end = _date(rng, 2021, 2026)
        # models used = all models whose hazard family this event generates
        fams = EVENT_TYPE_HAZARDS[etype]
        used = [m for f in fams for m in by_haz.get(f, [])]
        used_val = [{"resourceId": m["rid"]} for m in used]
        note = f"Scenario {sid}: {etype.lower()} exercise for the Aonach Mór resilience demo."
        tiles = [
            _tile(SE_GRAPHID, rid, "scenario_id", {_nid(SE_GRAPHID, "scenario_id"): sid}, "sid", i),
            _tile(SE_GRAPHID, rid, "name", {_nid(SE_GRAPHID, "name"): nm}, "name", i),
            _tile(SE_GRAPHID, rid, "event_type", {_nid(SE_GRAPHID, "event_type"): etype}, "et", i),
            _tile(SE_GRAPHID, rid, "event_span",
                  {_nid(SE_GRAPHID, "start_date"): start, _nid(SE_GRAPHID, "end_date"): end},
                  "span", i),
            _tile(SE_GRAPHID, rid, "hazard_models_used",
                  {_nid(SE_GRAPHID, "hazard_models_used"): used_val}, "hmu", i),
            _tile(SE_GRAPHID, rid, "notes", {_nid(SE_GRAPHID, "notes"): note}, "note", i),
        ]
        resources.append(_resource(SE_GRAPHID, rid, nm, note, tiles))
        meta.append({"rid": rid, "name": nm, "event_type": etype, "hazards": fams,
                     "span": (start, end)})
    return resources, meta


# ── 3. HazardFootprints ────────────────────────────────────────────────────────
# per-hazard target footprint counts → ~94
FOOTPRINT_COUNTS = {"Flood": 20, "Storm surge": 14, "Cyclone": 12, "Wind": 12,
                    "Earthquake": 12, "Heat": 12, "Crowding": 12}


def _ward_polygon_fc(sel):
    feats = []
    for w in sel:
        for ring in w["rings"]:
            feats.append({"type": "Feature",
                          "properties": {"ward": w["strip"], "kind": "ward"},
                          "geometry": {"type": "Polygon", "coordinates": [[list(p) for p in ring]]}})
    return {"type": "FeatureCollection", "features": feats}


def _ribbon_fc(ring, kind, label):
    return {"type": "FeatureCollection", "features": [
        {"type": "Feature", "properties": {"kind": kind, "feature": label},
         "geometry": {"type": "Polygon", "coordinates": [[list(p) for p in ring]]}}]}


def _pick_ward_cluster(wards, rng, span, coastal_bias=False, bld_seed=True):
    seed_pool = [w for w in wards if w["is_bld"]] if bld_seed else wards
    if coastal_bias:
        cz = [w for w in seed_pool if w["zone"] == "coastal"]
        seed_pool = cz or seed_pool
    seed = rng.choice(seed_pool)
    ordered = sorted(wards, key=lambda w: (w["centroid"][0] - seed["centroid"][0]) ** 2
                     + (w["centroid"][1] - seed["centroid"][1]) ** 2)
    k = rng.randint(*span)
    return ordered[:k]


def gen_footprints(rng, wards, model_meta, scenario_meta):
    by_haz_models = {}
    for m in model_meta:
        by_haz_models.setdefault(m["hazard_type"], []).append(m)
    scen_by_haz = {}
    for s in scenario_meta:
        for h in s["hazards"]:
            scen_by_haz.setdefault(h, []).append(s)

    rivers = _line_features("rivers.geojson")
    coast = _line_features("coastline.geojson")

    resources, meta, mapping = [], [], []
    i = 0
    for ht, n in FOOTPRINT_COUNTS.items():
        for _ in range(n):
            i += 1
            rid = str(uuid.uuid5(uuid.UUID(HF_GRAPHID), f"footprint:{i}"))

            # geometry + ward coverage
            if ht == "Flood":
                name_line, pts = rng.choice(rivers)
                seg = _runseg(pts, rng, 8, 24)
                ring = _buffer_line(seg, rng.uniform(0.003, 0.006))
                fc = _ribbon_fc(ring, "flood-river", name_line or "abhainn")
                anchor = _centroid(seg)
                sel = _coverage(wards, [ring], anchor)
                place = name_line or "an abhainn"
                kind = "river corridor"
            elif ht == "Storm surge":
                name_line, pts = rng.choice(coast)
                seg = _runseg(pts, rng, 8, 24)
                ring = _buffer_line(seg, rng.uniform(0.004, 0.008))
                fc = _ribbon_fc(ring, "storm-surge-coast", name_line or "cósta")
                anchor = _centroid(seg)
                sel = _coverage(wards, [ring], anchor)
                # prefer coastal building-wards when ribbon misses all centroids
                if not any(w["is_bld"] for w in sel):
                    sel.append(_nearest_ward(wards, anchor))
                place = "an chósta"
                kind = "coastal strip"
            else:  # ward-anchored family
                span = WARD_SPAN[ht]
                sel = _pick_ward_cluster(wards, rng, span,
                                         coastal_bias=(ht == "Crowding" and rng.random() < 0.3))
                fc = _ward_polygon_fc(sel)
                anchor = _centroid([w["centroid"] for w in sel])
                place = sel[0]["strip"]
                kind = {"Cyclone": "wind swath", "Wind": "wind swath",
                        "Earthquake": "shaking extent", "Heat": "heat-island patch",
                        "Crowding": "gathering area"}[ht]

            cov_wards = sorted({w["strip"] for w in sel})
            cov_wards_bld = sorted({w["strip"] for w in sel if w["is_bld"]})
            cov_zones = sorted({w["zone"] for w in sel if w["zone"]})

            # bands correlate: pick an intensity rank, derive severity
            bands = INTENSITY[ht]
            rank = rng.randrange(len(bands))
            intensity = bands[rank]
            severity = SEVERITY[min(3, int(rank / max(1, len(bands) - 1) * 3 + rng.randint(0, 1)))]
            confidence = rng.choice(CONFIDENCE)

            model = rng.choice(by_haz_models[ht])
            scen = rng.choice(scen_by_haz.get(ht) or scenario_meta)
            ss, se = scen["span"]
            pstart, pend = ss, se  # phenomenon within the scenario bracket

            name = f"{ht} footprint — {place} ({intensity.split('—')[-1].strip()})"
            wtxt = ", ".join(cov_wards_bld or cov_wards)
            note = (f"{ht} {kind} produced by '{model['name']}' during scenario "
                    f"'{scen['name']}'. Covers wards: {wtxt}. Zones: "
                    f"{', '.join(cov_zones) or 'n/a'}. severity={severity}.")

            tiles = [
                _tile(HF_GRAPHID, rid, "name", {_nid(HF_GRAPHID, "name"): name}, "name", i),
                _tile(HF_GRAPHID, rid, "hazard_type", {_nid(HF_GRAPHID, "hazard_type"): ht}, "ht", i),
                _tile(HF_GRAPHID, rid, "intensity_band",
                      {_nid(HF_GRAPHID, "intensity_band"): intensity}, "ib", i),
                _tile(HF_GRAPHID, rid, "severity_band",
                      {_nid(HF_GRAPHID, "severity_band"): severity}, "sb", i),
                _tile(HF_GRAPHID, rid, "geometry", {_nid(HF_GRAPHID, "geometry"): fc}, "geo", i),
                _tile(HF_GRAPHID, rid, "area_dimension",
                      {_nid(HF_GRAPHID, "area_sqkm"): _fc_area_sqkm(fc)}, "area", i),
                _tile(HF_GRAPHID, rid, "phenomenon_span",
                      {_nid(HF_GRAPHID, "phenomenon_start"): pstart,
                       _nid(HF_GRAPHID, "phenomenon_end"): pend}, "span", i),
                _tile(HF_GRAPHID, rid, "produced_by_model",
                      {_nid(HF_GRAPHID, "produced_by_model"): [{"resourceId": model["rid"]}]}, "pbm", i),
                _tile(HF_GRAPHID, rid, "scenario",
                      {_nid(HF_GRAPHID, "scenario"): [{"resourceId": scen["rid"]}]}, "scn", i),
                _tile(HF_GRAPHID, rid, "confidence",
                      {_nid(HF_GRAPHID, "confidence"): confidence}, "cb", i),
                _tile(HF_GRAPHID, rid, "notes", {_nid(HF_GRAPHID, "notes"): note}, "note", i),
            ]
            fres = _resource(HF_GRAPHID, rid, name, note, tiles)
            # Structured coverage tags (the join key emit_exposure reads directly from
            # the footprint bundle — resource["coverage"]["wards"]). STRIPPED ward names,
            # == each building's `site` key; zones == the footprints.csv zone vocabulary.
            # The emitter ignores this extra top-level key; exposure reads it.
            fres["coverage"] = {"wards": cov_wards_bld or cov_wards, "zones": cov_zones}
            resources.append(fres)
            mapping.append({
                "footprint_rid": rid, "hazard_type": ht, "intensity_band": intensity,
                "severity_band": severity, "produced_by_model": model["rid"],
                "scenario": scen["rid"],
                "wards": cov_wards_bld or cov_wards,   # STRIPPED, == building `site` key
                "wards_all": cov_wards, "zones": cov_zones,
            })
            meta.append({"rid": rid, "ht": ht})
    return resources, meta, mapping


def _runseg(pts, rng, lo, hi):
    """A contiguous run of `lo..hi` vertices from a LineString (the stretch the
    footprint ribbon follows)."""
    k = min(len(pts), rng.randint(lo, hi))
    if k >= len(pts):
        return list(pts)
    start = rng.randint(0, len(pts) - k)
    return list(pts[start:start + k])


# ── orchestration ─────────────────────────────────────────────────────────────
def generate(seed):
    rng = random.Random(seed)
    wards = load_wards()
    hm_res, hm_meta = gen_models(rng)
    se_res, se_meta = gen_scenarios(rng, hm_meta)
    hf_res, hf_meta, mapping = gen_footprints(rng, wards, hm_meta, se_meta)
    graphs = {
        HM_GRAPHID: build_graph(HM_GRAPHID, "hazard_model", "HazardModel",
                                "http://www.w3.org/ns/sosa/Procedure"),
        SE_GRAPHID: build_graph(SE_GRAPHID, "scenario_event", "ScenarioEvent",
                                CRM + "E5_Event"),
        HF_GRAPHID: build_graph(HF_GRAPHID, "hazard_footprint", "HazardFootprint",
                                "http://www.w3.org/ns/prov#Entity"),
    }
    return graphs, {"hazard_model": hm_res, "scenario_event": se_res,
                    "hazard_footprint": hf_res}, mapping


def main():
    ap = argparse.ArgumentParser(description="Emit HazardModel / ScenarioEvent / HazardFootprint "
                                             "resources for the Aonach Mór demo.")
    ap.add_argument("--seed", type=int, default=1904)
    ap.add_argument("--out", type=Path, required=True, help="emit data_dir to populate")
    args = ap.parse_args()

    graphs, resources, mapping = generate(args.seed)
    gdir = args.out / "graphs"
    gdir.mkdir(parents=True, exist_ok=True)
    for gid, g in graphs.items():
        (gdir / f"{gid}.json").write_text(json.dumps(g, ensure_ascii=False))
    for key, res in resources.items():
        rdir = args.out / "resources" / key
        rdir.mkdir(parents=True, exist_ok=True)
        (rdir / f"{key}.json").write_text(
            json.dumps({"business_data": {"resources": res}}, ensure_ascii=False))

    print(f"HM_GRAPHID={HM_GRAPHID}")
    print(f"SE_GRAPHID={SE_GRAPHID}")
    print(f"HF_GRAPHID={HF_GRAPHID}")
    print(f"wrote {len(resources['hazard_model'])} hazard models, "
          f"{len(resources['scenario_event'])} scenarios, "
          f"{len(resources['hazard_footprint'])} footprints")
    print(f"footprint rid scheme: uuid5(HF_GRAPHID, 'footprint:<i>')  "
          f"e.g. footprint:1 = {uuid.uuid5(uuid.UUID(HF_GRAPHID), 'footprint:1')}")


if __name__ == "__main__":
    main()

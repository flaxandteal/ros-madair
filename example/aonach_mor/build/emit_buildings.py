#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# © Flax & Teal Limited. Aonach Mór — building synthesis for the REAL ros-madair-emit
# path (translation of ros-madair-docs/scripts/gen-aonach-mor.mjs `genFull`).
#
# genFull hand-wrote Parquet directly; this produces the SAME realistic corpus as
# resource-JSON bundles that ros-madair-emit ingests, so the demo runs the real
# engine (melt substrate + layering) instead of a JS reimplementation. The realism
# mechanism is unchanged: real OSM footprints (geometry + ward + zone) and the
# LLM-authored skeleton of per-zone conditional attribute distributions — each
# building's typology is SAMPLED from its zone, so the joints (URM in coastal/
# low-income, steel in the industrial belt, RC high-rise + listed heritage in the
# core) are realistic. The believability engine (locality naming, bespoke notes,
# correlated attributes) is reused from 04_buildings.py. Deterministic.
#
#   python example/aonach_mor/build/emit_buildings.py --count 3000 --out <data_dir>
# writes <data_dir>/graphs/<graphid>.json and <data_dir>/resources/building/buildings.json
# (and refreshes example/aonach_mor/graphs/resource_models/building/graph.json).

import argparse
import csv
import importlib.util
import json
import math
import os
import random
import uuid
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
REPO = ROOT.parent.parent  # RosMadair-sandbox-parquet
DOCS = REPO.parent / "ros-madair-docs"

# Reuse the hand-tuned believability pools + notes engine from stage 04.
_spec = importlib.util.spec_from_file_location("b04", HERE / "04_buildings.py")
b04 = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(b04)

GRAPHID = str(uuid.uuid5(uuid.NAMESPACE_URL, "aonach-mor/resource_models/building"))
MODEL_DIR = ROOT / "graphs/resource_models/building"


# ── 1. Building graph JSON (minimal shape emit consumes; real nodes/datatypes) ──
def nid(alias):
    return str(uuid.uuid5(uuid.UUID(GRAPHID), alias))


def build_graph():
    rows = list(csv.DictReader((MODEL_DIR / "nodes.csv").open()))
    by_alias = {r["alias"]: r for r in rows}

    def top_ancestor(alias):
        r = by_alias[alias]
        while r["parent_alias"]:
            r = by_alias[r["parent_alias"]]
        return r["alias"]

    root_alias = "building"
    root = {"nodeid": nid(root_alias), "name": "Building", "alias": root_alias,
            "datatype": "semantic", "graph_id": GRAPHID, "istopnode": True,
            "is_collector": False, "isrequired": False, "issearchable": True,
            "exportable": False, "sortorder": 0,
            "ontologyclass": "http://www.cidoc-crm.org/cidoc-crm/E22_Human-Made_Object"}
    nodes = [root]
    edges = []
    nodegroups = {}
    for r in rows:
        alias = r["alias"]
        ng_alias = top_ancestor(alias)
        is_top = not r["parent_alias"]
        nodes.append({
            "nodeid": nid(alias), "name": r["name"], "alias": alias,
            "datatype": r["datatype"], "graph_id": GRAPHID, "istopnode": False,
            "is_collector": is_top, "isrequired": r["required"] == "true",
            "issearchable": r["searchable"] == "true", "exportable": r["exportable"] == "true",
            "sortorder": int(r["sortorder"]), "ontologyclass": r["ontology_class"],
            "nodegroup_id": nid(ng_alias),
        })
        # edge from parent (or root) with the node's parent_property
        parent = r["parent_alias"] or root_alias
        edges.append({"edgeid": str(uuid.uuid5(uuid.UUID(GRAPHID), f"edge:{alias}")),
                      "domainnode_id": nid(parent), "rangenode_id": nid(alias),
                      "graph_id": GRAPHID, "ontologyproperty": r["parent_property"]})
        if is_top:
            card = "n" if r["datatype"].endswith("-list") else "1"
            nodegroups[ng_alias] = {"nodegroupid": nid(ng_alias), "cardinality": card,
                                    "parentnodegroup_id": None}
    return {"graph": [{"graphid": GRAPHID, "name": "Building", "root": root,
                       "nodes": nodes, "nodegroups": list(nodegroups.values()), "edges": edges}]}


# ── 2. concept label maps (emit the full collection label so it matches the vocab) ──
def gem_labels():
    out = {}
    for r in csv.DictReader((MODEL_DIR / "collections.csv").open()):
        if r["collection_name"] == "GEMTaxonomy":
            out[r["concept_label"].split(" — ")[0]] = r["concept_label"]
    return out


ZONE_CLUSTER = {
    "commercial_core": "Central business district", "residential_affluent": "Residential",
    "residential_mixed": "Residential", "low_income_informal": "Peri-urban",
    "coastal": "Coastal", "industrial": "Industrial", "institutional": "Central business district",
}
GEM_CATEGORY = {  # gem code prefix → 04 notes category + a plausible ordinary type-noun
    "CR": ("rc_frame", ["Árasán", "Ionad Siopadóireachta", "Óstán", "Teach"]),
    "S": ("steel", ["Monarcha", "Stóras"]),
    "MCF": ("masonry", ["Teach", "Árasán"]),
    "MR": ("masonry", ["Teach", "Árasán"]),
    "MUR": ("heritage", ["Teach", "Séipéal", "Teach Oidhreachta", "Margadh"]),
    "E": ("heritage", ["Teach Cré", "Teach"]),
    "W": ("timber", ["Margadh", "Teach", "Séipéal"]),
    "MIX": ("mixed", ["Árasán", "Ionad Siopadóireachta"]),
}
ROLE_TYPE = {  # critical role → an Irish type-noun (reuses 04's cores/role branch)
    "Hospital": "Ospidéal", "Fire station": "Stáisiún Dóiteáin", "Police station": "Stáisiún Gardaí",
    "Emergency operations centre": "Ionad Éigeandála", "Shelter": "Halla Pobail",
    "Muster point": "Ionad Cruinnithe", "Electrical substation": "Fo-stáisiún Leictreach",
    "Water / pumping station": "Stáisiún Uisce", "School": "Scoil",
}


def weighted(d, rng):
    items = [(k, w) for k, w in d.items() if w > 0]
    return rng.choices([k for k, _ in items], [w for _, w in items])[0]


def gem_cat(code):
    pre = code.split("/")[0].split("+")[0]
    return GEM_CATEGORY.get(pre, GEM_CATEGORY.get(code.split("/")[0], ("masonry", ["Teach"])))


# ── critical-service facilities: FIXED city-wide totals, decoupled from the ×N
#    replica multiplier (there are only a handful of real hospitals/EOCs however big
#    the housing stock is). Placed once, on footprints in a suitable zone, with a
#    clamped minimum height. Shelter / Muster point / School are NOT here — they stay
#    per-building capability tags ("this hall can serve as a shelter"), which plausibly
#    scale with the stock.
CRITICAL_FIXED = {"Hospital": 8, "Emergency operations centre": 2, "Fire station": 14,
                  "Police station": 14, "Water / pumping station": 10, "School": 60,
                  "Electrical substation": 9}
# A true FACILITY is the building itself (named for its ward, capped). Shelter and
# Muster point are NOT facilities — they are capability TAGS a hall/block carries
# ("designated as a shelter"), so they ride ordinary buildings and keep the ordinary
# name. School joins the facilities: 63k "Scoil" is as implausible as 320 hospitals.
TRUE_FACILITIES = set(CRITICAL_FIXED)
CAPABILITY_TAGS = {"Shelter", "Muster point"}
MIN_H = {"Hospital": 15, "Emergency operations centre": 14, "Fire station": 9,
         "Police station": 9, "Water / pumping station": 5, "School": 7}
CRIT_ZONES = {
    "Hospital": {"commercial_core", "residential_affluent", "coastal", "institutional"},
    "Emergency operations centre": {"commercial_core", "institutional"},
    "Fire station": {"commercial_core", "residential_mixed", "industrial", "coastal"},
    "Police station": {"commercial_core", "residential_mixed", "residential_affluent"},
    "Water / pumping station": {"industrial", "coastal", "low_income_informal"},
    "School": {"residential_affluent", "residential_mixed", "commercial_core", "coastal", "institutional"},
    "Electrical substation": {"industrial", "residential_mixed", "commercial_core", "low_income_informal"},
}

# ── naming pools (believable at 763k) ───────────────────────────────────────
# Residential bulk → ADDRESS (<house-number> <street>); other premises →
# <type-noun> <locality>; facilities → <type-noun> <ward>. A number in an ADDRESS
# reads true (unlike an index suffix). Same Irish morpheme grammar as the renaming
# layer's fallback_stems, so generated streets/localities share the corpus namespace.
_NOUNS = ["Cnoc", "Gleann", "Cuan", "Dún", "Cé", "Ros", "Trá", "Baile", "Cill", "Sliabh",
          "Loch", "Carraig", "Tobar", "Garraí", "Droichead", "Páirc", "Rinn", "Mainistir",
          "Currach", "Faiche", "Machaire", "Móinéar", "Doire", "Lios", "Ráth", "Tulach",
          "Mullach", "Inse", "Port", "Caol", "Bruach", "Gort", "Léana", "Coill"]
_ADJS = ["Bán", "Mór", "Beag", "Dubh", "Rua", "Gorm", "Úr", "Liath", "Garbh", "Mín", "Ard",
         "Íseal", "Fada", "Crua", "Glas", "Thuaidh", "Theas", "Thoir", "Thiar", "Álainn",
         "Sléibhe", "Gaoithe", "Fliuch", "Tirim", "Ciúin", "Naofa"]
LOCALITIES = [f"{n} {a}" for n in _NOUNS for a in _ADJS]  # 34×26 = 884
_STREET_PREFIX = ["Slí", "Sráid", "Bóthar", "Lána", "Cé", "Ascaill", "Plás", "Céide"]
STREETS = [f"{p} {loc}" for p in _STREET_PREFIX for loc in LOCALITIES[:64]]  # 8×64 = 512
RESIDENTIAL = {"Teach", "Árasán"}

# Sites are the footprint's OWN ward (genFull's ~42 wards), not a 13-cell Voronoi —
# so building density follows the real footprint distribution, not a coarse remap.
SITE_GRAPHID = str(uuid.uuid5(uuid.NAMESPACE_URL, "aonach-mor/resource_models/site"))


def strip_ward(w):
    p = w.rsplit(" ", 1)
    return p[0] if len(p) == 2 and p[1].isdigit() else w


def site_rid(ward_clean):
    return str(uuid.uuid5(uuid.UUID(SITE_GRAPHID), f"site:{ward_clean}"))


def load_sites():
    ws = json.loads((ROOT / "geo/processed/iconic_wards.json").read_text())["wards"]
    return [(f"site-{i + 1:03d}", float(w["seed_lon"]), float(w["seed_lat"]), w["aonach_mor_label"])
            for i, w in enumerate(ws)]


def nearest_site(lng, lat, sites):
    best, bd = sites[0], 1e18
    for s in sites:
        d = (lng - s[1]) ** 2 + (lat - s[2]) ** 2
        if d < bd:
            bd, best = d, s
    return best[0]


def jitter(lng, lat, rng, radius=0.0008):
    # uniform in a ~90 m disc so replicas disperse into urban fabric rather than
    # stack on the footprint; combined across ~150 m-spaced real footprints this
    # tiles into continuous density (the footprints are a sparse OSM sample).
    t = rng.random() * 2 * math.pi
    r = radius * math.sqrt(rng.random())
    return lng + r * math.cos(t), lat + r * math.sin(t)


def _record(gid, f, lng, lat, rng, zones, ages, glabels, sites, forced_role=None, used_names=None):
    """Build one Building resource (and a compact audit stub). Attributes are
    (re-)sampled from the footprint's zone every call, so replicas of one footprint
    differ."""
    zkey = f["zone"] if f["zone"] in zones else "residential_mixed"
    z = zones[zkey]
    gem_code = weighted(z["gem"], rng)
    gem = glabels.get(gem_code, gem_code)
    heritage = weighted(z["heritage"], rng)
    age = weighted(z["age_band"], rng)
    ylo, yhi = ages.get(age, [1950, 1980])
    year = rng.randint(ylo, yhi)
    cat, ordinary = gem_cat(gem_code)
    if forced_role:                        # a true FACILITY — the building is it
        role, tag = forced_role, None
        type_noun = ROLE_TYPE[role]
    else:
        rr = weighted(z["roles"], rng)
        # facilities (incl School) are placed ONLY by the fixed loop; per-building
        # sampling yields at most a capability TAG (Shelter / Muster point) that
        # rides an ORDINARY building without changing its type or name.
        role, tag = None, (rr if rr in CAPABILITY_TAGS else None)
        type_noun = rng.choice(ordinary)
    # Height from the TYPE, not the footprint's real `levels` (the type is re-sampled
    # per building, so a 1-level footprint re-sampled as a hotel must not be "3 m").
    hr = b04.TYPES.get(type_noun)
    lo, hi = hr[3] if hr else (3, 8)
    height = rng.randint(lo, hi)
    if year < 1920:
        height = min(height, rng.randint(6, 16))
    if forced_role:
        height = max(height, MIN_H.get(role, height))
    ward_clean = strip_ward(f["ward"])
    # Naming: facility → <type> <ward> (deduped); residential bulk → <house-no>
    # <street> (an address — the believable way to have 100k houses); other premises
    # → <type-noun> <locality>.
    if forced_role:
        name = f"{type_noun} {ward_clean}"
        if used_names is not None and name in used_names:
            name = f"{name}, {rng.choice(b04.FEATURES)}"
        if used_names is not None:
            used_names.add(name)
    elif type_noun in RESIDENTIAL:
        name = f"{rng.randint(1, 250)} {rng.choice(STREETS)}"
    else:
        name = f"{type_noun} {rng.choice(LOCALITIES)}"
    note = b04.notes_for(type_noun, role, cat, year, height, ward_clean, rng)
    if tag:  # a capability tag rides an ordinary building — append, don't rename
        note = f"{note.rstrip('.')} · designated {tag.lower()}."
    sr = role or tag  # whichever (if any) populates service_roles
    cluster = ZONE_CLUSTER[zkey]
    if role in ("Water / pumping station",) or type_noun in ("Monarcha", "Stóras"):
        cluster = "Industrial"
    site_id = site_rid(ward_clean)
    rid = str(uuid.uuid5(uuid.UUID(GRAPHID), f"building:{gid}"))

    def tile(ng_alias, data, tag):
        return {"tileid": str(uuid.uuid5(uuid.UUID(GRAPHID), f"tile:{gid}:{tag}")),
                "nodegroup_id": nid(ng_alias), "parenttile_id": None,
                "resourceinstance_id": rid, "sortorder": 0, "provisionaledits": None, "data": data}

    fc = {"type": "FeatureCollection", "features": [
        {"type": "Feature", "properties": {}, "geometry": {"type": "Point", "coordinates": [lng, lat]}}]}
    tiles = [
        tile("name", {nid("name"): name}, "name"),
        tile("location", {nid("footprint"): fc}, "loc"),
        tile("gem_taxonomy", {nid("gem_taxonomy"): gem}, "gem"),
        tile("heritage_status", {nid("heritage_status"): heritage}, "her"),
        tile("cluster_type", {nid("cluster_type"): cluster}, "clu"),
        tile("height_dimension", {nid("height_m"): height}, "ht"),
        tile("production", {nid("age_band_years"): age}, "age"),
        tile("notes", {nid("notes"): note}, "note"),
        tile("site", {nid("site"): [{"resourceId": site_id}]}, "site"),
    ]
    if sr:
        tiles.append(tile("service_roles", {nid("service_roles"): [sr]}, "role"))
    # TIV (every building): insured value driven by WHAT the building is (its responder
    # role, else its urban zone), modulated by height and a heritage premium. Spans ~10
    # lakh housing to several-hundred-crore critical facilities — the believable range a
    # responder/underwriter actually filters on ("worth over N crore"). Raw in `data`;
    # melts to the ordered store so the insurer window SUMs it over a footprint (ΣTIV).
    CRORE = 10_000_000
    _role_band = {  # INR crore, low..high
        "Hospital": (50, 500), "Emergency operations centre": (20, 120),
        "Fire station": (10, 60), "Police station": (10, 60), "School": (5, 50),
        "Water / pumping station": (3, 40), "Electrical substation": (5, 60),
        "Muster point": (1, 10),
    }
    _zone_band = {  # no responder role → value by urban zone
        "Central business district": (3, 40), "Port": (5, 60), "Industrial": (2, 35),
        "Heritage": (1, 20), "Coastal": (0.3, 5), "Residential": (0.15, 3),
        "Peri-urban": (0.1, 2),
    }
    lo_cr, hi_cr = _role_band.get(role) or _zone_band.get(cluster, (0.15, 3))
    base = rng.uniform(lo_cr, hi_cr) * CRORE
    tiv = int(base * (0.6 + 0.08 * height) * rng.uniform(0.75, 1.3))
    if heritage == "Listed":
        tiv = int(tiv * rng.uniform(1.2, 2.0))  # listed-building premium
    tiles.append(tile("value_dimension", {nid("insured_value"): tiv}, "tiv"))
    # Capacity: ONLY where a responder can USE the building — true facilities plus the
    # shelter/muster capability tags ("shelters/hospitals in range with capacity ≥N").
    # Ordinary housing has no responder capacity, so it carries none.
    cap = None
    if forced_role:
        cap = {"Hospital": rng.randint(80, 600), "Emergency operations centre": rng.randint(40, 120),
               "Fire station": rng.randint(20, 60), "Police station": rng.randint(20, 80),
               "Water / pumping station": rng.randint(5, 20), "School": rng.randint(200, 1200)}.get(role)
    elif tag == "Shelter":
        cap = rng.randint(50, 400)
    elif tag == "Muster point":
        cap = rng.randint(100, 800)
    if cap is not None:
        tiles.append(tile("capacity_dimension", {nid("capacity"): cap}, "cap"))
    res = {
        "resourceinstance": {"resourceinstanceid": rid, "graph_id": GRAPHID, "name": name,
            "legacyid": f"bldg-{gid:07d}",
            "descriptors": {"en": {"name": name, "description": note, "map_popup": ""}}},
        "tiles": tiles,
    }
    stub = {"name": name, "note": note, "role": sr, "gem": gem_code, "age": age,
            "height": height, "cluster": cluster, "lng": lng, "lat": lat, "site": site_id,
            "ward": ward_clean, "heritage": heritage, "type": type_noun}
    return res, stub


def building_stream(footprints, replicas, seed):
    """Yield (resource, stub) for the whole corpus: a fixed set of critical facilities
    first, then `replicas` re-sampled buildings per real footprint (replica 0 at the
    true coordinate, the rest jittered into the surrounding block)."""
    skel = json.loads((DOCS / "scripts/seed/skeleton.json").read_text())
    zones, ages, glabels = skel["zones"], skel["age_years"], gem_labels()
    rng = random.Random(seed)
    gid = 0
    facility_names = set()  # dedup the handful of facility <type> <ward> names

    # 1) fixed critical facilities — pick suitable footprints, one building each.
    by_zone = {}
    for idx, f in enumerate(footprints):
        by_zone.setdefault(f["zone"], []).append(idx)
    for role, n in CRITICAL_FIXED.items():
        # sorted(): CRIT_ZONES[role] is a set, so iterating it raw is hash-salted
        # per process — which would place the fixed facilities at DIFFERENT gids
        # (hence rids) in each emit process, and the sibling generators (persons/
        # insurance/exposure) recover facility rids via building_stream in their OWN
        # processes. Sorting makes facility placement seed-only-deterministic, so
        # those cross-model keyholder/coverage/exposure links resolve without
        # depending on a pinned PYTHONHASHSEED.
        pool = [i for z in sorted(CRIT_ZONES[role]) for i in by_zone.get(z, [])]
        if not pool:
            pool = list(range(len(footprints)))
        chosen = rng.sample(pool, min(n, len(pool)))
        for idx in chosen:
            gid += 1
            f = footprints[idx]
            # jitter the facility off the exact footprint so it does not stack on
            # that footprint's replica-0 (which stays at the true coordinate).
            clng, clat = jitter(float(f["lng"]), float(f["lat"]), rng)
            yield _record(gid, f, clng, clat, rng, zones, ages, glabels, None,
                          forced_role=role, used_names=facility_names)

    # 2) the housing/commercial stock — `replicas` per footprint, re-sampled + jittered.
    for f in footprints:
        lng0, lat0 = float(f["lng"]), float(f["lat"])
        for rep in range(replicas):
            gid += 1
            lng, lat = (lng0, lat0) if rep == 0 else jitter(lng0, lat0, rng)
            yield _record(gid, f, lng, lat, rng, zones, ages, glabels, None)


def audit(footprints, replicas, seed):
    import collections
    names = collections.Counter()
    notes = collections.Counter()
    roles = collections.Counter()
    sites = collections.Counter()
    coords = set()
    dup = 0
    nn_sample = []  # reservoir of (lng,lat)
    scroll = []     # reservoir of stubs
    total = 0
    var_targets = {0, len(footprints) // 2, len(footprints) - 1}  # 3 footprints to show replica variety
    variety = collections.defaultdict(list)
    # footprint index per yielded bulk record: criticals come first, then rep blocks
    n_crit = sum(min(n, len(footprints)) for n in CRITICAL_FIXED.values())
    rng = random.Random(seed ^ 0x5151)
    for res, s in building_stream(footprints, replicas, seed):
        total += 1
        names[s["name"]] += 1
        notes[s["note"]] += 1
        roles[s["role"] or "(none)"] += 1
        sites[s["ward"]] += 1
        key = (round(s["lng"], 6), round(s["lat"], 6))
        if key in coords:
            dup += 1
        else:
            coords.add(key)
        if len(nn_sample) < 4000:
            nn_sample.append((s["lng"], s["lat"]))
        elif rng.random() < 0.02:
            nn_sample[rng.randrange(4000)] = (s["lng"], s["lat"])
        if len(scroll) < 20:
            scroll.append(s)
        elif rng.random() < 0.01:
            scroll[rng.randrange(20)] = s
        if total > n_crit:
            fi = (total - n_crit - 1) // replicas
            if fi in var_targets and len(variety[fi]) < replicas:
                variety[fi].append(s)

    # nearest-neighbour distances (metres) on the sample
    import math as _m
    pts = nn_sample
    mlat, mlng = 111320.0, 111320.0 * _m.cos(_m.radians(13.0))
    nn = []
    for i in range(len(pts)):
        bd = 1e18
        xi, yi = pts[i]
        for j in range(len(pts)):
            if i == j:
                continue
            dx = (xi - pts[j][0]) * mlng
            dy = (yi - pts[j][1]) * mlat
            d = dx * dx + dy * dy
            if d < bd:
                bd = d
        nn.append(_m.sqrt(bd))
    nn.sort()

    def pct(p):
        return nn[int(p * (len(nn) - 1))] if nn else 0

    print(f"=== 780k believability audit: {len(footprints)} footprints × {replicas} replicas ===")
    print(f"total buildings: {total}")
    print(f"\nNAMES: {len(names)} unique ({100 * len(names) // total}%); top-repeat {names.most_common(1)[0][1]}×")
    print(f"NOTES: {len(notes)} unique ({100 * len(notes) // total}%); top-repeat {notes.most_common(1)[0][1]}×")
    print(f"\nSERVICE ROLES (criticals must stay a handful):")
    for k, v in roles.most_common():
        print(f"  {v:8}  {k}")
    print(f"\nSPATIAL: duplicate exact coords: {dup}")
    print(f"  nearest-neighbour spacing (m) over {len(nn)}-pt sample: "
          f"p10={pct(.1):.0f} p50={pct(.5):.0f} p90={pct(.9):.0f}")
    sc = sites.most_common()
    print(f"  buildings per ward — {len(sc)} wards; "
          f"max={sc[0][1]} ({sc[0][0]}) median={sc[len(sc)//2][1]} min={sc[-1][1]} ({sc[-1][0]})")
    for sid, c in sc[:6] + sc[-3:]:
        print(f"    {sid}: {c}")
    print(f"\nREPLICA VARIETY (one footprint's {replicas} re-sampled buildings — should differ):")
    fi = sorted(var_targets)[1]
    for s in variety[fi][:8]:
        print(f"  {s['type']:20} gem={s['gem']:10} age={s['age']:10} h={s['height']:3}m  {s['name']:28} | {s['note'][:60]}")
    mr = names.most_common(1)[0][0]
    print(f"\nMOST-REPEATED NAME '{mr}' — do its buildings differ?")
    seen = 0
    for res, s in building_stream(footprints, replicas, seed):
        if s["name"] == mr:
            print(f"  gem={s['gem']:10} age={s['age']:10} h={s['height']:3}m her={s['heritage']:10} | {s['note'][:55]}")
            seen += 1
            if seen >= 4:
                break
    print(f"\nSCROLL (20 random):")
    for s in scroll:
        rl = f"[{s['role']}]" if s["role"] else ""
        print(f"  {s['name']:30}{rl:16} {s['gem']:10} {s['age']:10} {s['height']:3}m {s['cluster']:22} | {s['note'][:55]}")


# ── Site resources: one per footprint ward (genFull's ~42), so building→site
#    links resolve and density follows the real footprint distribution. ──────────
def _sidn(alias):
    return str(uuid.uuid5(uuid.UUID(SITE_GRAPHID), alias))


def build_site_graph():
    P = "http://www.cidoc-crm.org/cidoc-crm/"
    root = {"nodeid": _sidn("site"), "name": "Site", "alias": "site", "datatype": "semantic",
            "graph_id": SITE_GRAPHID, "istopnode": True, "is_collector": False, "isrequired": False,
            "issearchable": True, "exportable": False, "sortorder": 0, "ontologyclass": P + "E53_Place"}

    def nd(alias, name, oc, prop, datatype="string", collection=None):
        node = {"nodeid": _sidn(alias), "name": name, "alias": alias, "datatype": datatype,
                "graph_id": SITE_GRAPHID, "istopnode": False, "is_collector": True, "isrequired": False,
                "issearchable": True, "exportable": True, "sortorder": 0, "ontologyclass": oc,
                "nodegroup_id": _sidn(alias)}
        if collection:
            node["config"] = {"rdmCollection": collection}
        return (node,
                {"nodegroupid": _sidn(alias), "cardinality": "1", "parentnodegroup_id": None},
                {"edgeid": str(uuid.uuid5(uuid.UUID(SITE_GRAPHID), "e:" + alias)),
                 "domainnode_id": _sidn("site"), "rangenode_id": _sidn(alias),
                 "graph_id": SITE_GRAPHID, "ontologyproperty": prop})
    nm = nd("name", "Name", P + "E41_Appellation", P + "P1_is_identified_by")
    ad = nd("admin_division", "Admin division", P + "E55_Type", P + "P2_has_type")
    # zone (E55 Type, reusing the ClusterType vocabulary): the urban character of the site,
    # mirroring Building.cluster_type — so "registered in a coastal site" can filter.
    zn = nd("zone", "Zone", P + "E55_Type", P + "P2_has_type", datatype="concept", collection="ClusterType")
    return {"graph": [{"graphid": SITE_GRAPHID, "name": "Site", "root": root,
                       "nodes": [root, nm[0], ad[0], zn[0]], "nodegroups": [nm[1], ad[1], zn[1]],
                       "edges": [nm[2], ad[2], zn[2]]}]}


def site_resources(footprints):
    import collections as _c
    wards = sorted({strip_ward(f["ward"]) for f in footprints})
    # each ward's zone = the modal footprint zone, mapped to its ClusterType label.
    ward_zone = {}
    by_ward = _c.defaultdict(_c.Counter)
    for f in footprints:
        by_ward[strip_ward(f["ward"])][f.get("zone", "residential_mixed")] += 1
    for w in wards:
        zkey = by_ward[w].most_common(1)[0][0] if by_ward[w] else "residential_mixed"
        ward_zone[w] = ZONE_CLUSTER.get(zkey, "Residential")
    out = []
    for w in wards:
        rid = site_rid(w)
        out.append({
            "resourceinstance": {"resourceinstanceid": rid, "graph_id": SITE_GRAPHID, "name": w,
                "legacyid": None, "descriptors": {"en": {"name": w, "description": "Ward", "map_popup": ""}}},
            "tiles": [
                {"tileid": str(uuid.uuid5(uuid.UUID(SITE_GRAPHID), f"t:{w}:name")), "nodegroup_id": _sidn("name"),
                 "parenttile_id": None, "resourceinstance_id": rid, "sortorder": 0, "provisionaledits": None,
                 "data": {_sidn("name"): w}},
                {"tileid": str(uuid.uuid5(uuid.UUID(SITE_GRAPHID), f"t:{w}:adm")), "nodegroup_id": _sidn("admin_division"),
                 "parenttile_id": None, "resourceinstance_id": rid, "sortorder": 0, "provisionaledits": None,
                 "data": {_sidn("admin_division"): "Ward"}},
                {"tileid": str(uuid.uuid5(uuid.UUID(SITE_GRAPHID), f"t:{w}:zone")), "nodegroup_id": _sidn("zone"),
                 "parenttile_id": None, "resourceinstance_id": rid, "sortorder": 0, "provisionaledits": None,
                 "data": {_sidn("zone"): ward_zone[w]}},
            ]})
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--footprints", type=int, default=0, help="footprints to use (0 = all)")
    ap.add_argument("--replicas", type=int, default=int(os.environ.get("AONACH_REPLICAS", "23")))
    ap.add_argument("--count", type=int, default=0, help="back-compat: N footprints, replicas=1")
    ap.add_argument("--seed", type=int, default=1904)
    ap.add_argument("--out", type=Path, help="emit data_dir to populate (writes bundle; omit for audit-only)")
    ap.add_argument("--audit", action="store_true")
    args = ap.parse_args()

    graph = build_graph()
    (MODEL_DIR / "graph.json").write_text(json.dumps(graph, indent=1, ensure_ascii=False))

    fp = list(csv.DictReader((DOCS / "scripts/seed/footprints.csv").open()))
    replicas = args.replicas
    if args.count:  # back-compat path
        rng = random.Random(args.seed)
        rng.shuffle(fp)
        fp = fp[:args.count]
        replicas = 1
    elif args.footprints:
        fp = fp[:args.footprints]

    if args.audit:
        audit(fp, replicas, args.seed)
        return

    if not args.out:
        ap.error("--out required unless --audit")
    gdir = args.out / "graphs"
    rdir = args.out / "resources" / "building"
    gdir.mkdir(parents=True, exist_ok=True)
    rdir.mkdir(parents=True, exist_ok=True)
    (gdir / f"{GRAPHID}.json").write_text(json.dumps(graph, ensure_ascii=False))
    resources = [res for res, _ in building_stream(fp, replicas, args.seed)]
    (rdir / "buildings.json").write_text(json.dumps({"business_data": {"resources": resources}}, ensure_ascii=False))
    # Site resources (one per footprint ward) so building→site links resolve.
    (gdir / f"{SITE_GRAPHID}.json").write_text(json.dumps(build_site_graph(), ensure_ascii=False))
    sdir = args.out / "resources" / "site"
    sdir.mkdir(parents=True, exist_ok=True)
    sres = site_resources(fp)
    (sdir / "sites.json").write_text(json.dumps({"business_data": {"resources": sres}}, ensure_ascii=False))
    print(f"graphid={GRAPHID}  site_graphid={SITE_GRAPHID}")
    print(f"wrote {len(resources)} building resources ({len(fp)}×{replicas}) + {len(sres)} site resources")


if __name__ == "__main__":
    main()

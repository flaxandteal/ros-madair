#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# © Flax & Teal Limited. Aonach Mór — the SHARED coherent "world" assignment.
#
# The richness problem: the per-model generators used to assign every link
# independently and at random, so nothing composed — the graph was a star around
# Site and no deep multi-hop chains emerged. This module computes ONE deterministic
# relational world that every generator reads, so chains exist BY CONSTRUCTION:
#
#   Person ──employer──▶ Organisation ──occupies──▶ Building ──site──▶ Site
#                              ▲                        │
#     InsurancePolicy ─insured─┘            covered_by / exposed_by
#                                                       ▼
#                                   Coverage / Exposure ──▶ Footprint ──▶ Scenario
#
# Coherence rules (NOT random):
#  • Each FACILITY (hospital/EOC/…) is occupied by its own SECTOR-MATCHED operating
#    org, so "the org that runs this hospital" is real.
#  • Ordinary non-residential stock is occupied by ward-coherent commercial orgs
#    whose sizes follow a numpy power-law (a few big holders, a long tail of one-offs).
#  • A person is EMPLOYED by an org (size-weighted; clinical staff by hospital orgs)
#    and stationed in that org's ward.
#  • A building's KEYHOLDER is a person employed by the org that occupies it.
#  • (Downstream) an insurer's covered buildings are what its INSURED org occupies.
#
# Deterministic: seeded, and relies on emit_buildings.building_stream being
# seed-only deterministic (the sorted() fix on critical-facility placement).

import importlib.util
import random
import uuid
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent


def _load(mod, fname):
    spec = importlib.util.spec_from_file_location(mod, HERE / fname)
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


eb = _load("emit_buildings", "emit_buildings.py")

NS = uuid.NAMESPACE_URL
ORG_GRAPHID = str(uuid.uuid5(NS, "aonach-mor/resource_models/organisation"))
PERSON_GRAPHID = str(uuid.uuid5(NS, "aonach-mor/resource_models/person"))

ROLE_SECTOR = {
    "Hospital": "Health", "Emergency operations centre": "Emergency management",
    "Fire station": "Emergency management", "Police station": "Public safety",
    "Water / pumping station": "Utilities", "School": "Education",
}
CLUSTER_SECTOR = {
    "Industrial": "Industrial", "Central business district": "Commercial",
    "Coastal": "Commercial", "Residential": "Commercial", "Peri-urban": "Commercial",
}
CLINICAL_FRACTION = 0.18
# Staff a facility org employs (it occupies ONE building but needs many people) —
# drives the employment weight so facilities are reliably staffed → keyholders.
FAC_STAFF = {"Hospital": 24, "Emergency operations centre": 10, "Fire station": 8,
             "Police station": 8, "Water / pumping station": 4, "School": 14}


def _org_rid(i):
    return str(uuid.uuid5(uuid.UUID(ORG_GRAPHID), f"org:{i}"))


def _person_rid(i):
    return str(uuid.uuid5(uuid.UUID(PERSON_GRAPHID), f"person:{i}"))


def _size_band(n):
    if n >= 12:
        return "Large (250+)"
    if n >= 4:
        return "Medium (50-249)"
    if n >= 2:
        return "Small (10-49)"
    return "Micro (1-9)"


def build_world(fp, replicas, seed, n_orgs=None, n_persons=None, n_insurers=14):
    """Compute the coherent world; return orgs + persons with resolved cross-model
    rids plus the building→keyholder map. Deterministic for a given (fp, replicas, seed)."""
    rng = random.Random(seed ^ 0x5717)
    npr = np.random.default_rng(seed & 0xFFFFFFFF)

    # 1. Canonical building stream (same seed the building emit writes) → real rids.
    buildings = []  # (rid, role_or_None, btype, cluster, ward_clean)
    for res, s in eb.building_stream(fp, replicas, seed):
        buildings.append((res["resourceinstance"]["resourceinstanceid"],
                          s["role"], s["type"], s["cluster"], s["ward"]))
    occupiable = [b for b in buildings if b[2] not in eb.RESIDENTIAL]  # non-residential
    facilities = [b for b in occupiable if b[1] in eb.TRUE_FACILITIES]
    ordinary = [b for b in occupiable if b[1] not in eb.TRUE_FACILITIES]

    orgs = []
    oi = 0

    # (a) one sector-matched operating org per facility — coherent facility occupancy.
    for b in facilities:
        oi += 1
        orgs.append({"i": oi, "rid": _org_rid(oi), "sector": ROLE_SECTOR.get(b[1], "Public safety"),
                     "ward": b[4], "occupies": [b[0]], "_fac": [b], "size_band": None})

    # (b) commercial orgs over the ordinary non-residential stock, ward-coherent,
    #     power-law sizes (a few hold many, most hold one or two).
    by_ward = {}
    for b in ordinary:
        by_ward.setdefault(b[4], []).append(b)
    wards = sorted(by_ward)
    if ordinary:
        n_comm = n_orgs if n_orgs else max(len(wards), len(ordinary) // 5)
        sizes = npr.zipf(2.3, size=n_comm)
        sizes = np.clip(sizes, 1, max(2, len(ordinary) // 4))
        if sizes.sum():
            sizes = np.maximum(1, (sizes * (len(ordinary) / sizes.sum())).round().astype(int))
        wi = 0
        for k in np.argsort(-sizes):
            home = None
            for _ in range(len(wards)):
                w = wards[wi % len(wards)]
                wi += 1
                if by_ward[w]:
                    home = w
                    break
            if home is None:
                break  # ordinary stock exhausted
            want = int(sizes[k])
            occ = []
            for w in [home] + [x for x in wards if x != home]:
                while by_ward[w] and len(occ) < want:
                    occ.append(by_ward[w].pop())
                if len(occ) >= want:
                    break
            if not occ:
                continue
            oi += 1
            orgs.append({"i": oi, "rid": _org_rid(oi),
                         "sector": CLUSTER_SECTOR.get(occ[0][3], "Commercial"),
                         "ward": home, "occupies": [b[0] for b in occ], "_fac": [],
                         "size_band": None})

    # (c) insurers — a fixed financial pool (no premises occupancy needed).
    anchor = wards or sorted({b[4] for b in facilities}) or ["Aonach Mór"]
    for _ in range(n_insurers):
        oi += 1
        orgs.append({"i": oi, "rid": _org_rid(oi), "sector": "Financial and insurance",
                     "ward": rng.choice(anchor), "occupies": [], "_fac": [],
                     "size_band": "Medium (50-249)"})
    for o in orgs:
        if o["size_band"] is None:
            o["size_band"] = _size_band(len(o["occupies"]))

    # 2. People — employed by an org, weighted by STAFF NEED (a facility occupies one
    #    building but employs many), with EACH facility org guaranteed ≥1 employee so
    #    every facility reliably has a keyholder. Clinical staff go to hospital orgs.
    for o in orgs:
        if o["_fac"]:
            o["staff_need"] = FAC_STAFF.get(o["_fac"][0][1], 6)
        elif o["sector"] == "Financial and insurance":
            o["staff_need"] = 5
        else:
            o["staff_need"] = max(1, len(o["occupies"]))
    hosp_orgs = [o for o in orgs if any(r[1] == "Hospital" for r in o["_fac"])]
    fac_orgs = [o for o in orgs if o["_fac"]]
    weights = np.array([o["staff_need"] for o in orgs], dtype=float)
    weights = weights / weights.sum()
    n_people = n_persons if n_persons else max(len(fac_orgs) + len(orgs) * 2, len(facilities) * 4, 50)

    def _mk(i, o, clinical):
        return {"i": i, "rid": _person_rid(i), "employer_i": o["i"], "employer_rid": o["rid"],
                "ward": o["ward"], "is_clinical": bool(clinical), "keyholder_for": []}

    persons = []
    i = 0
    for o in fac_orgs:  # guarantee each facility org at least one employee
        i += 1
        persons.append(_mk(i, o, any(r[1] == "Hospital" for r in o["_fac"])))
    while i < n_people:  # the rest, weighted by staff need
        i += 1
        clinical = rng.random() < CLINICAL_FRACTION and bool(hosp_orgs)
        o = rng.choice(hosp_orgs) if clinical else orgs[int(npr.choice(len(orgs), p=weights))]
        persons.append(_mk(i, o, clinical))

    # 3. Keyholders — each facility's keyholder(s) drawn from the occupying org's staff.
    emp = {}
    for p in persons:
        emp.setdefault(p["employer_i"], []).append(p)
    building_keyholders = {}
    for o in orgs:
        staff = emp.get(o["i"], [])
        for b in o["_fac"]:
            if staff:
                khs = rng.sample(staff, min(len(staff), rng.randint(1, 2)))
                building_keyholders[b[0]] = [p["rid"] for p in khs]
                for p in khs:
                    p["keyholder_for"].append(b[0])

    return {
        "orgs": orgs, "persons": persons, "building_keyholders": building_keyholders,
        "n_occupiable": len(occupiable), "n_facilities": len(facilities),
        "ORG_GRAPHID": ORG_GRAPHID, "PERSON_GRAPHID": PERSON_GRAPHID,
    }


if __name__ == "__main__":  # quick self-check
    import argparse
    import csv
    ap = argparse.ArgumentParser()
    ap.add_argument("--footprints-count", type=int, default=1500)
    ap.add_argument("--replicas", type=int, default=1)
    ap.add_argument("--seed", type=int, default=1904)
    a = ap.parse_args()
    fp = list(csv.DictReader((eb.DOCS / "scripts/seed/footprints.csv").open()))
    r = random.Random(a.seed)
    r.shuffle(fp)
    fp = fp[:a.footprints_count]
    w = build_world(fp, a.replicas, a.seed)
    occ = sum(len(o["occupies"]) for o in w["orgs"])
    print(f"orgs={len(w['orgs'])}  persons={len(w['persons'])}  "
          f"occupiable={w['n_occupiable']}  facilities={w['n_facilities']}  "
          f"occupancy_links={occ}  keyholder_buildings={len(w['building_keyholders'])}")
    big = sorted(w["orgs"], key=lambda o: -len(o["occupies"]))[:3]
    for o in big:
        print(f"  big org i={o['i']} sector={o['sector']} ward={o['ward']} occupies={len(o['occupies'])}")

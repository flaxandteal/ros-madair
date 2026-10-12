#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# © Flax & Teal Limited. Aonach Mór resilience demo — building synthesis (stage 04).
#
# Executes the unbuilt building-generation half of RosMadair-ResilienceDemo-Plan.md
# §6.2. Every distribution here is anchored to an AGGREGATE reality statistic
# (GEM taxonomy frequencies, OSM building-use mix, critical-service densities) and
# documented inline — no individual record impersonates a real place. Output
# matches the hand-authored business_data/building.csv seed EXACTLY in format and
# voice: `<Irish type-noun> <ward/feature>` names, real GEM codes with a human
# gloss, a UNIQUE specific notes sentence per building, and attributes that
# correlate (GEM class ↔ height ↔ age ↔ cluster), with critical-service roles
# kept realistically sparse and spread across wards (never clumped).
#
# Deterministic: a single seed makes the city reproducible (plan §9). The notes
# are composed from per-category fragment pools with building-specific specifics
# (storeys from height, era-appropriate phrasing, ward/feature anchoring) so no
# two read copy-paste. Usage:
#   python example/aonach_mor/build/04_buildings.py --count 2400 [--seed 1904] [--out <csv>]
#   python example/aonach_mor/build/04_buildings.py --count 150   # a sniff-test sample

import argparse
import csv
import json
import random
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
WARDS = json.loads((ROOT / "geo/processed/iconic_wards.json").read_text())["wards"]

# ── Reality anchors (aggregate, documented) ──────────────────────────────────
# GEM building-taxonomy classes (real codes) with the same em-dash gloss style as
# the seed. Grouped by the material/era niche they plausibly occupy so a building's
# structure correlates with its age and cluster instead of being drawn at random.
GEM = {
    "rc_frame": [
        ("CR/LFINF — Reinforced concrete, infilled frame", (1960, 2010)),
        ("CR/LFM — Reinforced concrete, moment frame", (1975, 2024)),
        ("CR/LWAL — Reinforced concrete, shear wall", (1980, 2024)),
        ("CR/LPB — Reinforced concrete, precast", (1970, 2010)),
    ],
    "steel": [
        ("S/LFM — Steel, moment frame", (1990, 2024)),
        ("S/LFBR — Steel, braced frame", (1960, 2010)),
    ],
    "masonry": [
        ("MUR+CLBRS — Unreinforced masonry, clay brick", (1900, 1980)),
        ("MCF+CLBRS — Confined masonry, clay brick", (1950, 2000)),
        ("MR+CLBRS — Reinforced masonry, clay brick", (1960, 2010)),
    ],
    "heritage": [
        ("MUR+STDRE — Unreinforced masonry, stone dressed", (1780, 1920)),
        ("MUR+CLBRS — Unreinforced masonry, clay brick", (1850, 1930)),
        ("E/LWAL — Earthen, load-bearing wall", (1750, 1910)),
    ],
    "timber": [
        ("W/LPB — Wood, post and beam", (1880, 1960)),
        ("W/LWAL — Wood, shear wall", (1900, 1975)),
    ],
    "mixed": [
        ("MIX — Mixed / hybrid structural system", (1980, 2024)),
    ],
}

AGE_BANDS = [
    ("pre-1900", 1750, 1899),
    ("1900-1950", 1900, 1950),
    ("1950-1980", 1951, 1980),
    ("1980-2000", 1981, 2000),
    ("post-2000", 2001, 2024),
]


def age_band_for(year):
    for name, lo, hi in AGE_BANDS:
        if lo <= year <= hi:
            return name
    return "post-2000"


# ServiceRole vocabulary (SKOS ServiceRole.xml). Critical services only; most
# buildings carry none (realistic sparsity). Counts are per-CITY targets anchored
# to plausible municipal provision for a ~13-ward coastal city — a handful each,
# spread across wards, NOT scaled with building count.
CRITICAL = {
    "Hospital": 4,
    "Fire station": 5,
    "Police station": 5,
    "Emergency operations centre": 1,
    "Shelter": 9,
    "Muster point": 11,
    "Electrical substation": 7,
    "Water / pumping station": 4,
    "School": 14,
}

# Irish type-noun → (category, role-or-None, [cluster affinities], height range,
# heritage-leaning). The naming grammar the seed established.
TYPES = {
    # critical-service civic
    "Ospidéal": ("rc_frame", "Hospital", ["coastal", "commercial", "riverine"], (18, 34), False),
    "Stáisiún Dóiteáin": ("rc_frame", "Fire station", ["commercial", "riverine", "port"], (9, 16), False),
    "Stáisiún Gardaí": ("masonry", "Police station", ["commercial", "riverine"], (9, 14), False),
    "Ionad Éigeandála": ("steel", "Emergency operations centre", ["commercial"], (14, 22), False),
    "Halla Pobail": ("rc_frame", "Shelter", ["coastal", "residential", "peri-urban"], (6, 14), False),
    "Lóistín": ("timber", "Shelter", ["coastal"], (6, 10), False),
    "Ionad Cruinnithe": ("masonry", "Muster point", ["port", "heritage", "peri-urban"], (6, 12), True),
    "Halla Spóirt": ("mixed", "Muster point", ["peri-urban", "residential"], (8, 12), False),
    "Fo-stáisiún Leictreach": ("rc_frame", "Electrical substation", ["industrial", "peri-urban", "port"], (4, 8), False),
    "Stáisiún Uisce": ("rc_frame", "Water / pumping station", ["port", "peri-urban", "coastal"], (4, 8), False),
    "Scoil": ("masonry", "School", ["residential", "commercial", "coastal"], (7, 14), False),
    "Leabharlann": ("rc_frame", "School", ["commercial", "residential"], (10, 18), False),
    # ordinary fabric (no service role)
    "Árasán": ("rc_frame", None, ["residential", "commercial", "coastal"], (18, 40), False),
    "Teach": ("masonry", None, ["residential", "heritage", "peri-urban"], (3, 8), False),
    "Teach Cré": ("heritage", None, ["peri-urban", "heritage"], (3, 6), True),
    "Teach Oidhreachta": ("heritage", None, ["heritage", "port"], (6, 12), True),
    "Séipéal": ("heritage", None, ["heritage", "residential"], (10, 20), True),
    "Monarcha": ("steel", None, ["industrial", "port"], (10, 18), False),
    "Stóras": ("steel", None, ["port", "industrial"], (8, 16), False),
    "Margadh": ("timber", None, ["residential", "commercial", "peri-urban"], (6, 10), True),
    "Ionad Siopadóireachta": ("rc_frame", None, ["commercial"], (16, 28), False),
    "Tithe Ealaíne": ("masonry", None, ["commercial", "heritage"], (10, 16), True),
    "Óstán": ("rc_frame", None, ["coastal", "commercial"], (20, 36), False),
}

# Per-ward-cluster weighting over ordinary type-nouns, so a port ward fills with
# warehouses, a residential ward with dwellings, a heritage ward with old fabric —
# density and mix vary by ward character (anti-uniform). Anchored to OSM building-
# use frequency by land-use class.
ORDINARY_MIX = {
    "coastal": [("Árasán", 4), ("Teach", 6), ("Óstán", 3), ("Margadh", 2), ("Séipéal", 1)],
    "riverine": [("Teach", 7), ("Árasán", 3), ("Séipéal", 1), ("Tithe Ealaíne", 1)],
    "commercial": [("Ionad Siopadóireachta", 4), ("Árasán", 3), ("Tithe Ealaíne", 2), ("Óstán", 2), ("Teach", 2)],
    "port": [("Stóras", 6), ("Monarcha", 3), ("Teach Oidhreachta", 1), ("Teach", 2)],
    "heritage": [("Teach Oidhreachta", 3), ("Séipéal", 2), ("Teach", 4), ("Tithe Ealaíne", 2), ("Teach Cré", 1)],
    "peri-urban": [("Teach", 5), ("Teach Cré", 3), ("Margadh", 2), ("Monarcha", 1)],
}

# Building counts per ward by cluster character (anti-uniform density). These are
# relative weights; the --count total is apportioned across wards by them.
WARD_DENSITY = {
    "coastal": 1.1, "riverine": 0.9, "commercial": 1.6,
    "port": 1.0, "heritage": 0.8, "peri-urban": 0.7,
}

# Feature qualifiers to distinguish same-type buildings in one ward without
# collision — cardinal/position Irish terms + occasional waterway/quay anchors.
QUALIFIERS = [
    "Thuaidh", "Theas", "Thoir", "Thiar", "Láir", "Uachtarach", "Íochtarach",
    "an Chuain", "na Trá", "an Droichid", "na Cé", "an Mhargaidh", "na hAbhann",
    "Naomh Bréanainn", "Naomh Íde", "an tSeanbhaile", "Ard na Gaoithe",
]

# Locality names for ORDINARY fabric — a building is identified by its street/area,
# never by a bare integer (the dead giveaway of synthesis). Same Irish
# noun+adjective morphology as the renaming layer's `fallback_stems`, so the
# generated localities sit in the same namespace as the geography. Plus a curated
# set of genitive ("X na Y") street names whose Irish grammar is correct.
_LOC_NOUNS = ["Cnoc", "Gleann", "Cuan", "Dún", "Cé", "Ros", "Trá", "Baile", "Cill",
              "Sliabh", "Loch", "Carraig", "Tobar", "Garraí", "Droichead", "Páirc",
              "Faiche", "Rinn", "Mainistir", "Currach"]
_LOC_ADJS = ["Bán", "Mór", "Beag", "Dubh", "Rua", "Gorm", "Úr", "Liath", "Garbh",
             "Mín", "Ard", "Íseal", "Fada", "Crua", "Glas"]
_LOC_STREETS = [
    "Sráid na Cé", "Sráid na Trá", "Sráid Ard", "Sráid na Siopaí", "Lána na gCeannaithe",
    "Slí an Chnoic", "Slí na Cille", "Slí na Trá", "Slí Abhainn Rua", "Bóthar na Trá",
    "Cé na mBád", "Cé Thuaidh", "Cé an Rí", "Páirc an Phobail", "Cearnóg an Mhargaidh",
    "Gáirdín na Mainistreach", "an Caladh", "an tSráid Ard",
]
# ~300+ distinct localities, deterministic order.
LOCALITIES = [f"{n} {a}" for n in _LOC_NOUNS for a in _LOC_ADJS] + _LOC_STREETS


def pick_gem(category, rng):
    code, (lo, hi) = rng.choice(GEM[category])
    year = rng.randint(lo, hi)
    return code, year


def heritage_status(category, year, heritage_leaning, rng):
    if category in ("heritage", "timber") or year < 1920:
        return rng.choices(["Listed", "Candidate", "Non-listed"], [5, 3, 2])[0]
    if heritage_leaning or year < 1960:
        return rng.choices(["Listed", "Candidate", "Non-listed"], [2, 3, 5])[0]
    return rng.choices(["Candidate", "Non-listed"], [1, 9])[0]


# ── Notes: bespoke per building ──────────────────────────────────────────────
# Composed from a function clause (category-specific), a distinguishing detail
# drawn from the building's own attributes (storeys from height, era, a specific
# feature), and an optional status clause. The specifics vary per building so the
# sentences read hand-written, not filled-in.
STOREY_FT = 3.3  # m per storey, for deriving a plausible storey count from height

# Irish street / quay / landmark names to anchor a note to a specific place, so no
# two ordinary buildings read alike even when their core phrase matches.
FEATURES = [
    "Slí an Chnoic", "Cé an Rí", "Droichead na hAbhann", "Slí na Trá", "an tSráid Ard",
    "Cé na mBád", "Slí na Cille", "Páirc an Phobail", "an Caladh", "Sráid na Siopaí",
    "Slí Abhainn Rua", "Fáinne Slí Theas", "Lána na gCeannaithe", "Cearnóg an Mhargaidh",
    "Bóthar na Trá", "Cé Thuaidh", "Sráid na Cé", "Gáirdín na Mainistreach",
]


def storeys(height):
    return max(1, round(height / STOREY_FT))


def st_phrase(st):
    return f"{st} storey" if st == 1 else f"{st} storeys"


def st_adj(st):
    # hyphenated singular adjective form — "7-storey hospital", NOT "7 storeys hospital"
    return f"{st}-storey"


def era_phrase(year):
    if year < 1900:
        return f"{year // 100 + 1}th-century"
    return f"{(year // 10) * 10}s"


def notes_for(type_noun, role, category, year, height, ward_label, rng):
    st = storeys(height)
    era = era_phrase(year)
    decade = f"{(year // 10) * 10}s"
    feat = rng.choice(FEATURES)

    # Role-led buildings get operational specifics.
    if role == "Hospital":
        beds = rng.choice([80, 120, 160, 210, 240, 320])
        return rng.choice([
            f"{st_adj(st).capitalize()} general hospital serving {ward_label} and the adjoining wards, {beds} beds.",
            f"District hospital off {feat} with an emergency department and {beds} inpatient beds.",
            f"Coastal referral hospital, {st_phrase(st)}, {beds} beds and a helipad for evacuations.",
            f"Maternity and general hospital, {beds} beds, rebuilt in the {decade}.",
        ])
    if role == "Fire station":
        bays = rng.choice([2, 3, 4])
        return rng.choice([
            f"Fire station with {bays} appliance bays covering the {ward_label} approaches.",
            f"{bays}-bay fire and rescue station on {feat}, retained crew after dark.",
            f"Fire station and drill yard, {bays} bays, serving the harbour side.",
        ])
    if role == "Police station":
        return rng.choice([
            f"District police station for the {ward_label} sector.",
            f"Divisional police headquarters on {feat} with a public enquiry desk.",
            "Community police station with two holding cells.",
        ])
    if role == "Emergency operations centre":
        return f"City-wide emergency coordination centre on {feat} with a hardened communications room."
    if role == "Shelter":
        cap = rng.choice([120, 180, 250, 400])
        return rng.choice([
            f"{era.capitalize()} assembly hall designated as an emergency shelter, capacity ~{cap}.",
            f"Seafront hall near {feat}, a secondary shelter during surge warnings.",
            f"Community hall used as a muster-and-shelter point, capacity ~{cap}.",
            f"Parish hall off {feat}, stocked as a cyclone shelter, capacity ~{cap}.",
        ])
    if role == "Muster point":
        return rng.choice([
            f"Covered assembly space designated as a ward muster point for {ward_label}.",
            f"Sports and events hall by {feat}, doubling as a muster point.",
            "Open-sided community pavilion used as a muster point.",
        ])
    if role == "Electrical substation":
        kv = rng.choice([11, 22, 33])
        return rng.choice([
            f"{kv} kV distribution substation feeding the {ward_label} network.",
            f"{kv} kV substation and switchroom off {feat}.",
        ])
    if role == "Water / pumping station":
        return rng.choice([
            "Raw-water intake and pumping station.",
            f"Stormwater pumping station protecting the low-lying {ward_label} streets.",
            f"Sewage lift station beside {feat}.",
        ])
    if role == "School":
        kind = rng.choice(["Primary", "Secondary", "Primary", "Technical"])
        return rng.choice([
            f"{kind} school in {ward_label} ward, {st_phrase(st)}.",
            f"{kind} school off {feat}, built in the {decade}.",
            f"Public library doubling as a school resource centre, {st_phrase(st)}.",
        ])

    # Ordinary fabric: a varied CORE phrase + an optional provenance clause that
    # injects building-specific detail (year, street, decade, condition), so the
    # sentences read hand-written rather than filled from one template.
    cores = {
        "Árasán": ["residential tower block", "apartment block with ground-floor retail",
                   "council housing block", "mixed-tenure apartment building",
                   "sea-view apartment tower", "walk-up flats over shops"],
        "Teach": ["terraced townhouse", "two-family dwelling", "semi-detached house",
                  "artisan's cottage", "merchant's townhouse", "courtyard house",
                  "row of fishers' cottages"],
        "Teach Cré": ["earthen-walled cottage", "mud-and-thatch dwelling",
                      "traditional cob house", "lime-rendered earthen house"],
        "Teach Oidhreachta": ["stone townhouse, now a museum", "preserved merchant's house",
                              "heritage house with period interiors"],
        "Séipéal": ["church with a bell tower", "parish church with a timber roof",
                    "chapel with a lychgate", "colonial-era church"],
        "Monarcha": ["textile manufacturing warehouse", "light-engineering works",
                     "food-processing plant", "garment workshop"],
        "Stóras": ["quayside bonded warehouse", "transit shed on the wharf",
                   "cold store", "grain warehouse"],
        "Margadh": ["timber market hall", "covered produce market",
                    "fish market with an open colonnade", "weekly market shambles"],
        "Ionad Siopadóireachta": ["shopping centre", "retail arcade", "department store"],
        "Tithe Ealaíne": ["art gallery and civic events space",
                          "exhibition rooms over a reading library", "artists' studios and gallery"],
        "Óstán": ["seafront hotel", "business hotel over a conference floor",
                  "boutique harbour hotel"],
    }
    core = rng.choice(cores.get(type_noun, [type_noun.lower()]))

    # Lead taller ordinary fabric with concrete scale ("8-storey …"); keep low
    # fabric terse. Note the ADJECTIVE form (hyphen, singular), not "8 storeys …".
    tall = st >= 4 and type_noun in ("Árasán", "Ionad Siopadóireachta", "Óstán", "Monarcha")
    sentence = f"{st}-storey {core}" if tall else core
    sentence = sentence[0].upper() + sentence[1:]

    # Provenance clause, drawn from a pool that FITS the fabric (a warehouse isn't
    # "converted to flats"; a museum isn't). Injects a per-building specific.
    FABRIC = {
        "Árasán": "dwelling", "Teach": "dwelling",
        "Teach Cré": "heritage", "Teach Oidhreachta": "heritage", "Séipéal": "heritage",
        "Monarcha": "industrial", "Stóras": "industrial",
        "Margadh": "civic", "Ionad Siopadóireachta": "civic",
        "Tithe Ealaíne": "civic", "Óstán": "civic",
    }
    # One terse "" per pool keeps the seed's occasional short note without letting
    # any single bare description dominate the file.
    prov_pools = {
        "dwelling": [f" on {feat}", f" off {feat}", f" at {feat}", f", rebuilt {year}",
                     f", converted to flats in the {decade}", ", with a walled yard",
                     f", refronted in the {decade}", f", extended {year}", ""],
        "heritage": [f" on {feat}", f" by {feat}", ", under conservation review",
                     ", reroofed after storm damage", f", restored in the {decade}",
                     f", {era} fabric", ""],
        "industrial": [f" on {feat}", f" beside {feat}", f", re-clad in the {decade}",
                       ", part-vacant", ", rail siding to the rear", f", re-roofed {year}", ""],
        "civic": [f" on {feat}", f" off {feat}", f", extended in the {decade}",
                  f", refurbished {year}", f" near {feat}", ""],
    }
    provenance = rng.choice(prov_pools[FABRIC.get(type_noun, "dwelling")])
    return f"{sentence}{provenance}."


def apportion(count):
    """Spread `count` buildings across wards weighted by cluster density."""
    weights = [WARD_DENSITY[w["cluster"]] for w in WARDS]
    total = sum(weights)
    return [max(1, round(count * w / total)) for w in weights]


def generate(count, seed):
    rng = random.Random(seed)
    rows = []
    used_names = set()
    idx = 0

    def emit(type_noun, role, ward, force_year=None):
        nonlocal idx
        idx += 1
        category, _, _, hrange, heritage_leaning = TYPES[type_noun]
        gem, year = pick_gem(category, rng)
        if force_year is not None:
            year = force_year
        lo, hi = hrange
        height = rng.randint(lo, hi)
        # keep heritage low and modern towers tall — correlate height with era
        if year < 1920:
            height = min(height, rng.randint(6, 16))
        age = age_band_for(year)
        hs = heritage_status(category, year, heritage_leaning, rng)
        ward_label = ward["aonach_mor_label"]
        # Naming: a named/critical building is anchored to its WARD (so the demo's
        # "hospital in ward X" reads); ordinary fabric is named by its LOCALITY /
        # street. Collisions are resolved with another locality or an Irish
        # qualifier — NEVER a bare integer suffix.
        if role is not None:
            name = f"{type_noun} {ward_label}"
            if name in used_names:
                for q in rng.sample(QUALIFIERS, len(QUALIFIERS)):
                    cand = f"{type_noun} {q}"
                    if cand not in used_names:
                        name = cand
                        break
        else:
            name = None
            for loc in rng.sample(LOCALITIES, len(LOCALITIES)):
                cand = f"{type_noun} {loc}"
                if cand not in used_names:
                    name = cand
                    break
            if name is None:  # pool exhausted for this type — add a qualifier
                loc = rng.choice(LOCALITIES)
                q = rng.choice(QUALIFIERS)
                name = f"{type_noun} {loc} {q}"
        used_names.add(name)
        # map ward cluster → building ClusterType vocabulary
        ctype = {
            "coastal": "Coastal", "riverine": "Residential", "commercial": "Central business district",
            "port": "Port", "heritage": "Heritage", "peri-urban": "Peri-urban",
        }[ward["cluster"]]
        if role in ("Electrical substation", "Water / pumping station") or type_noun in ("Monarcha", "Stóras"):
            ctype = "Industrial"
        note = notes_for(type_noun, role, category, year, height, ward_label, rng)
        rows.append({
            "ResourceID": f"bldg-{idx:05d}",
            "name": name,
            "gem_taxonomy": gem,
            "heritage_status": hs,
            "service_roles": role or "",
            "cluster_type": ctype,
            "height_m": height,
            "age_band_years": age,
            "notes": note,
        })

    # 1) Critical-service buildings first — fixed small counts, spread across wards
    #    whose cluster suits each role (never clumped in one ward).
    for role, n in CRITICAL.items():
        candidates = [t for t, v in TYPES.items() if v[1] == role]
        placed_wards = []
        for _ in range(n):
            type_noun = rng.choice(candidates)
            affinities = TYPES[type_noun][2]
            pool = [w for w in WARDS if w["cluster"] in affinities] or WARDS
            # avoid repeating the same ward for a role until all suitable wards used
            fresh = [w for w in pool if w["key"] not in placed_wards] or pool
            ward = rng.choice(fresh)
            placed_wards.append(ward["key"])
            emit(type_noun, role, ward)

    # 2) Ordinary fabric fills the remainder, apportioned by ward density + mix.
    remaining = max(0, count - len(rows))
    per_ward = apportion(remaining)
    for ward, n in zip(WARDS, per_ward):
        mix = ORDINARY_MIX[ward["cluster"]]
        nouns = [t for t, _ in mix]
        weights = [w for _, w in mix]
        for _ in range(n):
            type_noun = rng.choices(nouns, weights)[0]
            emit(type_noun, None, ward)

    rng.shuffle(rows)  # don't leave criticals clustered at the top of the file
    # renumber after shuffle so ids are still stable/ordered on disk
    for i, r in enumerate(rows, 1):
        r["ResourceID"] = f"bldg-{i:05d}"
    return rows


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--count", type=int, default=2400)
    ap.add_argument("--seed", type=int, default=1904)
    ap.add_argument("--out", type=Path, default=ROOT / "business_data/building.csv")
    ap.add_argument("--stdout", action="store_true", help="write a sample to stdout instead of the file")
    args = ap.parse_args()

    rows = generate(args.count, args.seed)
    fields = ["ResourceID", "name", "gem_taxonomy", "heritage_status",
              "service_roles", "cluster_type", "height_m", "age_band_years", "notes"]
    if args.stdout:
        w = csv.DictWriter(sys.stdout, fieldnames=fields)
        w.writeheader()
        for r in rows:
            w.writerow(r)
    else:
        with args.out.open("w", newline="") as f:
            w = csv.DictWriter(f, fieldnames=fields)
            w.writeheader()
            for r in rows:
                w.writerow(r)
        print(f"wrote {len(rows)} buildings to {args.out}", file=sys.stderr)


if __name__ == "__main__":
    main()

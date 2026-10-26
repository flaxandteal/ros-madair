#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# © Flax & Teal Limited. Aonach Mór — organisation synthesis for the REAL
# ros-madair-emit path (companion to emit_buildings.py / emit_persons.py). Emits
# Organisation (CIDOC E74_Group) resource-JSON bundles so the demo's org/edge
# queries ("which orgs are registered in a ward with a hospital", "who insures
# what") run on the real melt substrate. Reuses emit_buildings.py's Site ids
# (site_rid / strip_ward / SITE_GRAPHID) so organisation.registered_site and
# building.site reference the SAME wards.
#
# Orgs are a FIXED corpus (~220), decoupled from the ×N building replica
# multiplier — a city has a fixed roster of registered organisations however big
# the housing stock is (same reasoning as emit_buildings' CRITICAL_FIXED).
# Deterministic (seeded random.Random).
#
#   python example/aonach_mor/build/emit_organisations.py --count 220 --out <data_dir>
# writes <data_dir>/graphs/<ORG_GRAPHID>.json and
# <data_dir>/resources/organisation/organisations.json
# (and refreshes example/aonach_mor/graphs/resource_models/organisation/graph.json).

import argparse
import csv
import importlib.util
import json
import random
import uuid
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent


def _load(mod, fname):
    spec = importlib.util.spec_from_file_location(mod, HERE / fname)
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


# site_rid, SITE_GRAPHID, strip_ward, DOCS, LOCALITIES — shared with the building emit
eb = _load("emit_buildings", "emit_buildings.py")

NS = uuid.NAMESPACE_URL
ORG_GRAPHID = str(uuid.uuid5(NS, "aonach-mor/resource_models/organisation"))
MODEL_DIR = ROOT / "graphs/resource_models/organisation"


def nid(alias):
    return str(uuid.uuid5(uuid.UUID(ORG_GRAPHID), alias))


# ── 1. Organisation graph JSON (same nodes.csv-driven scheme as
#    emit_buildings.build_graph: top_ancestor nodegroups, edges from
#    parent_property, concept-list cardinality "n"). ─────────────────────────────
def build_graph():
    rows = list(csv.DictReader((MODEL_DIR / "nodes.csv").open()))
    by_alias = {r["alias"]: r for r in rows}

    def top_ancestor(alias):
        r = by_alias[alias]
        while r["parent_alias"]:
            r = by_alias[r["parent_alias"]]
        return r["alias"]

    root_alias = "organisation"
    root = {"nodeid": nid(root_alias), "name": "Organisation", "alias": root_alias,
            "datatype": "semantic", "graph_id": ORG_GRAPHID, "istopnode": True,
            "is_collector": False, "isrequired": False, "issearchable": True,
            "exportable": False, "sortorder": 0,
            "ontologyclass": "http://www.cidoc-crm.org/cidoc-crm/E74_Group"}
    nodes = [root]
    edges = []
    nodegroups = {}
    for r in rows:
        alias = r["alias"]
        ng_alias = top_ancestor(alias)
        is_top = not r["parent_alias"]
        nodes.append({
            "nodeid": nid(alias), "name": r["name"], "alias": alias,
            "datatype": r["datatype"], "graph_id": ORG_GRAPHID, "istopnode": False,
            "is_collector": is_top, "isrequired": r["required"] == "true",
            "issearchable": r["searchable"] == "true", "exportable": r["exportable"] == "true",
            "sortorder": int(r["sortorder"]), "ontologyclass": r["ontology_class"],
            "nodegroup_id": nid(ng_alias),
        })
        parent = r["parent_alias"] or root_alias
        edges.append({"edgeid": str(uuid.uuid5(uuid.UUID(ORG_GRAPHID), f"edge:{alias}")),
                      "domainnode_id": nid(parent), "rangenode_id": nid(alias),
                      "graph_id": ORG_GRAPHID, "ontologyproperty": r["parent_property"]})
        if is_top:
            card = "n" if r["datatype"].endswith("-list") else "1"
            nodegroups[ng_alias] = {"nodegroupid": nid(ng_alias), "cardinality": card,
                                    "parentnodegroup_id": None}
    return {"graph": [{"graphid": ORG_GRAPHID, "name": "Organisation", "root": root,
                       "nodes": nodes, "nodegroups": list(nodegroups.values()), "edges": edges}]}


# ── 2. concept labels — emit the FULL collection label so it matches the vocab
#    (the emitter melts concept values with a raw-key fallback, so this resolves). ──
def collection(name):
    out = []
    for r in csv.DictReader((MODEL_DIR / "collections.csv").open()):
        if r["collection_name"] == name:
            out.append(r["concept_label"])
    return out


# ── 3. believable Irish/English organisation names ──────────────────────────────
# <geographic/eponym> <sector descriptor> <company form>. Geographic bases reuse
# the building emit's Irish locality grammar so the org corpus shares the namespace.
FORMS = ["Teoranta", "Teo.", "CTR", "Comhlacht", "Grúpa", "Ltd", "Group",
         "Holdings", "Partners", "Services", "plc", "Cuideachta"]
EPONYMS = ["Ó Braonáin", "Mac Cárthaigh", "Ní Dhálaigh", "Ó Súilleabháin", "De Barra",
           "Mac an Bhaird", "Ó Ceallaigh", "Ní Mhurchú", "Ó Riain", "Mac Gabhann",
           "Ó Dochartaigh", "Breathnach", "Ó Floinn", "Nic Aodha", "Ó Tuama"]
# ISIC sector letter → descriptor nouns that read true for that sector.
SECTOR_WORDS = {
    "A": ["Iascaigh", "Feirmeoireachta", "Talmhaíochta", "Agri", "Foraoiseachta"],
    "B": ["Mianadóireachta", "Cairéil", "Minerals", "Aggregates"],
    "C": ["Déantúsaíochta", "Monaróireachta", "Manufacturing", "Textiles", "Foundry"],
    "D": ["Fuinnimh", "Cumhachta", "Energy", "Power", "Grid"],
    "E": ["Uisce", "Water", "Dramhaíola", "Sanitation", "Utilities"],
    "F": ["Tógála", "Foirgníochta", "Construction", "Civil Works", "Builders"],
    "G": ["Miondíola", "Trádála", "Retail", "Wholesale", "Margadh"],
    "H": ["Iompair", "Lóistíochta", "Transport", "Logistics", "Freight"],
    "I": ["Óstáin", "Fáilteachais", "Hospitality", "Lóistín", "Bialann"],
    "J": ["Teicneolaíochta", "Cumarsáide", "Digital", "Media", "Telecom"],
    "K": ["Árachais", "Airgeadais", "Insurance", "Assurance", "Capital", "Mutual"],
    "L": ["Eastáit", "Maoine", "Property", "Estates", "Realty"],
    "M": ["Comhairleach", "Innealtóireachta", "Consulting", "Advisory", "Associates"],
    "N": ["Riaracháin", "Seirbhísí", "Facilities", "Support", "Bureau"],
    "O": ["Comhairle", "Údarás", "Council", "Authority", "Agency"],
    "P": ["Oideachais", "Coláiste", "Academy", "Education Trust", "Institiúid"],
    "Q": ["Sláinte", "Cúraim", "Health", "Clinic", "Care", "Ospidéal"],
    "R": ["Ealaíon", "Cultúir", "Arts", "Heritage", "Oidhreachta"],
    "S": ["Seirbhíse", "Services", "Guild", "Comhar"],
}


def sector_letter(label):
    return label.split(" ", 1)[0]


def org_name(sector_label, rng):
    letter = sector_letter(sector_label)
    words = SECTOR_WORDS.get(letter, ["Services"])
    base = (rng.choice(eb.LOCALITIES) if rng.random() < 0.62 else rng.choice(EPONYMS))
    word = rng.choice(words)
    form = rng.choice(FORMS)
    # a few orderings so it does not read templated
    roll = rng.random()
    if roll < 0.5:
        return f"{base} {word} {form}"
    elif roll < 0.8:
        return f"{word} {base} {form}"
    else:
        return f"{base} {form}"


# ── 4. sector → the critical-service roles a sector plausibly operates. Most orgs
#    carry NO service role; a public-admin / utility / health / education org may. ──
SECTOR_ROLES = {
    "D": ["Electrical substation"],
    "E": ["Water / pumping station"],
    "O": ["Emergency operations centre", "Fire station", "Police station", "Muster point"],
    "P": ["School"],
    "Q": ["Hospital"],
}
# A critical-service operator should actually appear — bias the sector draw so the
# demo has a handful of each. Remaining mass spreads across the ordinary economy.
SECTOR_WEIGHTS = {
    "A": 3, "B": 1, "C": 10, "D": 4, "E": 4, "F": 12, "G": 16, "H": 8,
    "I": 10, "J": 7, "K": 8, "L": 7, "M": 10, "N": 7, "O": 6, "P": 6,
    "Q": 6, "R": 4, "S": 5,
}
SIZE_WEIGHTS = {  # micro → very large, a believable long tail
    "Micro (1–9)": 42, "Small (10–49)": 30, "Medium (50–249)": 18,
    "Large (250–999)": 7, "Very large (1000+)": 3,
}


# The coherent world (world.py) is the shared occupancy/employment assignment. It
# drives WHICH buildings each org occupies + its ward + identity; sector/size/name
# stay believable. Map the world's plain sector to the model's ISIC letter so the
# existing naming + service-role logic still apply.
W = _load("world", "world.py")
WORLD_SECTOR_LETTER = {
    "Health": "Q", "Emergency management": "O", "Public safety": "O",
    "Utilities": "E", "Education": "P", "Industrial": "C",
    "Commercial": "G", "Financial and insurance": "K",
}


def generate_from_world(world, seed):
    """Serialise the coherent world's organisations: identity, ward and the NEW
    `occupies` link come from the world (so the org→building web is real and
    coherent); sector/size/name remain believable."""
    rng = random.Random(seed ^ 0x0C)
    by_letter = {sector_letter(s): s for s in collection("Sector")}
    resources, insurers, used = [], [], set()
    for o in world["orgs"]:
        i, rid = o["i"], o["rid"]
        letter = WORLD_SECTOR_LETTER.get(o["sector"], "G")
        sector = by_letter.get(letter, o["sector"])
        size, ward = o["size_band"], o["ward"]
        for _ in range(8):
            name = org_name(sector, rng)
            if name not in used:
                break
        used.add(name)
        legal_id = f"AM-{letter}-{rng.randint(10000, 99999)}"
        note = (f"{size.split(' (')[0]} {sector.split(' — ')[-1].lower()} organisation "
                f"in {ward}; occupies {len(o['occupies'])} building(s).")
        site_id = eb.site_rid(ward)

        def tile(ng_alias, data, tag, _i=i, _rid=rid):
            return {"tileid": str(uuid.uuid5(uuid.UUID(ORG_GRAPHID), f"tile:{_i}:{tag}")),
                    "nodegroup_id": nid(ng_alias), "parenttile_id": None,
                    "resourceinstance_id": _rid, "sortorder": 0, "provisionaledits": None,
                    "data": data}

        tiles = [
            tile("name", {nid("name"): name}, "name"),
            tile("legal_identifier", {nid("legal_identifier"): legal_id}, "lei"),
            tile("sector", {nid("sector"): sector}, "sec"),
            tile("size_band", {nid("size_band"): size}, "size"),
            tile("registered_location", {nid("registered_site"): [{"resourceId": site_id}]}, "site"),
            tile("notes", {nid("notes"): note}, "note"),
        ]
        # Service roles: the facility(ies) this org operates (coherent with occupancy).
        roles = sorted({b[1] for b in o["_fac"] if b[1]})
        if roles:
            tiles.append(tile("service_roles", {nid("service_roles"): roles}, "role"))
        # THE NEW occupancy link — org → the buildings it occupies (its own nodegroup).
        if o["occupies"]:
            tiles.append(tile("occupies",
                              {nid("occupies"): [{"resourceId": b} for b in o["occupies"]]}, "occ"))
        # org_type (E55 legal form, orthogonal to ISIC sector): emergency services, utilities,
        # government, and a believable NGO/charity share in health/social/education/other —
        # so "relief NGOs" resolves to real NGO/charity orgs (many of which operate shelters).
        _emerg = {"Fire station", "Police station", "Emergency operations centre"}
        if roles and set(roles) & _emerg:
            org_type = "Emergency service"
        elif letter in ("D", "E") or (roles and "Water / pumping station" in roles):
            org_type = "Utility"
        elif letter == "O":
            org_type = "Government body"
        elif letter in ("Q", "S", "P"):
            org_type = rng.choices(["NGO / charity", "Government body", "Private company"], [5, 2, 3])[0]
        else:
            org_type = rng.choices(["Private company", "NGO / charity", "Community group"], [7, 2, 1])[0]
        tiles.append(tile("org_type", {nid("org_type"): org_type}, "otype"))

        resources.append({
            "resourceinstance": {"resourceinstanceid": rid, "graph_id": ORG_GRAPHID, "name": name,
                "legacyid": f"org-{i:05d}",
                "descriptors": {"en": {"name": name, "description": note, "map_popup": ""}}},
            "tiles": tiles})
        if letter == "K":
            insurers.append((i, rid, name))
    return resources, insurers


def load_wards(seed):
    """The site wards emit_buildings actually emits: footprint `ward` stripped of its
    trailing cell number (exactly site_resources' key). Weighted by footprint count so
    busier wards host more organisations."""
    import collections
    fp = list(csv.DictReader((eb.DOCS / "scripts/seed/footprints.csv").open()))
    c = collections.Counter(eb.strip_ward(f["ward"]) for f in fp)
    wards = sorted(c)
    weights = [c[w] for w in wards]
    return wards, weights


def main():
    ap = argparse.ArgumentParser()
    # Org count now DERIVES from the coherent world's occupancy (it is not a free
    # knob), so we mirror the building footprint slice exactly to recover the same
    # building rids the building emit wrote.
    ap.add_argument("--footprints-count", type=int, default=0, help="mirror emit_buildings --count (shuffled slice, replicas=1)")
    ap.add_argument("--footprints", type=int, default=0, help="mirror emit_buildings --footprints (head slice)")
    ap.add_argument("--replicas", type=int, default=1, help="mirror emit_buildings --replicas (pass 23 for full scale)")
    ap.add_argument("--seed", type=int, default=1904)
    ap.add_argument("--out", type=Path, required=True, help="emit data_dir to populate")
    args = ap.parse_args()

    graph = build_graph()
    (MODEL_DIR / "graph.json").write_text(json.dumps(graph, indent=1, ensure_ascii=False))

    fp = list(csv.DictReader((eb.DOCS / "scripts/seed/footprints.csv").open()))
    replicas = args.replicas
    if args.footprints_count:
        r = random.Random(args.seed)
        r.shuffle(fp)
        fp = fp[:args.footprints_count]
        replicas = 1
    elif args.footprints:
        fp = fp[:args.footprints]
    world = W.build_world(fp, replicas, args.seed)
    resources, insurers = generate_from_world(world, args.seed)

    gdir = args.out / "graphs"
    rdir = args.out / "resources" / "organisation"
    gdir.mkdir(parents=True, exist_ok=True)
    rdir.mkdir(parents=True, exist_ok=True)
    (gdir / f"{ORG_GRAPHID}.json").write_text(json.dumps(graph, ensure_ascii=False))
    (rdir / "organisations.json").write_text(
        json.dumps({"business_data": {"resources": resources}}, ensure_ascii=False))
    occ = sum(len(o["occupies"]) for o in world["orgs"])
    print(f"org_graphid={ORG_GRAPHID}")
    print(f"wrote {len(resources)} organisations ({len(insurers)} insurers); "
          f"{occ} occupancy links across {world['n_occupiable']} occupiable buildings")


if __name__ == "__main__":
    main()

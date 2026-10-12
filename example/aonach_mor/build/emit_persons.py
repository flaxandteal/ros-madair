#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# © Flax & Teal Limited. Aonach Mór — person synthesis for the REAL ros-madair-emit
# path (companion to emit_buildings.py). Emits Person resource-JSON bundles so the
# demo's "doctors in wards that have a hospital" query runs on the real melt
# substrate. Reuses 06_persons.py's believable Irish-name roster + role mix, and
# emit_buildings.py's Site ids (site_rid) so person.primary_site and building.site
# reference the SAME wards. Clinical roles (Doctor/Nurse/Paramedic) are placed in
# wards that actually received a Hospital building, so the headline query returns a
# sensible set. Deterministic.
#
#   python example/aonach_mor/build/emit_persons.py --count 3000 --footprints <N> --out <data_dir>

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


p06 = _load("p06", "06_persons.py")          # GIVEN/SURNAME/ROLES/contact_handle/notes_for
eb = _load("emit_buildings", "emit_buildings.py")  # site_rid, SITE_GRAPHID, strip_ward, building_stream

PERSON_GRAPHID = str(uuid.uuid5(uuid.NAMESPACE_URL, "aonach-mor/resource_models/person"))
MODEL_DIR = ROOT / "graphs/resource_models/person"
CLINICAL = {"Doctor", "Nurse", "Paramedic"}


def pnid(alias):
    return str(uuid.uuid5(uuid.UUID(PERSON_GRAPHID), alias))


def build_person_graph():
    """Person model CSV → Arches graph JSON, same scheme as emit_buildings.build_graph
    (deterministic uuid5 node ids; role_titles is the PersonRole concept-list)."""
    rows = list(csv.DictReader((MODEL_DIR / "nodes.csv").open()))
    P = "http://www.cidoc-crm.org/cidoc-crm/"
    root = {"nodeid": pnid("person"), "name": "Person", "alias": "person", "datatype": "semantic",
            "graph_id": PERSON_GRAPHID, "istopnode": True, "is_collector": False, "isrequired": False,
            "issearchable": True, "exportable": False, "sortorder": 0, "ontologyclass": P + "E21_Person"}
    nodes, edges, nodegroups = [root], [], []
    for r in rows:
        alias = r["alias"]
        nodes.append({"nodeid": pnid(alias), "name": r["name"], "alias": alias, "datatype": r["datatype"],
                      "graph_id": PERSON_GRAPHID, "istopnode": False, "is_collector": True,
                      "isrequired": r["required"] == "true", "issearchable": r["searchable"] == "true",
                      "exportable": r["exportable"] == "true", "sortorder": int(r["sortorder"]),
                      "ontologyclass": r["ontology_class"], "nodegroup_id": pnid(alias)})
        edges.append({"edgeid": str(uuid.uuid5(uuid.UUID(PERSON_GRAPHID), "e:" + alias)),
                      "domainnode_id": pnid("person"), "rangenode_id": pnid(alias),
                      "graph_id": PERSON_GRAPHID, "ontologyproperty": r["parent_property"]})
        nodegroups.append({"nodegroupid": pnid(alias),
                           "cardinality": "n" if r["datatype"].endswith("-list") else "1",
                           "parentnodegroup_id": None})
    return {"graph": [{"graphid": PERSON_GRAPHID, "name": "Person", "root": root,
                       "nodes": nodes, "nodegroups": nodegroups, "edges": edges}]}


def hospital_wards(fp, seed):
    """The wards that actually got a Hospital building — building_stream places the
    fixed criticals first, so replicas=0 yields them cheaply with the same rng/seed
    as the real building emit (criticals precede the bulk loop)."""
    return sorted({s["ward"] for _, s in eb.building_stream(fp, 0, seed) if s["role"] == "Hospital"})


def generate(count, fp, seed):
    rng = random.Random(seed)
    hosp = hospital_wards(fp, seed)
    all_wards = sorted({eb.strip_ward(f["ward"]) for f in fp})
    roles = [r[0] for r in p06.ROLES]
    weights = [r[1] for r in p06.ROLES]
    title_of = {r[0]: r[3] for r in p06.ROLES}
    used = set()
    resources = []
    for i in range(1, count + 1):
        role = rng.choices(roles, weights)[0]
        # Clinical staff are stationed in a ward that has a hospital (so "doctors in
        # wards with a hospital" is meaningful); everyone else across all wards.
        ward = rng.choice(hosp) if (role in CLINICAL and hosp) else rng.choice(all_wards)
        for _ in range(6):
            name = f"{rng.choice(p06.GIVEN)} {rng.choice(p06.SURNAME)}"
            if name not in used:
                break
        used.add(name)
        role_list = [role]
        if rng.random() < 0.08:
            second = rng.choices(roles, weights)[0]
            if second != role:
                role_list.append(second)
        contact = p06.contact_handle(role, name, rng)
        note = p06.notes_for(role, title_of[role], ward, rng)
        site_id = eb.site_rid(ward)
        rid = str(uuid.uuid5(uuid.UUID(PERSON_GRAPHID), f"person:{i}"))

        def tile(alias, data, tag):
            return {"tileid": str(uuid.uuid5(uuid.UUID(PERSON_GRAPHID), f"tile:{i}:{tag}")),
                    "nodegroup_id": pnid(alias), "parenttile_id": None, "resourceinstance_id": rid,
                    "sortorder": 0, "provisionaledits": None, "data": data}
        tiles = [
            tile("name", {pnid("name"): name}, "name"),
            tile("role_titles", {pnid("role_titles"): role_list}, "role"),
            tile("contact_handle", {pnid("contact_handle"): contact}, "ct"),
            tile("primary_site", {pnid("primary_site"): [{"resourceId": site_id}]}, "site"),
        ]
        if note:
            tiles.append(tile("notes", {pnid("notes"): note}, "note"))
        resources.append({
            "resourceinstance": {"resourceinstanceid": rid, "graph_id": PERSON_GRAPHID, "name": name,
                "legacyid": f"pers-{i:06d}",
                "descriptors": {"en": {"name": name, "description": note, "map_popup": ""}}},
            "tiles": tiles})
    return resources, hosp


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--persons", type=int, default=3000)
    # Reproduce emit_buildings' footprint slice EXACTLY so hospital_wards matches the
    # buildings actually emitted: --footprints-count mirrors its --count (shuffle+slice
    # on the same seed); --footprints mirrors its --footprints (head slice).
    ap.add_argument("--footprints-count", type=int, default=0)
    ap.add_argument("--footprints", type=int, default=0)
    ap.add_argument("--seed", type=int, default=1904)
    ap.add_argument("--out", type=Path, required=True)
    args = ap.parse_args()

    fp = list(csv.DictReader((eb.DOCS / "scripts/seed/footprints.csv").open()))
    if args.footprints_count:
        rng = random.Random(args.seed)
        rng.shuffle(fp)
        fp = fp[:args.footprints_count]
    elif args.footprints:
        fp = fp[:args.footprints]

    gdir, rdir = args.out / "graphs", args.out / "resources" / "person"
    gdir.mkdir(parents=True, exist_ok=True)
    rdir.mkdir(parents=True, exist_ok=True)
    (gdir / f"{PERSON_GRAPHID}.json").write_text(json.dumps(build_person_graph(), ensure_ascii=False))
    resources, hosp = generate(args.persons, fp, args.seed)
    (rdir / "persons.json").write_text(json.dumps({"business_data": {"resources": resources}}, ensure_ascii=False))
    docs = sum(1 for r in resources if "Doctor" in r["tiles"][1]["data"][pnid("role_titles")])
    print(f"person_graphid={PERSON_GRAPHID}")
    print(f"wrote {len(resources)} persons ({docs} doctors) across {len(hosp)} hospital wards: {hosp}")


if __name__ == "__main__":
    main()

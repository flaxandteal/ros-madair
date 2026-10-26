#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# © Flax & Teal Limited. Aonach Mór — person synthesis for the REAL ros-madair-emit
# path (companion to emit_buildings.py / emit_organisations.py). Emits Person
# resource-JSON bundles so the demo's "doctors in wards that have a hospital" query
# runs on the real melt substrate. Reuses 06_persons.py's believable Irish-name
# roster + role mix, and emit_buildings.py's Site ids (site_rid) so
# person.primary_site and building.site reference the SAME wards.
#
# Persons are now SERIALISED FROM THE SHARED COHERENT WORLD (world.py): the world
# owns who is employed by which org, which ward they are stationed in, whether they
# are clinical, and which buildings they keyhold (a keyholder is employed by the org
# that occupies that building). This generator no longer synthesises any of those
# links independently — it reads world["persons"] and renders each person's tiles,
# so Person ──employer──▶ Organisation ──occupies──▶ Building chains exist BY
# CONSTRUCTION. Deterministic.
#
#   python example/aonach_mor/build/emit_persons.py --footprints-count <N> --out <data_dir>

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
W = _load("world", "world.py")                # the shared coherent occupancy/employment world

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


def generate_from_world(world, seed):
    """Serialise the coherent world's persons: identity, employer, stationed ward,
    clinical status and keyholding all come from the world (so the person→org→building
    web is real and coherent); name/role title/contact/notes remain believable. No
    link is re-derived here — world.py owns the relational assignment."""
    rng = random.Random(seed ^ 0x7E)
    roles = [r[0] for r in p06.ROLES]
    weights = [r[1] for r in p06.ROLES]
    title_of = {r[0]: r[3] for r in p06.ROLES}
    clinical_roles = [r for r in roles if r in CLINICAL]
    civ_roles = [r for r in roles if r not in CLINICAL]
    civ_weights = [w for r, w in zip(roles, weights) if r not in CLINICAL]
    used = set()
    resources = []
    for person in world["persons"]:
        i, rid, ward = person["i"], person["rid"], person["ward"]
        # Role: clinical staff get a clinical title (Doctor/Nurse/Paramedic); everyone
        # else a plausible civilian role. is_clinical is the world's call, not ours.
        if person["is_clinical"] and clinical_roles:
            role = rng.choice(clinical_roles)
        else:
            role = rng.choices(civ_roles, civ_weights)[0]
        for _ in range(6):
            name = f"{rng.choice(p06.GIVEN)} {rng.choice(p06.SURNAME)}"
            if name not in used:
                break
        used.add(name)
        role_list = [role]
        if rng.random() < 0.08:  # a small chance of a second role, as before
            second = rng.choices(roles, weights)[0]
            if second != role:
                role_list.append(second)
        contact = p06.contact_handle(role, name, rng)
        note = p06.notes_for(role, title_of[role], ward, rng)
        site_id = eb.site_rid(ward)

        def tile(alias, data, tag, _i=i, _rid=rid):
            return {"tileid": str(uuid.uuid5(uuid.UUID(PERSON_GRAPHID), f"tile:{_i}:{tag}")),
                    "nodegroup_id": pnid(alias), "parenttile_id": None, "resourceinstance_id": _rid,
                    "sortorder": 0, "provisionaledits": None, "data": data}
        tiles = [
            tile("name", {pnid("name"): name}, "name"),
            tile("role_titles", {pnid("role_titles"): role_list}, "role"),
            tile("contact_handle", {pnid("contact_handle"): contact}, "ct"),
            tile("primary_site", {pnid("primary_site"): [{"resourceId": site_id}]}, "site"),
            # THE employer link — person → the org that employs them (coherent occupancy).
            tile("employer", {pnid("employer"): [{"resourceId": person["employer_rid"]}]}, "emp"),
        ]
        # Keyholder link — buildings this person keyholds, already derived coherently by
        # the world (a keyholder is employed by the org occupying that building).
        if person["keyholder_for"]:
            tiles.append(tile("keyholder_for",
                              {pnid("keyholder_for"): [{"resourceId": b} for b in person["keyholder_for"]]}, "kh"))
        # duty_status (E55 standing roster/availability — NOT real-time): keyholders skew
        # on-call (they can be summoned to open a building); others spread across the roster.
        if person["keyholder_for"]:
            duty = rng.choices(["On call", "On duty", "Off duty"], [5, 3, 2])[0]
        else:
            duty = rng.choices(["On duty", "Off duty", "On call", "Unavailable"], [4, 3, 2, 1])[0]
        tiles.append(tile("duty_status", {pnid("duty_status"): duty}, "duty"))
        if note:
            tiles.append(tile("notes", {pnid("notes"): note}, "note"))
        resources.append({
            "resourceinstance": {"resourceinstanceid": rid, "graph_id": PERSON_GRAPHID, "name": name,
                "legacyid": f"pers-{i:06d}",
                "descriptors": {"en": {"name": name, "description": note, "map_popup": ""}}},
            "tiles": tiles})
    return resources


def main():
    ap = argparse.ArgumentParser()
    # Person count now DERIVES from the coherent world's employment (it is not a free
    # knob), so we mirror the building footprint slice exactly to recover the same
    # building rids the building/org emit wrote.
    ap.add_argument("--footprints-count", type=int, default=0, help="mirror emit_buildings --count (shuffled slice, replicas=1)")
    ap.add_argument("--footprints", type=int, default=0, help="mirror emit_buildings --footprints (head slice)")
    ap.add_argument("--replicas", type=int, default=1, help="mirror emit_buildings --replicas (pass 23 for full scale)")
    ap.add_argument("--seed", type=int, default=1904)
    ap.add_argument("--out", type=Path, required=True)
    args = ap.parse_args()

    fp = list(csv.DictReader((eb.DOCS / "scripts/seed/footprints.csv").open()))
    replicas = args.replicas
    if args.footprints_count:
        rng = random.Random(args.seed)
        rng.shuffle(fp)
        fp = fp[:args.footprints_count]
        replicas = 1
    elif args.footprints:
        fp = fp[:args.footprints]

    world = W.build_world(fp, replicas, args.seed)
    resources = generate_from_world(world, args.seed)

    gdir, rdir = args.out / "graphs", args.out / "resources" / "person"
    gdir.mkdir(parents=True, exist_ok=True)
    rdir.mkdir(parents=True, exist_ok=True)
    (gdir / f"{PERSON_GRAPHID}.json").write_text(json.dumps(build_person_graph(), ensure_ascii=False))
    (rdir / "persons.json").write_text(json.dumps({"business_data": {"resources": resources}}, ensure_ascii=False))
    docs = sum(1 for r in resources if "Doctor" in r["tiles"][1]["data"][pnid("role_titles")])
    khs = sum(1 for p in world["persons"] if p["keyholder_for"])
    print(f"person_graphid={PERSON_GRAPHID}")
    print(f"wrote {len(resources)} persons ({docs} doctors); {khs} carry keyholder_for")


if __name__ == "__main__":
    main()

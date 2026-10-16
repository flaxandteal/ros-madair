#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# © Flax & Teal Limited. Aonach Mór — insurance synthesis for the REAL ros-madair-emit
# path (companion to emit_buildings.py / emit_persons.py). Emits TWO related models:
# InsurancePolicy and InsuranceCoverage, as resource-JSON bundles the real melt
# substrate ingests, so the demo's "which policies cover this hospital / what is the
# insurer's ΣTIV window" queries run on real data.
#
# COHERENT with the shared world (world.py): an insurer covers what its INSURED org
# actually occupies, not a random facility sample. Linkage (every cross-model edge
# resolves BY CONSTRUCTION):
#   • policy.insurer → an insurer Organisation rid — a world org whose sector is
#     "Financial and insurance" (the fixed financial pool; occupies == []).
#   • policy.insured_party → an insured Organisation rid — a world org WITH a
#     non-empty `occupies` set (preferring ones that run a critical facility, so the
#     insured stock includes hospitals/EOCs/…). Same uuid5(ORG_GRAPHID,"org:{i}")
#     scheme the org emit writes, so both ends resolve to real orgs.
#   • coverage.part_of_policy → the InsurancePolicy rid we just emitted.
#   • coverage.covered_buildings → a BOUNDED (≤ ~20) sample of THAT INSURED ORG's
#     own `occupies` building rids — so coverage is coherent (the insurer covers what
#     its client occupies) AND resolves (the rids are real buildings the world places).
# The world is built here from the SAME footprint slice args the building/org emit
# use (--footprints-count / --footprints / --replicas), so the recovered rids match.
# Deterministic.
#
#   python example/aonach_mor/build/emit_insurance.py --footprints-count <N> --out <data_dir>

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


eb = _load("emit_buildings", "emit_buildings.py")  # GRAPHID, DOCS, building_stream, TRUE_FACILITIES
W = _load("world", "world.py")  # build_world — the shared coherent occupancy/insurance world

NS = uuid.NAMESPACE_URL
POL_GRAPHID = str(uuid.uuid5(NS, "aonach-mor/resource_models/insurance_policy"))
COV_GRAPHID = str(uuid.uuid5(NS, "aonach-mor/resource_models/insurance_coverage"))
ORG_GRAPHID = str(uuid.uuid5(NS, "aonach-mor/resource_models/organisation"))
BUILDING_GRAPHID = str(uuid.uuid5(NS, "aonach-mor/resource_models/building"))  # == eb.GRAPHID

POL_DIR = ROOT / "graphs/resource_models/insurance_policy"
COV_DIR = ROOT / "graphs/resource_models/insurance_coverage"


def rid5(graphid, key):
    return str(uuid.uuid5(uuid.UUID(graphid), key))


# ── Graph JSON builder — IDENTICAL scheme to emit_buildings.build_graph (nested
#    semantic handled via top_ancestor; per-top nodegroup; deterministic uuid5 ids).
def build_graph(graphid, model_dir, name, root_alias, root_oc):
    def nid(alias):
        return rid5(graphid, alias)

    rows = list(csv.DictReader((model_dir / "nodes.csv").open()))
    by_alias = {r["alias"]: r for r in rows}

    def top_ancestor(alias):
        r = by_alias[alias]
        while r["parent_alias"]:
            r = by_alias[r["parent_alias"]]
        return r["alias"]

    root = {"nodeid": nid(root_alias), "name": name, "alias": root_alias,
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
        edges.append({"edgeid": rid5(graphid, f"edge:{alias}"),
                      "domainnode_id": nid(parent), "rangenode_id": nid(alias),
                      "graph_id": graphid, "ontologyproperty": r["parent_property"]})
        if is_top:
            card = "n" if r["datatype"].endswith("-list") else "1"
            nodegroups[ng_alias] = {"nodegroupid": nid(ng_alias), "cardinality": card,
                                    "parentnodegroup_id": None}
    return {"graph": [{"graphid": graphid, "name": name, "root": root,
                       "nodes": nodes, "nodegroups": list(nodegroups.values()), "edges": edges}]}


# ── concept label pools from a model's collections.csv (full labels, raw-key melt) ──
def coll_labels(model_dir):
    out = {}
    path = model_dir / "collections.csv"
    if not path.exists():
        return out
    for r in csv.DictReader(path.open()):
        name = r.get("collection_name")
        label = r.get("concept_label")
        if name and label:
            out.setdefault(name, []).append(label)
    return out


POLICY_KINDS = ["Portfolio", "Commercial property", "Civic estate", "Critical-infrastructure",
                "Heritage estate", "Municipal", "Industrial estate", "Blanket fire-and-peril"]
COVERAGE_LABEL_FALLBACK = {"CoverageType": ["Property / buildings", "Business interruption",
                                            "Contents / inventory", "Public / third-party liability"],
                           "LimitBand": ["Under 10 k", "10 k – 100 k", "100 k – 1 m",
                                         "1 m – 10 m", "10 m – 100 m", "Over 100 m"]}


COVERED_CAP = 20  # max covered_buildings per coverage line


def generate_from_world(world, n_policies, seed):
    """Emit policies COHERENTLY with the shared world: an insurer covers what its
    INSURED org occupies. `covered_buildings` is a bounded (≤ COVERED_CAP) sample of
    THAT insured org's own `occupies` set, so coverage is coherent and resolves."""
    rng = random.Random(seed)

    insurers = [o for o in world["orgs"] if o["sector"] == "Financial and insurance"]
    occupiers = [o for o in world["orgs"] if o["occupies"]]
    facility_occupiers = [o for o in occupiers if o["_fac"]]
    if not insurers or not occupiers:
        raise SystemExit("world has no insurer orgs or no occupying orgs — cannot build coherent policies")

    cov_labels = coll_labels(COV_DIR)
    coverage_types = cov_labels.get("CoverageType") or COVERAGE_LABEL_FALLBACK["CoverageType"]
    limit_bands = cov_labels.get("LimitBand") or COVERAGE_LABEL_FALLBACK["LimitBand"]

    def pnid(a):
        return rid5(POL_GRAPHID, a)

    def cnid(a):
        return rid5(COV_GRAPHID, a)

    def pick_insured():
        # Prefer an org that runs a critical facility (so the insured stock includes
        # hospitals/EOCs/…), else any occupying org.
        if facility_occupiers and rng.random() < 0.6:
            return rng.choice(facility_occupiers)
        return rng.choice(occupiers)

    policies = []
    coverages = []
    cov_i = 0
    for i in range(1, n_policies + 1):
        prid = rid5(POL_GRAPHID, f"policy:{i}")
        insurer = rng.choice(insurers)
        insured = pick_insured()
        insurer_rid = insurer["rid"]
        insured_rid = insured["rid"]
        insured_occ = insured["occupies"]

        kind = rng.choice(POLICY_KINDS)
        year = rng.choice([2021, 2022, 2023, 2024])
        month = rng.randint(1, 12)
        eff_from = f"{year}-{month:02d}-01"
        eff_to = f"{year + 1}-{month:02d}-01"
        pid = f"POL-{i:04d}"
        pname = f"{kind} policy {pid} ({year})"
        pnote = (f"{kind} cover underwritten for org {insured['i']} "
                 f"({len(insured_occ)} occupied building(s)); effective {eff_from} to {eff_to}.")

        def ptile(alias, data, tag, _i=i, _rid=prid):
            return {"tileid": rid5(POL_GRAPHID, f"tile:{_i}:{tag}"), "nodegroup_id": pnid(alias),
                    "parenttile_id": None, "resourceinstance_id": _rid, "sortorder": 0,
                    "provisionaledits": None, "data": data}

        policies.append({
            "resourceinstance": {"resourceinstanceid": prid, "graph_id": POL_GRAPHID, "name": pname,
                "legacyid": f"pol-{i:06d}",
                "descriptors": {"en": {"name": pname, "description": pnote, "map_popup": ""}}},
            "tiles": [
                ptile("policy_id", {pnid("policy_id"): pid}, "pid"),
                ptile("policy_name", {pnid("policy_name"): pname}, "pname"),
                ptile("insurer", {pnid("insurer"): [{"resourceId": insurer_rid}]}, "ins"),
                ptile("insured_party", {pnid("insured_party"): [{"resourceId": insured_rid}]}, "insd"),
                ptile("effective_span",
                      {pnid("effective_from"): eff_from, pnid("effective_to"): eff_to}, "span"),
                ptile("notes", {pnid("notes"): pnote}, "note"),
            ]})

        # 1–3 coverage lines per policy; each covers a BOUNDED sample of the INSURED
        # ORG's own occupancy (≤ COVERED_CAP) — coherent, and resolves by construction.
        for _ in range(rng.randint(1, 3)):
            cov_i += 1
            crid = rid5(COV_GRAPHID, f"coverage:{cov_i}")
            cov_type = rng.choice(coverage_types)
            band = rng.choice(limit_bands)
            # bounded sample of the insured org's occupancy; if it occupies fewer
            # than the draw, cover them all.
            k = min(len(insured_occ), COVERED_CAP, rng.randint(3, COVERED_CAP))
            covered = rng.sample(insured_occ, k) if k else []
            cid = f"COV-{cov_i:04d}"
            cnote = f"{cov_type} coverage ({band}) on {len(covered)} covered building(s) under {pid}."

            def ctile(alias, data, tag, _i=cov_i, _rid=crid):
                return {"tileid": rid5(COV_GRAPHID, f"tile:{_i}:{tag}"), "nodegroup_id": cnid(alias),
                        "parenttile_id": None, "resourceinstance_id": _rid, "sortorder": 0,
                        "provisionaledits": None, "data": data}

            tiles = [
                ctile("coverage_id", {cnid("coverage_id"): cid}, "cid"),
                ctile("part_of_policy", {cnid("part_of_policy"): [{"resourceId": prid}]}, "pol"),
                ctile("covered_buildings",
                      {cnid("covered_buildings"): [{"resourceId": r} for r in covered]}, "bld"),
                ctile("coverage_type", {cnid("coverage_type"): cov_type}, "ctype"),
                ctile("limit_band", {cnid("limit_band"): band}, "band"),
                ctile("notes", {cnid("notes"): cnote}, "note"),
            ]
            coverages.append({
                "resourceinstance": {"resourceinstanceid": crid, "graph_id": COV_GRAPHID, "name": cid,
                    "legacyid": f"cov-{cov_i:06d}",
                    "descriptors": {"en": {"name": cid, "description": cnote, "map_popup": ""}}},
                "tiles": tiles})

    return policies, coverages, len(insurers), len(occupiers), len(facility_occupiers)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--policies", type=int, default=36, help="number of InsurancePolicy resources (a few dozen)")
    # Mirror emit_buildings/emit_organisations' footprint slice EXACTLY so the world
    # built here recovers the SAME building rids: --footprints-count mirrors --count
    # (shuffle+slice on the same seed, replicas=1); --footprints mirrors --footprints
    # (head slice); --replicas mirrors --replicas (pass 23 for full scale).
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

    gdir = args.out / "graphs"
    gdir.mkdir(parents=True, exist_ok=True)
    (gdir / f"{POL_GRAPHID}.json").write_text(json.dumps(
        build_graph(POL_GRAPHID, POL_DIR, "Insurance policy", "insurance_policy",
                    "https://rosmadair.example.org/aonach_mor/ins#InsurancePolicy"), ensure_ascii=False))
    (gdir / f"{COV_GRAPHID}.json").write_text(json.dumps(
        build_graph(COV_GRAPHID, COV_DIR, "Insurance coverage", "insurance_coverage",
                    "https://rosmadair.example.org/aonach_mor/ins#InsuranceCoverage"), ensure_ascii=False))

    policies, coverages, n_ins, n_occ, n_fac = generate_from_world(world, args.policies, args.seed)
    pdir = args.out / "resources" / "insurance_policy"
    cdir = args.out / "resources" / "insurance_coverage"
    pdir.mkdir(parents=True, exist_ok=True)
    cdir.mkdir(parents=True, exist_ok=True)
    (pdir / "policies.json").write_text(
        json.dumps({"business_data": {"resources": policies}}, ensure_ascii=False))
    (cdir / "coverages.json").write_text(
        json.dumps({"business_data": {"resources": coverages}}, ensure_ascii=False))

    print(f"policy_graphid={POL_GRAPHID}")
    print(f"coverage_graphid={COV_GRAPHID}")
    print(f"wrote {len(policies)} policies + {len(coverages)} coverage lines; "
          f"covered_buildings drawn from each insured org's own occupancy "
          f"({n_ins} insurer orgs, {n_occ} occupying orgs, {n_fac} of them run a facility)")


if __name__ == "__main__":
    main()

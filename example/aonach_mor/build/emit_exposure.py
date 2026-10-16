#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# © Flax & Teal Limited. Aonach Mór — exposure synthesis for the REAL ros-madair-emit
# path (companion to emit_buildings.py / emit_persons.py). Emits ONE model (Exposure,
# prov:Entity) as resource-JSON bundles so the demo's "what's exposed under this
# hazard footprint / ΣTIV-per-footprint" queries run on the real melt substrate.
#
# An Exposure is the join between a Building and a HazardFootprint: for each of the
# ~94 scenario footprints emit_hazard.py produced, we take the buildings in the wards
# (and/or zones) that footprint's polygon COVERS and write one Exposure per
# (sampled building, footprint). Building rids are recovered from the SAME
# eb.building_stream the real building emit ran (reproduce its footprint slice +
# replicas + seed), so about_building points at the exact building the building emit
# produced; from_footprint points at the footprint rid read from emit_hazard's output.
#
# SCALE: Exposure exists ONLY for the scenario footprints, and is CAPPED per footprint
# (MAX_EXPOSURES_PER_FOOTPRINT) — NEVER one-per-building-citywide (that would be
# millions at 763k). Over-cap stock is dropped and LOGGED, never silently truncated.
# Deterministic.
#
#   # run AFTER emit_hazard.py has populated <out>/resources/hazard_footprint/
#   python example/aonach_mor/build/emit_exposure.py --footprints-count 800 --out <data_dir>
# writes <data_dir>/graphs/<EXP_GRAPHID>.json and
#        <data_dir>/resources/exposure/exposures.json

import argparse
import csv
import importlib.util
import json
import random
import sys
import uuid
from collections import defaultdict
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent


def _load(mod, fname):
    spec = importlib.util.spec_from_file_location(mod, HERE / fname)
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


eb = _load("emit_buildings", "emit_buildings.py")  # BUILDING_GRAPHID, strip_ward, building_stream, DOCS

NS = uuid.NAMESPACE_URL
EXP_GRAPHID = str(uuid.uuid5(NS, "aonach-mor/resource_models/exposure"))
BUILDING_GRAPHID = str(uuid.uuid5(NS, "aonach-mor/resource_models/building"))
HF_GRAPHID = str(uuid.uuid5(NS, "aonach-mor/resource_models/hazard_footprint"))
MODEL_DIR = ROOT / "graphs/resource_models/exposure"

PROV = "http://www.w3.org/ns/prov#"

# ── scale cap: hard upper bound on Exposure rows per footprint. ~94 footprints ×
#    this ceiling bounds the whole model; the severity-driven fraction keeps the
#    realistic count well below it. Over-cap stock is DROPPED and logged (loud, not
#    silent).
MAX_EXPOSURES_PER_FOOTPRINT = 3000

# Severity band (SeverityBand collection) → fraction of covered building stock that
# records an exposure. Higher severity ⇒ a larger share of the stock is affected.
SEVERITY_FRACTION = {"Low": 0.10, "Medium": 0.25, "High": 0.50, "Very high": 0.80}
SEVERITY_BANDS = ["Low", "Medium", "High", "Very high"]
CONFIDENCE_BANDS = ["Low", "Medium", "High"]


def nid(alias):
    return str(uuid.uuid5(uuid.UUID(EXP_GRAPHID), alias))


# ── 1. Exposure graph JSON (same scheme as emit_buildings.build_graph: deterministic
#    uuid5 node/edge/nodegroup ids; nested dim/span wrappers grouped onto their top
#    ancestor's nodegroup). ──────────────────────────────────────────────────────
def build_graph():
    rows = list(csv.DictReader((MODEL_DIR / "nodes.csv").open()))
    by_alias = {r["alias"]: r for r in rows}

    def top_ancestor(alias):
        r = by_alias[alias]
        while r["parent_alias"]:
            r = by_alias[r["parent_alias"]]
        return r["alias"]

    root_alias = "exposure"
    root = {"nodeid": nid(root_alias), "name": "Exposure", "alias": root_alias,
            "datatype": "semantic", "graph_id": EXP_GRAPHID, "istopnode": True,
            "is_collector": False, "isrequired": False, "issearchable": True,
            "exportable": False, "sortorder": 0,
            "ontologyclass": PROV + "Entity"}
    nodes = [root]
    edges = []
    nodegroups = {}
    for r in rows:
        alias = r["alias"]
        ng_alias = top_ancestor(alias)
        is_top = not r["parent_alias"]
        nodes.append({
            "nodeid": nid(alias), "name": r["name"], "alias": alias,
            "datatype": r["datatype"], "graph_id": EXP_GRAPHID, "istopnode": False,
            "is_collector": is_top, "isrequired": r["required"] == "true",
            "issearchable": r["searchable"] == "true", "exportable": r["exportable"] == "true",
            "sortorder": int(r["sortorder"]), "ontologyclass": r["ontology_class"],
            "nodegroup_id": nid(ng_alias),
        })
        parent = r["parent_alias"] or root_alias
        edges.append({"edgeid": str(uuid.uuid5(uuid.UUID(EXP_GRAPHID), f"edge:{alias}")),
                      "domainnode_id": nid(parent), "rangenode_id": nid(alias),
                      "graph_id": EXP_GRAPHID, "ontologyproperty": r["parent_property"]})
        if is_top:
            card = "n" if r["datatype"].endswith("-list") else "1"
            nodegroups[ng_alias] = {"nodegroupid": nid(ng_alias), "cardinality": card,
                                    "parentnodegroup_id": None}
    return {"graph": [{"graphid": EXP_GRAPHID, "name": "Exposure", "root": root,
                       "nodes": nodes, "nodegroups": list(nodegroups.values()), "edges": edges}]}


# ── 2. read emit_hazard.py's output — the ~94 scenario footprints + the wards/zones
#    each polygon covers. We depend on it; if it is absent we FAIL loudly. ─────────
def load_footprints(out_dir):
    """Return [(fp_rid, covered_wards_set, covered_zones_list, severity)] read from
    <out>/resources/hazard_footprint/*.json. Raises SystemExit (loud) if the bundle
    is missing/empty or carries no coverage tags.

    Coverage contract (what we read from each resource object in the bundle):
      - fp_rid       := resource["resourceinstance"]["resourceinstanceid"]
                        (emit_hazard uses uuid5(HF_GRAPHID, f"footprint:{i}"))
      - covered wards := resource["coverage"]["wards"]  (preferred)
                         or the flat resource["wards"]  (fallback)
      - covered zones := resource["coverage"]["zones"]  or resource["zones"] (optional)
      - severity      := resource["coverage"]["severity"] / resource["severity"]
                         (optional; else inferred from a SeverityBand concept tile,
                         else defaults to "Medium")
    Report the exact keys matched so the operator can confirm against emit_hazard.
    """
    hf_dir = out_dir / "resources" / "hazard_footprint"
    files = sorted(hf_dir.glob("*.json")) if hf_dir.is_dir() else []
    if not files:
        sys.exit(
            f"FATAL: no hazard_footprint bundle at {hf_dir}.\n"
            f"       emit_exposure depends on emit_hazard's output — run\n"
            f"         python {HERE / 'emit_hazard.py'} --out {out_dir} ...\n"
            f"       FIRST, then re-run emit_exposure with the same --out.")

    def _coverage(res):
        cov = res.get("coverage")
        if isinstance(cov, dict):
            return (cov.get("wards") or [], cov.get("zones") or [], cov.get("severity"),
                    "coverage.wards")
        if res.get("wards") is not None:
            return (res.get("wards") or [], res.get("zones") or [], res.get("severity"),
                    "wards")
        ri = res.get("resourceinstance", {})
        if isinstance(ri, dict) and ri.get("wards") is not None:
            return (ri.get("wards") or [], ri.get("zones") or [], ri.get("severity"),
                    "resourceinstance.wards")
        return ([], [], None, None)

    def _severity_from_tiles(res):
        for t in res.get("tiles", []):
            for v in (t.get("data") or {}).values():
                if isinstance(v, str) and v in SEVERITY_FRACTION:
                    return v
        return None

    out = []
    matched_keys = set()
    skipped_no_rid = 0
    for f in files:
        doc = json.loads(f.read_text())
        for res in doc.get("business_data", {}).get("resources", []):
            rid = res.get("resourceinstance", {}).get("resourceinstanceid")
            if not rid:
                skipped_no_rid += 1
                continue
            wards, zones, sev, key = _coverage(res)
            if key:
                matched_keys.add(key)
            sev = sev or _severity_from_tiles(res)
            sev = sev if sev in SEVERITY_FRACTION else "Medium"
            wards = [eb.strip_ward(w) for w in wards]
            out.append((rid, set(wards), list(zones), sev))

    if not out:
        sys.exit(f"FATAL: hazard_footprint bundle at {hf_dir} has no footprint resources.")
    if not any(w or z for _, w, z, _ in out):
        sys.exit(
            f"FATAL: hazard_footprint bundle at {hf_dir} carries NO coverage tags.\n"
            f"       emit_exposure needs each footprint tagged with the wards/zones its\n"
            f"       polygon covers. Expected one of: resource['coverage']['wards'] "
            f"(preferred), resource['wards'], or resource['resourceinstance']['wards'].")
    # stable order so exposure numbering is deterministic
    out.sort(key=lambda t: t[0])
    return out, (sorted(matched_keys) or ["(none — zones only)"]), skipped_no_rid


# ── 3. index the building corpus by ward (the SAME stream the building emit ran). ──
def building_index(fp, replicas, seed):
    """ward (clean) → [(building_rid, height, name)], from eb.building_stream with the
    exact fp slice / replicas / seed the building emit used (so rids match).

    NOTE (determinism): eb.building_stream places the fixed critical facilities by
    iterating CRIT_ZONES[role] — a set — so their footprint (and thus ward) selection
    is PYTHONHASHSEED-salted across processes. The bulk housing is fully deterministic;
    only the ~158 criticals shift. emit_persons shares this (its hospital_wards reads
    ONLY the criticals), so the pipeline must pin PYTHONHASHSEED for building rids to
    line up across emit_buildings / emit_persons / emit_exposure. With it pinned this
    generator is byte-for-byte deterministic."""
    by_ward = defaultdict(list)
    for res, s in eb.building_stream(fp, replicas, seed):
        by_ward[s["ward"]].append(
            (res["resourceinstance"]["resourceinstanceid"], s["height"], s["name"]))
    return by_ward


def zone_to_wards(fp):
    z2w = defaultdict(set)
    for f in fp:
        z2w[f["zone"]].add(eb.strip_ward(f["ward"]))
    return z2w


# ── 4. emit one Exposure per (sampled building, footprint). ───────────────────────
def generate(footprints, by_ward, z2w, seed):
    resources = []
    j = 0
    total_dropped = 0
    per_fp = []
    for (fp_rid, cov_wards, cov_zones, sev) in footprints:
        wards = set(cov_wards)
        for z in cov_zones:
            wards |= z2w.get(z, set())
        cands = [b for w in sorted(wards) for b in by_ward.get(w, [])]
        frac = SEVERITY_FRACTION.get(sev, 0.25)
        want = round(frac * len(cands))
        n = min(want, MAX_EXPOSURES_PER_FOOTPRINT, len(cands))
        dropped = max(0, want - MAX_EXPOSURES_PER_FOOTPRINT)
        if dropped:
            total_dropped += dropped
            print(f"  [cap] footprint {fp_rid}: ward stock {len(cands)} × {sev} fraction "
                  f"{frac} = {want} wanted, capped to {MAX_EXPOSURES_PER_FOOTPRINT} "
                  f"(DROPPED {dropped})")
        # deterministic per-footprint rng: seed from the rid (str seed → stable hash,
        # NOT process-salted hash()).
        rng = random.Random(f"{seed}:{fp_rid}")
        chosen = rng.sample(cands, n) if n < len(cands) else list(cands)
        per_fp.append((fp_rid, sev, len(cands), len(chosen)))
        for (b_rid, height, b_name) in chosen:
            j += 1
            rid = str(uuid.uuid5(uuid.UUID(EXP_GRAPHID), f"exposure:{j}"))
            exposure_id = f"EXP-{j:07d}"
            # TIV: reuse the building emit's height×U(50k,200k) form (own rng, so
            # deterministic for this exposure).
            value_amount = int(height * rng.uniform(50_000, 200_000))
            # plausible headcount affected, scaled by storeys and severity.
            affected = max(1, int(height * rng.uniform(1.5, 5.0)
                                  * (1.0 + SEVERITY_FRACTION.get(sev, 0.25))))
            confidence = rng.choices(CONFIDENCE_BANDS, [1, 2, 2])[0]
            month = rng.randint(1, 10)
            day = rng.randint(1, 28)
            assessment_date = f"2026-{month:02d}-{day:02d}"
            note = (f"{sev} exposure for {b_name} under scenario footprint; "
                    f"~{affected} affected, ΣTIV contribution {value_amount}.")
            name = f"Exposure — {b_name}"

            def tile(ng_alias, data, tag, _j=j, _rid=rid):
                return {"tileid": str(uuid.uuid5(uuid.UUID(EXP_GRAPHID), f"tile:{_j}:{tag}")),
                        "nodegroup_id": nid(ng_alias), "parenttile_id": None,
                        "resourceinstance_id": _rid, "sortorder": 0,
                        "provisionaledits": None, "data": data}

            tiles = [
                tile("exposure_id", {nid("exposure_id"): exposure_id}, "eid"),
                tile("about_building", {nid("about_building"): [{"resourceId": b_rid}]}, "bld"),
                tile("from_footprint", {nid("from_footprint"): [{"resourceId": fp_rid}]}, "fp"),
                tile("severity_band", {nid("severity_band"): sev}, "sev"),
                tile("confidence", {nid("confidence"): confidence}, "conf"),
                tile("affected_count_dim", {nid("affected_count"): affected}, "aff"),
                tile("value_amount_dim", {nid("value_amount"): value_amount}, "val"),
                tile("assessment_span", {nid("assessment_date"): assessment_date}, "date"),
                tile("notes", {nid("notes"): note}, "note"),
            ]
            resources.append({
                "resourceinstance": {"resourceinstanceid": rid, "graph_id": EXP_GRAPHID,
                    "name": name, "legacyid": f"expo-{j:07d}",
                    "descriptors": {"en": {"name": name, "description": note, "map_popup": ""}}},
                "tiles": tiles})
    return resources, per_fp, total_dropped


def main():
    ap = argparse.ArgumentParser()
    # Mirror emit_buildings' footprint slice EXACTLY so building rids match the ones
    # the building emit produced: --footprints-count mirrors its --count (shuffle+slice
    # on the same seed, replicas→1); --footprints mirrors its --footprints (head slice).
    ap.add_argument("--footprints-count", type=int, default=0)
    ap.add_argument("--footprints", type=int, default=0)
    ap.add_argument("--replicas", type=int,
                    default=int(__import__("os").environ.get("AONACH_REPLICAS", "23")))
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

    footprints, matched_keys, skipped = load_footprints(args.out)
    by_ward = building_index(fp, replicas, args.seed)
    z2w = zone_to_wards(fp)

    gdir = args.out / "graphs"
    rdir = args.out / "resources" / "exposure"
    gdir.mkdir(parents=True, exist_ok=True)
    rdir.mkdir(parents=True, exist_ok=True)
    (gdir / f"{EXP_GRAPHID}.json").write_text(json.dumps(build_graph(), ensure_ascii=False))

    resources, per_fp, total_dropped = generate(footprints, by_ward, z2w, args.seed)
    (rdir / "exposures.json").write_text(
        json.dumps({"business_data": {"resources": resources}}, ensure_ascii=False))

    print(f"exposure_graphid={EXP_GRAPHID}")
    print(f"read hazard coverage via key(s): {matched_keys}"
          + (f"  (skipped {skipped} resources with no rid)" if skipped else ""))
    print(f"cap = {MAX_EXPOSURES_PER_FOOTPRINT} exposures/footprint; total dropped = {total_dropped}")
    print(f"wrote {len(resources)} exposures across {len(footprints)} footprints "
          f"({len(by_ward)} wards in building index)")
    for fp_rid, sev, ncand, nchosen in per_fp[:6]:
        print(f"  {fp_rid}  sev={sev:9} stock={ncand:6} → {nchosen} exposures")


if __name__ == "__main__":
    main()

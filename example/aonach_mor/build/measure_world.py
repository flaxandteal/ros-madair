#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# "Really look at the edge corpus": build the coherent world's edges in networkx and
# measure whether DEEP multi-hop chains actually emerge (the thing a star-around-Site
# graph lacked). Run with the build venv: ./.venv/bin/python measure_world.py
import argparse
import csv
import importlib.util
import random
from pathlib import Path

import networkx as nx

HERE = Path(__file__).resolve().parent


def _load(m, f):
    s = importlib.util.spec_from_file_location(m, HERE / f)
    mod = importlib.util.module_from_spec(s)
    s.loader.exec_module(mod)
    return mod


eb = _load("emit_buildings", "emit_buildings.py")
W = _load("world", "world.py")
H = _load("emit_hazard", "emit_hazard.py")

ap = argparse.ArgumentParser()
ap.add_argument("--footprints-count", type=int, default=4000)
ap.add_argument("--replicas", type=int, default=1)
ap.add_argument("--seed", type=int, default=1904)
a = ap.parse_args()

fp = list(csv.DictReader((eb.DOCS / "scripts/seed/footprints.csv").open()))
rng = random.Random(a.seed)
rng.shuffle(fp)
fp = fp[:a.footprints_count]

world = W.build_world(fp, a.replicas, a.seed)

# building → ward (site) from the canonical stream; and the hazard footprint → wards.
b_ward = {}
for res, s in eb.building_stream(fp, a.replicas, a.seed):
    b_ward[res["resourceinstance"]["resourceinstanceid"]] = s["ward"]
_, _, hmap = H.generate(a.seed)
fp_wards = {m["footprint_rid"]: set(m["wards"]) for m in hmap}

G = nx.Graph()  # undirected: we ask "what is reachable / how deep do chains go"
etypes = {}


def add(u, v, t):
    G.add_edge(u, v)
    etypes[t] = etypes.get(t, 0) + 1


# org -occupies- building ; person -employer- org ; person -keyholder- building
for o in world["orgs"]:
    for b in o["occupies"]:
        add(("org", o["i"]), ("bld", b), "org_occupies_building")
for p in world["persons"]:
    add(("per", p["i"]), ("org", p["employer_i"]), "person_employer_org")
    for b in p["keyholder_for"]:
        add(("per", p["i"]), ("bld", b), "person_keyholder_building")
# building -site- ward
for b, w in b_ward.items():
    add(("bld", b), ("site", w), "building_site")
# footprint -exposes- building (building in a ward the footprint covers)
bw_by_ward = {}
for b, w in b_ward.items():
    bw_by_ward.setdefault(w, []).append(b)
exp = 0
for frid, wards in fp_wards.items():
    for w in wards:
        for b in bw_by_ward.get(w, []):
            if ("bld", b) in G:  # only buildings that are in the occupancy/site web
                add(("fp", frid), ("bld", b), "footprint_exposes_building")
                exp += 1

print(f"=== world @ {a.footprints_count} footprints ===")
print(f"orgs={len(world['orgs'])} persons={len(world['persons'])} "
      f"occupiable={world['n_occupiable']} facilities={world['n_facilities']}")
print(f"graph: {G.number_of_nodes()} nodes, {G.number_of_edges()} edges")
print("edge types:")
for t, n in sorted(etypes.items(), key=lambda x: -x[1]):
    print(f"  {n:8}  {t}")

# connectivity: one web, or fragments?
comps = sorted((len(c) for c in nx.connected_components(G)), reverse=True)
print(f"connected components: {len(comps)}; largest={comps[0]} "
      f"({100*comps[0]//G.number_of_nodes()}% of nodes); next few={comps[1:5]}")

# chain depth: from a sample of hazard footprints, how many nodes are reachable at
# each hop — does a hazard reach buildings→orgs→people→their other buildings…?
fps = [n for n in G if n[0] == "fp" and G.degree(n) > 0]
print(f"\nreachability from a hazard footprint (avg over {min(20,len(fps))} sampled):")
samp = random.Random(1).sample(fps, min(20, len(fps)))
import statistics
for depth in range(1, 7):
    sizes = []
    for f in samp:
        seen = {f}
        frontier = {f}
        for _ in range(depth):
            nxt = set()
            for u in frontier:
                nxt |= set(G.neighbors(u))
            frontier = nxt - seen
            seen |= nxt
        sizes.append(len(seen) - 1)
    print(f"  ≤{depth} hops: reaches ~{int(statistics.mean(sizes))} nodes "
          f"(min {min(sizes)}, max {max(sizes)})")

# the specific deep responder chain: footprint → exposed building → occupying org →
# an employee who is ALSO a keyholder of a (possibly different) building.
chain4 = 0
kh_people = {("per", p["i"]) for p in world["persons"] if p["keyholder_for"]}
for f in samp:
    for b in G.neighbors(f):
        if b[0] != "bld":
            continue
        for o in G.neighbors(b):
            if o[0] != "org":
                continue
            for per in G.neighbors(o):
                if per in kh_people:
                    chain4 += 1
                    break
print(f"\n4-hop responder chains (footprint→building→org→keyholder-employee) "
      f"found in sample: {chain4}")

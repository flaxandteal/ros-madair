#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
# © Flax & Teal Limited. Aonach Mór Responder — the THREE SIGNED SLICES.
#
# Produces the signed heads the Responder/insurer app composes, proving the
# default-deny tier (egress-by-emit) AND the "two independent owners compose
# on-device" beat on real-shaped Aonach data. Each slice is a separately-signed
# head; the demo composes the two the responder needs, and gives the insurer only
# its own.
#
#   building_head/  — OWNER 1 (council). All buildings with capacity / service-role /
#                     insured_value / footprint (+ their sites). NO persons.
#   contact_head/   — OWNER 2 (a contacts custodian). The keyholders (persons), whose
#                     keyholder_for links point at building_head's building ids — a
#                     SHARED-ID join composed on-device, NOT a merged dump.
#   insurer_head/   — the TIERED window: ONLY the covered buildings (insured_value +
#                     capacity + footprint). Persons/orgs PHYSICALLY ABSENT; it is a
#                     different artifact, not a filtered view. manifest.tier declares
#                     the scope; the signed snapshot_id covers it.
#
# Responder app opens  building_head + contact_head  → tap a building → its
# keyholders via cited_by ACROSS the two signed slices.
# Insurer window opens  insurer_head  alone → covered buildings + ΣTIV, no contacts.
#
# The protection is L0 (absence) + distribution control: each party is handed a
# genuinely scoped artifact. (L1 — a typed "denied" at compile time — is the honest
# UX on top; see crates/ros-madair-duck/tests/tier.rs.)
#
# Usage:
#   cargo build -p ros-madair-emit --bin ros-madair-emit
#   example/aonach_mor/build/responder_slices.sh [COUNT] [OUT_DIR]
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
COUNT="${1:-200}"
OUT="${2:-$REPO/target/aonach-slices}"
# REPLICAS re-samples each footprint N times (dispersed into the urban fabric) to scale the
# building stock beyond the ~33k real footprints — deterministic, so the buildings match the
# published docs corpus. e.g. 33172 × 2 ≈ 66k. The relational world (orgs/persons/insurance)
# scales proportionally off the same stream.
REPLICAS="${3:-1}"
EMIT="${EMIT:-$REPO/target/debug/ros-madair-emit}"
BASE="https://rosmadair.example.org/aonach_mor/"
SEED="${SEED:-1904}"

[ -x "$EMIT" ] || { echo "build the emitter first: cargo build -p ros-madair-emit --bin ros-madair-emit"; exit 1; }

DATA="$OUT/data"
rm -rf "$OUT"; mkdir -p "$DATA"

# Promote the responder's card/pin fields onto the grid edge (RM render_fields config — the
# DOMAIN names them; RM reads the list). The building graph carries these; other graphs skip them.
export ROS_MADAIR_RENDER_FIELDS="service_roles,capacity,height_m,cluster_type"

echo "== 1. generate the coherent world (buildings → keyholders → orgs → coverage) =="
python3 "$HERE/emit_buildings.py"      --count           "$COUNT" --replicas "$REPLICAS" --out "$DATA" >/dev/null
python3 "$HERE/emit_organisations.py"  --footprints-count "$COUNT" --replicas "$REPLICAS" --seed "$SEED" --out "$DATA" >/dev/null
python3 "$HERE/emit_persons.py"        --footprints-count "$COUNT" --replicas "$REPLICAS" --seed "$SEED" --out "$DATA" >/dev/null
python3 "$HERE/emit_insurance.py"      --footprints-count "$COUNT" --replicas "$REPLICAS" --seed "$SEED" --out "$DATA" >/dev/null

echo "== 2. covered-building ids + the model graph ids =="
python3 - "$DATA" <<'PY'
import json, sys, glob
data = sys.argv[1]
def resources(model):
    out = []
    for f in glob.glob(f"{data}/resources/{model}/*.json"):
        out += json.load(open(f)).get("business_data", {}).get("resources", [])
    return out
rid = lambda r: r["resourceinstance"]["resourceinstanceid"]
buildings = {rid(r) for r in resources("building")}
refs = set()
def walk(o):
    if isinstance(o, dict):
        if isinstance(o.get("resourceId"), str): refs.add(o["resourceId"])
        for v in o.values(): walk(v)
    elif isinstance(o, list):
        for v in o: walk(v)
for r in resources("insurance_coverage"): walk(r)
covered = sorted(refs & buildings)
open(f"{data}/covered_ids.txt", "w").write("\n".join(covered) + "\n")
gid = {}
for f in glob.glob(f"{data}/graphs/*.json"):
    g = json.load(open(f))["graph"][0]
    gid[g["name"]] = g["graphid"]
# building site person organisation policy coverage
order = ["Building", "Site", "Person", "Organisation", "Insurance policy", "Insurance coverage"]
open(f"{data}/.gids", "w").write("\n".join(gid[k] for k in order) + "\n")
print(f"   {len(buildings)} buildings, {len(covered)} covered")
PY
mapfile -t G < "$DATA/.gids"
BUILDING="${G[0]}"; SITE="${G[1]}"; PERSON="${G[2]}"; ORG="${G[3]}"; POLICY="${G[4]}"; COVERAGE="${G[5]}"

# Owner-declared provenance (presentation/policy, not crypto identity) — the field-register
# trust UI reads these beside the signature. Caller-provided as-of (fixed) keeps the manifest
# reproducible. Colours match the design review's per-owner swatches.
ASOF="${ASOF:-2026-10-22T00:00:00Z}"

echo "== 3. building_head (OWNER 1 — buildings + sites, no contacts) =="
"$EMIT" "$DATA" "$OUT/building_head" --base-uri "$BASE" \
  --tier council-buildings \
  --owner-name "City Council · Asset Register" --owner-colour "#2F7F7A" \
  --as-of "$ASOF" --freshness-window-secs 2592000 \
  --exclude-model "$PERSON" --exclude-model "$ORG" \
  --exclude-model "$POLICY" --exclude-model "$COVERAGE" >/dev/null

echo "== 4. contact_head (OWNER 2 — keyholders/persons only, shared building ids) =="
"$EMIT" "$DATA" "$OUT/contact_head" --base-uri "$BASE" \
  --tier keyholder-contacts \
  --owner-name "Relief Network · Keyholders" --owner-colour "#A54C86" \
  --as-of "$ASOF" --freshness-window-secs 1209600 \
  --exclude-model "$BUILDING" --exclude-model "$SITE" \
  --exclude-model "$ORG" --exclude-model "$POLICY" --exclude-model "$COVERAGE" >/dev/null

echo "== 5. insurer_head (TIERED window — covered buildings only, no contacts) =="
"$EMIT" "$DATA" "$OUT/insurer_head" --base-uri "$BASE" \
  --tier insurer-book --include-resources "$DATA/covered_ids.txt" \
  --owner-name "Covered Book · Insurer" --owner-colour "#6E7F2E" \
  --as-of "$ASOF" --freshness-window-secs 2592000 \
  --exclude-model "$PERSON" --exclude-model "$ORG" >/dev/null

echo "== 6. sign + verify all three (one owner key each would be ideal; one key here) =="
KEY="$OUT/aonach_signing.key"
for head in building_head contact_head insurer_head; do
  "$EMIT" sign "$OUT/$head" "$KEY" --actor https://flaxandteal.co.uk --actor-name "Flax & Teal" --role endorsed >/dev/null
  printf "   %-15s " "$head:"; "$EMIT" verify "$OUT/$head"
done

echo "== 7. copy each model's graph INTO its head (the hydrate/buffer path reads graph.json"
echo "       + graph_<gid>.json from the head dir; emit ships only parquet + manifest) =="
for head in building_head contact_head insurer_head; do
  python3 - "$OUT/$head" "$DATA/graphs" <<'PY'
import json, sys, shutil, os
hd, graphs = sys.argv[1], sys.argv[2]
models = json.load(open(f"{hd}/manifest.json")).get("models", [])
for i, mod in enumerate(models):
    src = f"{graphs}/{mod['graph_id']}.json"
    if not os.path.exists(src):
        print(f"   MISSING graph {mod['graph_id']} ({mod['slug']})"); continue
    shutil.copy(src, f"{hd}/graph_{mod['graph_id']}.json")
    if i == 0:
        shutil.copy(src, f"{hd}/graph.json")  # primary = the spine (grid-carrying) model
print(f"   {os.path.basename(hd)}: {len(models)} graph(s) copied")
PY
done

echo "== done =="
echo "   RESPONDER composes:  $OUT/building_head  +  $OUT/contact_head"
echo "   INSURER opens:       $OUT/insurer_head   (tier insurer-book — contact-free, signed)"

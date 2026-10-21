#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
# © Flax & Teal Limited. Aonach Mór Responder — the TWO SIGNED SLICES.
#
# Produces the two heads the Responder/insurer demo composes, proving the
# default-deny tier (egress-by-emit) on real-shaped Aonach data:
#
#   responder_head/  — the FULL head: buildings + their keyholders (persons),
#                      organisations, sites, policies, coverage. The field
#                      responder, offline, taps a point and gets buildings in
#                      range AND their keyholders/contacts.
#   insurer_head/    — the TIERED head: ONLY the covered buildings, with their
#                      insured_value + capacity + footprint. NO persons, NO
#                      organisations — not filtered at read time, PHYSICALLY ABSENT
#                      (there is no person/organisation parquet in the head at all).
#                      manifest.tier self-declares the exclusions; the signed
#                      snapshot_id covers them, so contacts cannot be added back
#                      without breaking the owner's signature.
#
# The protection is L0 (absence) + distribution control: the insurer is handed a
# genuinely contact-free artifact, not a full head with a polite refusal bolted on.
# (L1 — a typed "denied" at compile time — is the honest UX on top; see tier.rs.)
#
# Usage:
#   cargo build -p ros-madair-emit --bin ros-madair-emit
#   example/aonach_mor/build/responder_slices.sh [COUNT] [OUT_DIR]
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
COUNT="${1:-200}"
OUT="${2:-$REPO/target/aonach-slices}"
EMIT="${EMIT:-$REPO/target/debug/ros-madair-emit}"
BASE="https://rosmadair.example.org/aonach_mor/"
SEED="${SEED:-1904}"

[ -x "$EMIT" ] || { echo "build the emitter first: cargo build -p ros-madair-emit --bin ros-madair-emit"; exit 1; }

DATA="$OUT/data"
rm -rf "$OUT"; mkdir -p "$DATA"

echo "== 1. generate the coherent world (buildings → keyholders → orgs → coverage) =="
python3 "$HERE/emit_buildings.py"      --count           "$COUNT" --out "$DATA" >/dev/null
python3 "$HERE/emit_organisations.py"  --footprints-count "$COUNT" --seed "$SEED" --out "$DATA" >/dev/null
python3 "$HERE/emit_persons.py"        --footprints-count "$COUNT" --seed "$SEED" --out "$DATA" >/dev/null
python3 "$HERE/emit_insurance.py"      --footprints-count "$COUNT" --seed "$SEED" --out "$DATA" >/dev/null

echo "== 2. covered-building ids + the contact model graph ids =="
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
open(f"{data}/.gids", "w").write(f"{gid['Person']}\n{gid['Organisation']}\n")
print(f"   {len(buildings)} buildings, {len(covered)} covered")
PY
PERSON="$(sed -n 1p "$DATA/.gids")"; ORG="$(sed -n 2p "$DATA/.gids")"

echo "== 3. emit the RESPONDER full head =="
"$EMIT" "$DATA" "$OUT/responder_head" --base-uri "$BASE" >/dev/null

echo "== 4. emit the INSURER tiered head (covered buildings only; persons/orgs excluded) =="
"$EMIT" "$DATA" "$OUT/insurer_head" --base-uri "$BASE" \
  --tier insurer-book --include-resources "$DATA/covered_ids.txt" \
  --exclude-model "$PERSON" --exclude-model "$ORG" >/dev/null

echo "== 5. sign + verify both (one owner key) =="
KEY="$OUT/aonach_signing.key"
for head in responder_head insurer_head; do
  "$EMIT" sign "$OUT/$head" "$KEY" --actor https://flaxandteal.co.uk --actor-name "Flax & Teal" --role endorsed >/dev/null
  printf "   %-15s " "$head:"; "$EMIT" verify "$OUT/$head"
done

echo "== done =="
echo "   responder head: $OUT/responder_head"
echo "   insurer  head:  $OUT/insurer_head  (tier: insurer-book — contact-free, signed)"

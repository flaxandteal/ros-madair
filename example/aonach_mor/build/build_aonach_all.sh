#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
# © Flax & Teal Limited. Aonach Mór — the COMBINED "aonach-all" head.
#
# Generates the whole coherent world (buildings → orgs → keyholders → insurance,
# PLUS hazard models/scenarios/footprints and exposure) into one data_dir, then
# emits a single combined head with NO model exclusions — the corpus the NL→DSL
# testbed queries across every model. Companion to responder_slices.sh (which cuts
# the same data into the three signed slices).
#
# Usage:
#   cargo build -p ros-madair-emit --bin ros-madair-emit
#   example/aonach_mor/build/build_aonach_all.sh [COUNT] [OUT_DIR]
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
COUNT="${1:-200}"
OUT="${2:-$REPO/target/aonach-slices}"          # data_dir lives here (graphs + resources)
HEAD="${HEAD:-$REPO/target/aonach-all}"
EMIT="${EMIT:-$REPO/target/debug/ros-madair-emit}"
BASE="https://rosmadair.example.org/aonach_mor/"
SEED="${SEED:-1904}"

[ -x "$EMIT" ] || { echo "build the emitter first: cargo build -p ros-madair-emit --bin ros-madair-emit"; exit 1; }

DATA="$OUT/data"
rm -rf "$OUT"; mkdir -p "$DATA"

echo "== 1. generate the coherent world (all models) =="
python3 "$HERE/emit_buildings.py"      --count            "$COUNT" --out "$DATA" >/dev/null
python3 "$HERE/emit_organisations.py"  --footprints-count "$COUNT" --seed "$SEED" --out "$DATA" >/dev/null
python3 "$HERE/emit_persons.py"        --footprints-count "$COUNT" --seed "$SEED" --out "$DATA" >/dev/null
python3 "$HERE/emit_insurance.py"      --footprints-count "$COUNT" --seed "$SEED" --out "$DATA" >/dev/null
python3 "$HERE/emit_hazard.py"                             --seed "$SEED" --out "$DATA" >/dev/null
python3 "$HERE/emit_exposure.py"       --footprints-count "$COUNT" --seed "$SEED" --out "$DATA" >/dev/null
python3 "$HERE/emit_sensors.py"        --footprints-count "$COUNT" --seed "$SEED" --out "$DATA" >/dev/null
python3 "$HERE/emit_observations.py"   --footprints-count "$COUNT" --seed "$SEED" --out "$DATA" >/dev/null

echo "== 2. emit the combined head (no exclusions) =="
rm -rf "$HEAD"
"$EMIT" "$DATA" "$HEAD" --base-uri "$BASE" >/dev/null

echo "== 3. sign + verify =="
KEY="$OUT/aonach_signing.key"
"$EMIT" sign "$HEAD" "$KEY" --actor https://flaxandteal.co.uk --actor-name "Flax & Teal" --role endorsed >/dev/null
printf "   aonach-all: "; "$EMIT" verify "$HEAD"

echo "== models in the head =="
ls "$DATA/graphs"/*.json | xargs -n1 basename | sed 's/^/   /'
echo "== done -> $HEAD =="

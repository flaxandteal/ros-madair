#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# © Flax & Teal Limited. Aonach Mór — Observation synthesis (companion to emit_sensors.py).
# Emits ONE model (Observation, sosa:Observation): sensor readings — water levels, wind
# speeds, temperatures, etc. `made_by_sensor` points at a REAL Sensor rid (from the
# <out>/.sensors manifest emit_sensors wrote); `feature_of_interest` at a REAL Building
# rid. `result_value` is the reading (a number), so "high water-level observations" and
# "observations of water level" resolve on the melt substrate. A water-level reading also
# carries the matching flood intensity_band, so it coheres with the hazard model.
#
#   # run AFTER emit_sensors.py (+ emit_buildings.py)
#   python example/aonach_mor/build/emit_observations.py --footprints-count 200 --out <data_dir>
import argparse
import csv
import importlib.util
import json
import random
import uuid
from pathlib import Path

HERE = Path(__file__).resolve().parent


def _load(mod, fname):
    spec = importlib.util.spec_from_file_location(mod, HERE / fname)
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


eb = _load("emit_buildings", "emit_buildings.py")
eh = _load("emit_hazard", "emit_hazard.py")

NS = uuid.NAMESPACE_URL
OBS_GRAPHID = str(uuid.uuid5(NS, "aonach-mor/resource_models/observation"))

# observed-property -> (low, high) plausible reading range
RANGES = {
    "Water level (m)": (0.05, 4.2),
    "Wind speed (m/s)": (3.0, 58.0),
    "Temperature (°C)": (16.0, 47.0),
    "PM2.5 concentration (µg/m³)": (8.0, 220.0),
    "Head count (persons)": (5, 1800),
    "Peak ground acceleration (g)": (0.01, 0.45),
}


def flood_band(level_m):
    if level_m >= 3.0:
        return "Flood — extreme (>3.0 m)"
    if level_m >= 1.0:
        return "Flood — deep (1.0–3.0 m)"
    if level_m >= 0.3:
        return "Flood — shallow (0.3–1.0 m)"
    return "Flood — nuisance (<0.3 m)"


def generate(sensors, buildings, seed):
    rng = random.Random(seed ^ 0x0B5E7)
    out = []
    gid = OBS_GRAPHID
    k = 0
    for srid, stype, obs in sensors:
        lo, hi = RANGES.get(obs, (0.0, 1.0))
        for _ in range(rng.randint(3, 8)):
            val = round(rng.uniform(lo, hi), 2)
            rid = str(uuid.uuid5(uuid.UUID(gid), f"obs:{k}"))
            yr, mo, dy = rng.randint(2021, 2024), rng.randint(1, 12), rng.randint(1, 28)
            ts = f"{yr}-{mo:02d}-{dy:02d}"
            bld = rng.choice(buildings)

            def tile(alias, data, tag):
                return eh._tile(gid, rid, alias, data, tag, k)

            def nid(a):
                return eh._nid(gid, a)

            tiles = [
                tile("observation_id", {nid("observation_id"): f"OBS-{k:05d}"}, "id"),
                tile("observed_property", {nid("observed_property"): obs}, "op"),
                tile("made_by_sensor", {nid("made_by_sensor"): [{"resourceId": srid}]}, "bs"),
                tile("feature_of_interest", {nid("feature_of_interest"): [{"resourceId": bld}]}, "fi"),
                tile("result_value", {nid("result_value"): val}, "rv"),
                tile("result_time", {nid("result_time"): ts}, "rt"),
                tile("phenomenon_span",
                     {nid("phenomenon_start"): ts, nid("phenomenon_end"): ts}, "ps"),
            ]
            if obs == "Water level (m)":
                tiles.append(tile("intensity_band", {nid("intensity_band"): flood_band(val)}, "ib"))
            name = f"{obs} = {val} ({ts})"
            out.append(eh._resource(gid, rid, name, f"{stype} reading: {obs} = {val}.", tiles))
            k += 1
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--footprints-count", type=int, default=0)
    ap.add_argument("--seed", type=int, default=1904)
    ap.add_argument("--out", type=Path, required=True)
    a = ap.parse_args()

    sfile = a.out / ".sensors"
    if not sfile.exists():
        raise SystemExit("run emit_sensors.py first — no <out>/.sensors manifest found")
    sensors = [line.split("\t") for line in sfile.read_text().splitlines() if line]

    fp = list(csv.DictReader((eb.DOCS / "scripts/seed/footprints.csv").open()))
    replicas = int(__import__("os").environ.get("AONACH_REPLICAS", "23"))
    if a.footprints_count:
        rng = random.Random(a.seed)
        rng.shuffle(fp)
        fp = fp[:a.footprints_count]
        replicas = 1
    buildings = [res["resourceinstance"]["resourceinstanceid"]
                 for res, _ in eb.building_stream(fp, replicas, a.seed)]

    obs = generate(sensors, buildings, a.seed)
    gdir = a.out / "graphs"
    rdir = a.out / "resources" / "observation"
    gdir.mkdir(parents=True, exist_ok=True)
    rdir.mkdir(parents=True, exist_ok=True)
    (gdir / f"{OBS_GRAPHID}.json").write_text(json.dumps(
        eh.build_graph(OBS_GRAPHID, "observation", "Observation",
                       "http://www.w3.org/ns/sosa/Observation"), ensure_ascii=False))
    (rdir / "observations.json").write_text(json.dumps(
        {"business_data": {"resources": obs}}, ensure_ascii=False))

    print(f"observation_graphid={OBS_GRAPHID}")
    print(f"wrote {len(obs)} observations from {len(sensors)} sensors "
          f"({sum(1 for o in obs if 'Water level' in o['resourceinstance']['name'])} water-level readings)")


if __name__ == "__main__":
    main()

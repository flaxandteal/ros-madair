#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# © Flax & Teal Limited. Aonach Mór — Sensor synthesis (companion to emit_hazard.py).
# Emits ONE model (Sensor, sosa:Sensor): monitoring devices deployed at Sites — water-
# level gauges (flood-weighted), anemometers, seismometers, thermometers, air-quality
# monitors, people counters. `deployed_at` points at a REAL Site rid (site_rid per ward
# from emit_buildings), so "river gauges" and "sensors deployed at …" resolve on the
# substrate. Scale is FIXED (~N sensors) — decoupled from the building replica count.
#
#   # run AFTER emit_buildings.py has populated <out>/resources/building/
#   python example/aonach_mor/build/emit_sensors.py --footprints-count 200 --out <data_dir>
# writes <out>/graphs/<SENSOR_GRAPHID>.json, <out>/resources/sensor/sensors.json,
#        and <out>/.sensors (rid<TAB>type<TAB>observed-property, for emit_observations).
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


eb = _load("emit_buildings", "emit_buildings.py")   # DOCS, building_stream, site_rid
eh = _load("emit_hazard", "emit_hazard.py")          # build_graph, _tile, _resource, _nid

NS = uuid.NAMESPACE_URL
SENSOR_GRAPHID = str(uuid.uuid5(NS, "aonach-mor/resource_models/sensor"))

# (sensor type, observed-property label, weight) — water-level gauges dominate (flood demo)
TYPES = [
    ("Water-level gauge", "Water level (m)", 11),
    ("Water-level gauge", "Water level (m)", 11),
    ("Anemometer", "Wind speed (m/s)", 3),
    ("Seismometer", "Peak ground acceleration (g)", 2),
    ("Thermometer", "Temperature (°C)", 3),
    ("Air-quality monitor", "PM2.5 concentration (µg/m³)", 2),
    ("People counter", "Head count (persons)", 2),
]


def sensors_for(sites, seed, n):
    rng = random.Random(seed ^ 0x53E50)
    pool = [t for t in TYPES for _ in range(t[2])]
    out = []
    for i in range(n):
        stype, obs, _ = rng.choice(pool)
        yr = rng.randint(2015, 2022)
        out.append({
            "rid": str(uuid.uuid5(uuid.UUID(SENSOR_GRAPHID), f"sensor:{i}")),
            "i": i, "stype": stype, "obs": obs, "site": rng.choice(sites),
            "start": f"{yr}-{rng.randint(1,12):02d}-15",
            "end": f"{yr + rng.randint(3,8)}-12-31",
        })
    return out


def resource_of(s):
    gid = SENSOR_GRAPHID
    name = f"{s['stype']} #{s['i'] + 1:03d}"

    def tile(alias, data, tag):
        return eh._tile(gid, s["rid"], alias, data, tag, s["i"])

    def nid(a):
        return eh._nid(gid, a)

    tiles = [
        tile("name", {nid("name"): name}, "nm"),
        tile("sensor_type", {nid("sensor_type"): s["stype"]}, "ty"),
        tile("observes", {nid("observes"): [s["obs"]]}, "ob"),
        tile("deployed_at", {nid("deployed_at"): [{"resourceId": s["site"]}]}, "da"),
        tile("deployment_span",
             {nid("deployment_start"): s["start"], nid("deployment_end"): s["end"]}, "sp"),
        tile("notes", {nid("notes"): f"{s['stype']} observing {s['obs']}."}, "no"),
    ]
    return eh._resource(gid, s["rid"], name, f"{s['stype']} deployed in the field.", tiles)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--footprints-count", type=int, default=0)
    ap.add_argument("--seed", type=int, default=1904)
    ap.add_argument("--count", type=int, default=48)
    ap.add_argument("--out", type=Path, required=True)
    a = ap.parse_args()

    # recover the EXACT Site ids the building emit produced (same footprint slice + seed)
    fp = list(csv.DictReader((eb.DOCS / "scripts/seed/footprints.csv").open()))
    replicas = int(__import__("os").environ.get("AONACH_REPLICAS", "23"))
    if a.footprints_count:
        rng = random.Random(a.seed)
        rng.shuffle(fp)
        fp = fp[:a.footprints_count]
        replicas = 1
    sites = sorted({stub["site"] for _, stub in eb.building_stream(fp, replicas, a.seed)})
    sensors = sensors_for(sites, a.seed, a.count)

    gdir = a.out / "graphs"
    rdir = a.out / "resources" / "sensor"
    gdir.mkdir(parents=True, exist_ok=True)
    rdir.mkdir(parents=True, exist_ok=True)
    (gdir / f"{SENSOR_GRAPHID}.json").write_text(json.dumps(
        eh.build_graph(SENSOR_GRAPHID, "sensor", "Sensor", "http://www.w3.org/ns/sosa/Sensor"),
        ensure_ascii=False))
    (rdir / "sensors.json").write_text(json.dumps(
        {"business_data": {"resources": [resource_of(s) for s in sensors]}}, ensure_ascii=False))
    (a.out / ".sensors").write_text(
        "\n".join(f"{s['rid']}\t{s['stype']}\t{s['obs']}" for s in sensors) + "\n")

    print(f"sensor_graphid={SENSOR_GRAPHID}")
    print(f"wrote {len(sensors)} sensors across {len(sites)} sites "
          f"({sum(1 for s in sensors if s['stype'] == 'Water-level gauge')} water-level gauges)")


if __name__ == "__main__":
    main()

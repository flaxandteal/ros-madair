# Aonach Mór — Rós Madair Resilience Demo

A fictional city-resilience exemplar built on real Chennai geography.
Aonach Mór is Irish for "great meeting place".

## What this is

A demonstration of Rós Madair as a semantic query engine for multi-hazard
city resilience and emergency response. It shows three persona views
(emergency coordinator, resilience manager, windowed insurer) over a
single graph that chains sensor → hazard model → exposure → asset →
operator → policy.

## What is real

- **Geography**: Chennai's real coastline, rivers (Adyar, Cooum,
  Kosasthalaiyar, Buckingham Canal), iconic ward boundaries, road graph,
  building footprints — sourced from Bhuvan, Survey of India Open Series
  Maps (NGP 2021), Overture Maps (CDLA-Permissive) and OSM (ODbL)
- **Ontologies**: CIDOC-CRM, FIBO (insurance + formal business modules),
  SOSA/SSN, PROV-O, BOT, GeoSPARQL, GEM Building Taxonomy, SKOS — shipped
  in `vocab/` under their original licences
- **Model formulas**: GEM fragility curves, Holland wind-model
  coefficients, HEC-RAS-style flood depth-damage curves, Keith Still
  crowd-density fragility thresholds

## What is fictional

- **Every label**: street names, ward names, admin labels, landmarks are
  all substituted via `renaming/chennai_to_aonach_mor.json`
- **Every instance**: organisations, persons, contacts, phones, emails,
  policies, owners, keyholders, sensors, observations, hazard model runs,
  scenario events — all synthesised from real aggregate distributions,
  none matching real individuals or real operational data
- **Every scenario**: Storm Bealtaine (April), the heatwave, the
  earthquake drill — none are replays of any real event
- **Coordinate reference system**: a rebranded local transverse-Mercator
  projection, not WGS84, so no lat/long that could be reverse-geocoded
  to real Chennai addresses

## Why Chennai geography under a fictional name

The Chennai shape makes the demo recognisable and immediately useful to
Greater Chennai Corporation, TNSDMA and Chennai Smart City Mission
stakeholders. The fictional branding makes it unambiguous that the tool
is a demonstration and not an authoritative Chennai risk model.

If you are reading this because you thought it **was** an authoritative
Chennai risk model — please see the plan document at
`../RosMadair-ResilienceDemo-Plan.md` in this repo's parent directory,
which details every synthesised layer.

## Layout

```
aonach_mor/
├── README.md            # this file
├── ATTRIBUTION.md       # geographic data + ontology licences
├── renaming/            # Chennai → Aonach Mór label dictionary
├── build/               # offline build pipeline (01..15_*.py)
├── vocab/               # staged ontologies (CIDOC-CRM, FIBO, SOSA, …)
├── graphs/              # generated Arches resource models (alizarin-compatible)
│   └── resource_models/ # per-model CSVs
├── business_data/       # generated Arches business data CSVs
├── crosswalks/          # FIBO ↔ CIDOC ↔ SOSA ↔ PROV crosswalk table
├── fragility/           # GEM + Huizinga + Holland curves
├── scenarios/           # three rehearsal event definitions
└── static/              # deployable output
```

## Build pipeline

See `build/` for stages 00 through 15, run in order:

0. Ontology prep
1. Chennai geographic ingest (raw labels)
2. Apply renaming dictionary
3. Generate Arches resource model CSVs (via `arches-model` skill)
4. Buildings with GEM taxonomy attrs
5. Organisations (FIBO FormalOrganization)
6. Persons + public contacts
7. Occupancy + keyholding
8. Sensor network
9. Observations (10y synthetic time series)
10. Hazard models + footprints
11. Exposure intersections
12. Insurance policies + coverages (FIBO)
13. Emit business data CSVs per model
14. Compile to Arches business data JSON
15. Persona views + Rós Madair index + Sparnatural configs

## Licences

Chennai geographic data: Bhuvan NDSAP / NGP-2021 / CDLA-Permissive / ODbL
as applicable — see `ATTRIBUTION.md` for per-source notices.
Ontologies: CIDOC-CRM (CC-BY), FIBO (MIT for specs, CC-BY-4.0 for docs),
SOSA/SSN + PROV-O + BOT + GeoSPARQL (W3C / OGC), GEM Building Taxonomy
(GEM Foundation terms of use).
Build scripts and generated instance data: AGPL-3.0-or-later, Flax &
Teal Limited, consistent with the rest of Rós Madair.

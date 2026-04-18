# Attribution

Aonach Mór is a fictional city built on real Chennai geography and real
well-established ontologies. This file records the source, licence and
attribution requirements for every third-party artefact shipped in or
referenced by the demo.

## Geographic data (Chennai)

### Overture Maps Foundation — Buildings theme

- **Source**: https://overturemaps.org/
- **Licence**: CDLA-Permissive-2.0
- **Use**: building footprints for the iconic ward set
- **Attribution**: "© Overture Maps Foundation"

### OpenStreetMap contributors

- **Source**: https://www.openstreetmap.org/ (via Overpass API / Geofabrik)
- **Licence**: Open Data Commons Open Database License (ODbL)
- **Use**: supplementary streets, landmarks, tagged features where
  Overture coverage is thin
- **Attribution**: "© OpenStreetMap contributors"

### Bhuvan (ISRO / National Remote Sensing Centre)

- **Source**: https://bhuvan.nrsc.gov.in/
- **Licence**: National Data Sharing and Accessibility Policy (NDSAP)
- **Use**: coastline, river network, Bay of Bengal hazard atlas context
- **Attribution**: "© Indian Space Research Organisation / NRSC"

### Survey of India — Open Series Maps

- **Source**: https://onlinemaps.surveyofindia.gov.in/
- **Licence**: National Geospatial Policy 2021 (NGP-2021)
- **Use**: admin boundaries, topographic base
- **Attribution**: "© Survey of India, 2021"

## Ontologies

### CIDOC-CRM

- **Source**: http://www.cidoc-crm.org/ (v7.1.x pinned)
- **Licence**: CC-BY 4.0
- **Attribution**: "CIDOC Conceptual Reference Model, ICOM / CIDOC"

### FIBO — Financial Industry Business Ontology

- **Source**: https://github.com/edmcouncil/fibo (commit hash in
  `vocab/fibo/SOURCE.md`)
- **Licence**: MIT (specifications), CC-BY-4.0 (documentation)
- **Use**: `fibo-fnd-aap-*` (agents and people), `fibo-fnd-org-fm`
  (formal organisations), and `fibo-be-le-*` (legal entities) subsets.
  FIBO does **not** publish an insurance module — the `IND` namespace
  is "Indices and Indicators" (interest rates, FX, market indices),
  not "Industry > Insurance". Insurance concepts in Aonach Mór come
  from Riskine (see below).
- **Attribution**: "© EDM Council, FIBO working groups"

### Riskine — Global Insurance Ontology

- **Source**: https://github.com/riskine/ontology (commit hash in
  `vocab/riskine/SOURCE.md`)
- **Licence**: Apache-2.0
- **Upstream namespace**: `https://ontology.riskine.com/`
- **Use**: insurance coverage, damage, risk event, product,
  household, person, organisation and related classes. Upstream is
  published as JSON Schema; Aonach Mór ships a mechanically-generated
  OWL rendering (`vocab/riskine/riskine.rdf`) with class-scoped
  property URIs, plus a companion SKOS concept scheme for enums
  (`vocab/skos/riskine_enums.xml`). The rendering is a lossy,
  approximate projection of the JSON Schema structure into OWL,
  produced by `build/riskine_to_owl.py`, and is **not** an official
  Riskine artefact.
- **Licence of rendering**: AGPL-3.0-or-later (Flax & Teal,
  transform script) over Apache-2.0 upstream content
- **Attribution**: "© Riskine GmbH, Global Insurance Ontology"
  for upstream; "OWL rendering © 2026 Flax & Teal Limited" for the
  transform output

### SOSA / SSN — Semantic Sensor Network

- **Source**: https://www.w3.org/TR/vocab-ssn/
- **Licence**: W3C Document Licence
- **Attribution**: "© W3C, Spatial Data on the Web Working Group"

### PROV-O

- **Source**: https://www.w3.org/TR/prov-o/
- **Licence**: W3C Document Licence
- **Attribution**: "© W3C, Provenance Working Group"

### BOT — Building Topology Ontology

- **Source**: https://w3id.org/bot
- **Licence**: W3C Community Group Report
- **Attribution**: "© W3C Linked Building Data Community Group"

### GeoSPARQL

- **Source**: https://www.ogc.org/standards/geosparql
- **Licence**: OGC Document Licence
- **Attribution**: "© Open Geospatial Consortium"

### GEM Building Taxonomy v3

- **Source**: https://www.globalquakemodel.org/gem (catalogue)
- **Licence**: GEM Foundation terms of use
- **Use**: SKOS-XML subset of the taxonomy for building classification
- **Attribution**: "© GEM Foundation"

## Model formulas and fragility catalogues

### GEM / OpenQuake fragility curves

- **Source**: GEM Foundation published model catalogue
- **Licence**: see upstream terms
- **Use**: structural fragility per building class

### JRC / Huizinga global flood depth-damage curves

- **Source**: Huizinga, J., Moel, H. de, Szewczyk, W. (2017),
  JRC Technical Report
- **Licence**: Creative Commons Attribution 4.0 International
- **Attribution**: "Huizinga et al. 2017, JRC Global Flood Depth-Damage
  Functions"

### Holland wind-model coefficients

- **Source**: Holland, G.J. (1980, 2010) peer-reviewed literature
- **Licence**: public-domain formulas, implemented in-house

### Pedestrian engineering crowd-density thresholds

- **Source**: Keith Still, *Introduction to Crowd Science*, and derivative
  published research
- **Licence**: formulas as published

## Build scripts and generated data

Build scripts (`build/*.py`), generated Arches resource model CSVs
(`arches_models/`), generated business data (`business_data/`,
`static/aliz/business_data/`), and the Rós Madair index (`static/index/`)
are:

- **Licence**: AGPL-3.0-or-later
- **Copyright**: © 2026 Flax & Teal Limited
- **Consistent with**: the rest of the Rós Madair repository

All instance data (organisations, persons, policies, sensors,
observations, hazard model runs, scenario events) is synthetic and
bears no relation to any real person, organisation or operational
dataset.

## Rebranding

The Chennai → Aonach Mór label dictionary in
`renaming/chennai_to_aonach_mor.json` is a deterministic hash-based
substitution table applied at build time. It is shipped in the demo for
reviewer transparency: anyone who wants to know how a specific Aonach
Mór label maps back to the Chennai source can look it up directly.

The fictional labels themselves are generated from a morpheme table
designed not to collide with real toponyms in Tamil Nadu, India,
Ireland, or any other region.

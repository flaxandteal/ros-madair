# Riskine Global Insurance Ontology (OWL rendering)

- **Source**: https://github.com/riskine/ontology
- **Version / commit**: 4cd6876ac305e5e10adeefc400548dc6e96b0a0f
- **Licence**: Apache-2.0 (upstream) / AGPL-3.0-or-later (our OWL rendering)
- **Attribution**: © Riskine GmbH (source JSON Schema); OWL rendering © Flax & Teal Limited
- **Fetched on**: 2026-04-11
- **Fetched by**: `example/aonach_mor/build/00_ontology_prep.py`

Upstream publishes JSON Schema, not OWL. The OWL file at `riskine.rdf` is an automated, mechanical translation produced by `build/riskine_to_owl.py`. SKOS enums are published alongside the other concept collections at `../skos/riskine_enums.xml`. See the transform script for the exact mapping rules — this is an approximate rendering, not a semantic canonicalisation.

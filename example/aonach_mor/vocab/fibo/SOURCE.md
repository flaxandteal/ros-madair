# FIBO — Financial Industry Business Ontology

- **Source**: https://github.com/edmcouncil/fibo
- **Version / commit**: master_2025Q4
- **Licence**: MIT (specifications), CC-BY-4.0 (documentation)
- **Attribution**: © EDM Council, FIBO working groups
- **Fetched on**: 2026-04-11
- **Fetched by**: `example/aonach_mor/build/00_ontology_prep.py`

Staged subset:

  - `FND/AgentsAndPeople`
  - `FND/Organizations`
  - `BE/LegalEntities`

We bind `cmns-org:FormalOrganization` (reachable via FIBO's import of OMG Commons) on the Organisation resource model and `fibo-fnd-aap-ppl:Person` on the Person resource model. Transitive imports load from the same tree.

FIBO has **no** insurance module — its `IND` namespace is Indices and Indicators, not Industry>Insurance. Insurance classes in Aonach Mór come from Riskine; see `vocab/riskine/REFERENCE.md`.

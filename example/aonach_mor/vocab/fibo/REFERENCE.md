# FIBO Quick Reference for Arches Modelling (Aonach Mór subset)

Scope of this reference: the minimum set of FIBO (and imported OMG Commons)
classes and properties the Aonach Mór resilience demo binds onto the
**Organisation** and **Person** resource models. FIBO is a large ontology;
this reference intentionally excludes everything except what Stage 0 of
the build actually stages and what Stage 3 actually references.

**Not** used: FIBO has no insurance module. `fibo-ind-*` (IND) is
Indices and Indicators, not Insurance. Insurance concepts come from
Riskine, not FIBO — see `vocab/riskine/REFERENCE.md`.

## Namespace prefixes

| Prefix | URI |
|--------|-----|
| `fibo-fnd-aap-agt` | `https://spec.edmcouncil.org/fibo/ontology/FND/AgentsAndPeople/Agents/` |
| `fibo-fnd-aap-ppl` | `https://spec.edmcouncil.org/fibo/ontology/FND/AgentsAndPeople/People/` |
| `fibo-fnd-org-fm`  | `https://spec.edmcouncil.org/fibo/ontology/FND/Organizations/FormalOrganizations/` |
| `fibo-be-le-fbo`   | `https://spec.edmcouncil.org/fibo/ontology/BE/LegalEntities/FormalBusinessOrganizations/` |
| `fibo-be-le-lp`    | `https://spec.edmcouncil.org/fibo/ontology/BE/LegalEntities/LegalPersons/` |
| `fibo-be-le-lei`   | `https://spec.edmcouncil.org/fibo/ontology/BE/LegalEntities/LEIEntities/` |
| `cmns-org`         | `https://www.omg.org/spec/Commons/Organizations/` |
| `cmns-pts`         | `https://www.omg.org/spec/Commons/PartiesAndSituations/` |
| `cmns-col`         | `https://www.omg.org/spec/Commons/Collections/` |
| `cmns-loc`         | `https://www.omg.org/spec/Commons/Locations/` |

**Note:** several classes commonly thought of as "FIBO" (including
`FormalOrganization`, `LegalEntity`, `LegalPerson`, `Agent`, `Organization`)
are actually defined in the **OMG Commons** ontology library that FIBO
imports. We treat them as reachable via FIBO because they load through
FIBO's transitive imports, but the authoritative namespace is `cmns-*`.

## Core classes — Organisation side

| Class | Use for | Notes |
|-------|---------|-------|
| `cmns-org:FormalOrganization` | **Organisation root** — any organised body with a formal structure | Bound to the Aonach Mór Organisation resource model |
| `cmns-org:Organization`       | Top of the Commons organisation hierarchy | Parent of FormalOrganization |
| `cmns-org:LegalEntity`        | Anything that can hold rights and obligations | Parent class for most organisations |
| `cmns-org:LegalPerson`        | Legal entity that can act in law | Covers both natural and juridical persons |
| `fibo-fnd-org-fm:Group`       | Collection of people with a common purpose | Lighter-weight than FormalOrganization |
| `fibo-be-le-fbo:Branch`       | Subordinate location of a parent organisation | For multi-site operators |
| `fibo-be-le-fbo:Division`     | Internal sub-unit of an organisation | Not a separate legal entity |
| `fibo-be-le-fbo:JointVenture` | Co-owned operating vehicle | |
| `fibo-be-le-fbo:NonGovernmentalOrganization` | NGO | |
| `fibo-be-le-fbo:NotForProfitOrganization`    | Not-for-profit body | |
| `fibo-be-le-lp:BusinessEntity`               | Any entity engaged in business activity | Parent of most commercial organisations |
| `fibo-be-le-lp:CharteredLegalPerson`         | Entity established by charter | |
| `fibo-be-le-lp:StatutoryBody`                | Public body created by statute | Useful for municipal authorities |
| `fibo-be-le-lei:LegalEntityIdentifier`       | ISO 17442 LEI | Twenty-character identifier |

## Core classes — Person side

| Class | Use for | Notes |
|-------|---------|-------|
| `fibo-fnd-aap-ppl:Person`          | **Person root** — an individual human | Bound to the Aonach Mór Person resource model |
| `fibo-fnd-aap-ppl:Adult`           | Person at or above the age of majority | |
| `fibo-fnd-aap-ppl:Minor`           | Person below the age of majority | |
| `fibo-fnd-aap-ppl:LegallyCompetentNaturalPerson` | `fibo-be-le-lp:` — person who can act in law | |
| `fibo-fnd-aap-ppl:Contact`         | Contact point for a person | |
| `fibo-fnd-aap-ppl:PersonName`      | Structured personal name | |
| `fibo-fnd-aap-ppl:NationalIdentificationNumber` | Citizen / resident identifier | |
| `cmns-pts:Agent`                   | Anything that can act intentionally | Parent of Person and FormalOrganization |

## Organisation-side properties

| Property | Domain → Range | Use for |
|----------|---------------|---------|
| `cmns-org:hasSubUnit`                    | Organization → Organization | Hierarchical structure |
| `cmns-org:isSubUnitOf`                   | Organization → Organization | Inverse of hasSubUnit |
| `fibo-fnd-org-fm:hasEmployee`            | Employer → Employee | Who works for us |
| `fibo-fnd-org-fm:isEmployedBy`           | Employee → Employer | Who I work for |
| `fibo-fnd-org-fm:employs`                | Employer → Person   | Shortcut to Person without the Employment-reified link |
| `fibo-be-le-fbo:hasHeadquartersAddress`  | FormalOrganization → Address | HQ location |
| `fibo-be-le-fbo:hasOperatingAddress`     | FormalOrganization → Address | Operating location |
| `fibo-be-le-fbo:hasRegisteredAddress`    | FormalOrganization → Address | Registered office |
| `fibo-be-le-lei:hasLegalEntityIdentifier` | LegalEntity → LEI | LEI attachment |
| `cmns-org:hasLegalName`                  | LegalEntity → text | Registered legal name |

## Person-side properties

| Property | Domain → Range | Use for |
|----------|---------------|---------|
| `fibo-fnd-aap-ppl:hasPersonName`    | Person → PersonName | Structured name |
| `fibo-fnd-aap-ppl:hasDateOfBirth`   | Person → date | Birth date |
| `fibo-fnd-aap-ppl:hasContact`       | Person → Contact    | Contact point |

## Common Arches modelling patterns (FIBO)

### Organisation root pattern

```
Organisation ─P1_is_identified_by──────────► E41_Appellation (string: display name)
             ─cmns-org:hasLegalName─────────► (string: registered legal name)
             ─fibo-be-le-fbo:hasHeadquartersAddress─► Address (resource-instance → Site)
             ─fibo-be-le-lei:hasLegalEntityIdentifier─► (string: LEI, 20 chars)
             ─P2_has_type───────────────────► E55_Type (concept: Sector collection)
             ─cmns-org:hasSubUnit───────────► Organisation (resource-instance-list: subsidiaries)
             ─cmns-org:isSubUnitOf──────────► Organisation (resource-instance: parent)
```

### Person root pattern

```
Person ─fibo-fnd-aap-ppl:hasPersonName─► PersonName (semantic)
         PersonName ─P3_has_note──────► E62_String (string: full name)
         PersonName ─P2_has_type──────► E55_Type (concept: name type — preferred, alias, …)
       ─fibo-fnd-aap-ppl:hasContact────► Contact (semantic)
         Contact ─P3_has_note──────────► E62_String (string: email / phone / address line)
         Contact ─P2_has_type──────────► E55_Type (concept: contact method)
       ─cmns-pts:actsOnBehalfOf────────► Organisation (resource-instance)
       ─P2_has_type───────────────────► E55_Type (concept: ServiceRole)
```

## Crosswalk to CIDOC-CRM

FIBO classes can be projected into CIDOC-CRM where the demo needs heritage
queries to see them. Only the bindings actually used in the crosswalk file
are listed here.

| FIBO class | CIDOC-CRM equivalent | Notes |
|-----------|---------------------|-------|
| `cmns-org:FormalOrganization`      | `crm:E74_Group` | Organisation ↔ Group |
| `fibo-fnd-aap-ppl:Person`          | `crm:E21_Person` | Person ↔ Person |
| `cmns-pts:Agent`                   | `crm:E39_Actor`  | Agent ↔ Actor |
| `fibo-be-le-fbo:hasHeadquartersAddress` | `crm:P74_has_current_or_former_residence` | Address link |

## Out of scope for Aonach Mór v1

- FIBO SEC (Securities), DER (Derivatives), FBC (Financial Business &
  Commerce), LOAN (Loans) — not relevant to resilience or organisation
  profiling for the demo
- FIBO IND (Indices and Indicators) — not staged, no insurance content
- Employment reification, power-of-attorney, citizenship, nationality —
  not needed; a simple `hasContact` + `hasServiceRole` suffices

## Sources

Upstream: `edmcouncil/fibo` at commit hash in `SOURCE.md`. Staged modules:
`FND/AgentsAndPeople`, `FND/Organizations`, `BE/LegalEntities`. Licence:
MIT for specifications, CC-BY-4.0 for documentation.

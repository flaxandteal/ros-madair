# PROV-O Quick Reference for Arches Modelling (Aonach Mór subset)

Scope: the PROV-O classes and properties the Aonach Mór resilience demo
binds onto the **HazardModel** resource model and onto the provenance
tiles of Observation, Exposure, HazardFootprint, and ScenarioEvent.

PROV-O is the single source of truth for "who ran what model, when, from
which inputs". It is the glue that lets resilience managers distinguish
*the risk changed* from *the model changed*, and insurers audit what was
visible to them at any given point in time.

## Namespace

| Prefix | URI |
|--------|-----|
| `prov` | `http://www.w3.org/ns/prov#` |

## Core classes

### The three starting points

| Class | Use for | Notes |
|-------|---------|-------|
| `prov:Entity`   | **Any thing** whose provenance is of interest | Observations, hazard footprints, exposure records, model outputs |
| `prov:Activity` | **Any process or event** that produces or consumes entities | HazardModel runs — bound to the HazardModel resource model |
| `prov:Agent`    | **Any party** with responsibility for an activity or entity | Organisations and persons — the ones in the attribution chain |

### Specialised agents

| Class | Use for |
|-------|---------|
| `prov:Person`         | Individual human agent |
| `prov:Organization`   | Organisation agent |
| `prov:SoftwareAgent`  | Software system acting autonomously |

### Specialised activity and entity classes (optional)

| Class | Use for |
|-------|---------|
| `prov:Plan`            | Pre-existing procedure an activity implements |
| `prov:Collection`      | Entity that groups other entities |
| `prov:PrimarySource`   | Entity that is the direct source for another |
| `prov:Revision`        | Entity derived from another by revision |
| `prov:Derivation`      | Reified derivation link |
| `prov:Generation`      | Reified generation link |
| `prov:Usage`           | Reified usage link |
| `prov:Attribution`     | Reified attribution link |
| `prov:Association`     | Reified agent-to-activity link |
| `prov:Delegation`      | Agent acting on behalf of another |

Reified classes are rarely bound directly in Arches; use the shortcut
properties instead unless the demo needs to attach a timestamp or a
reason to the relationship itself.

## Core properties

### Activity-to-entity

| Property | Domain → Range | Use for |
|----------|---------------|---------|
| `prov:used`             | `prov:Activity` → `prov:Entity` | Inputs consumed |
| `prov:generated`        | `prov:Activity` → `prov:Entity` | Outputs produced |
| `prov:wasAssociatedWith` | `prov:Activity` → `prov:Agent` | Agent responsible for the activity |

### Entity-to-activity

| Property | Domain → Range | Use for |
|----------|---------------|---------|
| `prov:wasGeneratedBy`   | `prov:Entity` → `prov:Activity` | Which activity produced this entity |
| `prov:wasInvalidatedBy` | `prov:Entity` → `prov:Activity` | Which activity retired this entity |

### Entity-to-entity

| Property | Domain → Range | Use for |
|----------|---------------|---------|
| `prov:wasDerivedFrom`   | `prov:Entity` → `prov:Entity` | Source entity for this one |
| `prov:wasRevisionOf`    | `prov:Entity` → `prov:Entity` | This is a revision of that |
| `prov:hadPrimarySource` | `prov:Entity` → `prov:Entity` | Authoritative original source |
| `prov:wasQuotedFrom`    | `prov:Entity` → `prov:Entity` | Quoted from |

### Entity-to-agent

| Property | Domain → Range | Use for |
|----------|---------------|---------|
| `prov:wasAttributedTo` | `prov:Entity` → `prov:Agent` | Who is responsible for this entity |

### Agent-to-agent

| Property | Domain → Range | Use for |
|----------|---------------|---------|
| `prov:actedOnBehalfOf` | `prov:Agent` → `prov:Agent` | Delegation chain |

### Time

| Property | Domain → Range | Use for |
|----------|---------------|---------|
| `prov:generatedAtTime`  | `prov:Entity` → `xsd:dateTime` | When the entity was produced |
| `prov:invalidatedAtTime` | `prov:Entity` → `xsd:dateTime` | When the entity ceased to be valid |
| `prov:startedAtTime`    | `prov:Activity` → `xsd:dateTime` | Activity start |
| `prov:endedAtTime`      | `prov:Activity` → `xsd:dateTime` | Activity end |
| `prov:atTime`           | `prov:InstantaneousEvent` → `xsd:dateTime` | Point-in-time event |

### Influence shortcuts

| Property | Use for |
|----------|---------|
| `prov:wasInfluencedBy` | Generic influence (catch-all) |
| `prov:wasInformedBy`   | Activity-to-activity information flow |

## Common Arches modelling patterns

### HazardModel (Activity) pattern

```
HazardModel (prov:Activity)
  ─P1_is_identified_by────────► E41_Appellation (string: run name)
  ─P2_has_type────────────────► E55_Type (concept from Method collection)
  ─prov:startedAtTime─────────► (date)
  ─prov:endedAtTime───────────► (date)
  ─prov:wasAssociatedWith─────► Organisation (resource-instance: modelling agent)
  ─prov:used──────────────────► Observation (resource-instance-list: inputs)
  ─prov:generated─────────────► HazardFootprint (resource-instance-list: outputs)
```

### Entity provenance tile (on HazardFootprint / Exposure / Observation)

```
<Entity> ─prov:wasGeneratedBy─► HazardModel (resource-instance)
         ─prov:wasAttributedTo─► Organisation (resource-instance)
         ─prov:wasDerivedFrom─► <Entity> (resource-instance: source)
         ─prov:generatedAtTime─► (date)
```

### Model versioning

```
HazardFootprint_v2 ─prov:wasRevisionOf─► HazardFootprint_v1 (resource-instance)
HazardFootprint_v2 ─prov:wasGeneratedBy─► HazardModel_2024 (resource-instance)
HazardFootprint_v1 ─prov:invalidatedAtTime─► (date)
```

This is what lets resilience managers run "buildings newly at risk in
the 2024 model vs the 2022 model" as a pure graph query.

## Crosswalk

| PROV-O class | CIDOC-CRM equivalent | Notes |
|--------------|---------------------|-------|
| `prov:Activity` | `crm:E7_Activity`            | Intentional activity |
| `prov:Entity`   | `crm:E1_CRM_Entity`          | Anything |
| `prov:Agent`    | `crm:E39_Actor`              | Actor (person or group) |
| `prov:Person`   | `crm:E21_Person`             | Individual |
| `prov:Organization` | `crm:E74_Group`          | Group |

## Sources

Upstream: `https://www.w3.org/ns/prov-o-20130430` and
`https://www.w3.org/TR/prov-o/`. W3C Recommendation. Shipped under the
W3C Document Licence. Staged as `prov_o.ttl` (upstream Turtle) and
`prov_o.rdf` (converted RDF/XML).

# SOSA/SSN Quick Reference for Arches Modelling (Aonach Mór subset)

Scope: the SOSA and SSN classes and properties the Aonach Mór resilience
demo binds onto the **Sensor** and **Observation** resource models. SOSA
(Sensor, Observation, Sample, Actuator) is the lightweight core; SSN
(Semantic Sensor Network) extends it. The demo uses SOSA for nearly
everything, with a few SSN extensions for procedure / deployment context.

## Namespace prefixes

| Prefix | URI |
|--------|-----|
| `sosa` | `http://www.w3.org/ns/sosa/` |
| `ssn`  | `http://www.w3.org/ns/ssn/`  |

## Core classes

### SOSA observation stack

| Class | Use for | Notes |
|-------|---------|-------|
| `sosa:Sensor`              | **Sensor root** — physical or virtual sensor | Bound to Aonach Mór Sensor resource model |
| `sosa:Observation`         | **Observation root** — a single measurement act | Bound to Aonach Mór Observation resource model |
| `sosa:ObservableProperty`  | What is being observed (water level, PGA, …) | Typed from the ObservableProperty SKOS collection |
| `sosa:FeatureOfInterest`   | The thing the property is of | Usually a Site or Building |
| `sosa:Sample`              | A sample drawn from a feature | Rare in this demo |
| `sosa:Result`              | The value of an observation | Can be a number, quantity, or string |
| `sosa:Platform`            | Host for one or more sensors | E.g. a river gauge station |
| `sosa:Procedure`           | Method used to make the observation | E.g. "Holland wind model" |

### SSN extensions

| Class | Use for | Notes |
|-------|---------|-------|
| `ssn:System`      | A system composed of sensors / platforms | Parent of Sensor |
| `ssn:Deployment`  | Named deployment of a system / sensor    | Lifecycle grouping |
| `ssn:Property`    | Generic property of a feature           | Super-type of ObservableProperty |
| `ssn:Input`       | Procedure input                         | |
| `ssn:Output`      | Procedure output                        | |
| `ssn:Stimulus`    | Event that triggers a sensor response   | Rarely bound directly |

## Core properties

### Observation-side

| Property | Domain → Range | Use for |
|----------|---------------|---------|
| `sosa:madeObservation`     | `sosa:Sensor` → `sosa:Observation` | Sensor's observations |
| `sosa:madeBySensor`        | `sosa:Observation` → `sosa:Sensor` | Which sensor made it |
| `sosa:observedProperty`    | `sosa:Observation` → `sosa:ObservableProperty` | What was observed |
| `sosa:hasFeatureOfInterest` | `sosa:Observation` → `sosa:FeatureOfInterest` | What the observation is about |
| `sosa:hasResult`           | `sosa:Observation` → `sosa:Result` | Measured value |
| `sosa:resultTime`          | `sosa:Observation` → `xsd:dateTime` | When the result was obtained |
| `sosa:phenomenonTime`      | `sosa:Observation` → `xsd:dateTime` | When the phenomenon occurred (may differ from resultTime) |
| `sosa:usedProcedure`       | `sosa:Observation` → `sosa:Procedure` | Method used |

### Sensor / platform-side

| Property | Domain → Range | Use for |
|----------|---------------|---------|
| `sosa:observes`       | `sosa:Sensor` → `sosa:ObservableProperty` | What the sensor can observe |
| `sosa:hosts`          | `sosa:Platform` → `ssn:System`  | What a platform hosts |
| `sosa:isHostedBy`     | `ssn:System` → `sosa:Platform`  | Inverse of hosts |
| `sosa:hasSample`      | `sosa:FeatureOfInterest` → `sosa:Sample` | Sampling relation |
| `sosa:isSampleOf`     | `sosa:Sample` → `sosa:FeatureOfInterest` | Inverse |

### SSN-side

| Property | Domain → Range | Use for |
|----------|---------------|---------|
| `ssn:hasProperty`     | `sosa:FeatureOfInterest` → `ssn:Property` | Feature's inherent properties |
| `ssn:isPropertyOf`    | `ssn:Property` → `sosa:FeatureOfInterest` | Inverse |
| `ssn:hasInput`        | `sosa:Procedure` → `ssn:Input`  | Procedure input |
| `ssn:hasOutput`       | `sosa:Procedure` → `ssn:Output` | Procedure output |
| `ssn:implementedBy`   | `sosa:Procedure` → `ssn:System` | Implementing system |
| `ssn:inDeployment`    | `sosa:Platform` → `ssn:Deployment` | Deployment context |
| `ssn:deployedSystem`  | `ssn:Deployment` → `ssn:System` | System deployed |

## Common Arches modelling patterns

### Sensor root pattern

```
Sensor ─P1_is_identified_by──────► E41_Appellation (string: deployment name)
       ─sosa:observes────────────► E55_Type (concept from ObservableProperty collection)
       ─P2_has_type──────────────► E55_Type (concept from SensorType collection)
       ─sosa:isHostedBy──────────► Platform/Site (resource-instance)
       ─prov:wasAttributedTo─────► Organisation (resource-instance: operator)
       ─sosa:hasSample───────────► (geojson-feature-collection: location)
       ─ssn:inDeployment─────────► E55_Type (concept: deployment state)
```

### Observation pattern

```
Observation ─sosa:madeBySensor────────► Sensor (resource-instance)
            ─sosa:observedProperty────► E55_Type (concept from ObservableProperty)
            ─sosa:hasFeatureOfInterest─► Site / Building (resource-instance)
            ─sosa:resultTime─────────► (date)
            ─sosa:hasResult──────────► E54_Dimension (semantic)
              E54 ─P90_has_value─────► (number)
              E54 ─P91_has_unit──────► E58_Measurement_Unit (concept)
            ─sosa:usedProcedure──────► HazardModel/Procedure (resource-instance)
```

The demo flattens `sosa:hasResult` into a `Dimension`-style tile
because the concrete value carries a unit and a band. A pure
`xsd:decimal` result with a unit concept is acceptable for v1.

## Crosswalk

| SOSA/SSN class | CIDOC-CRM equivalent | PROV-O equivalent |
|-----------------|---------------------|-------------------|
| `sosa:Sensor`       | `crm:E22_Human-Made_Object` | — |
| `sosa:Observation`  | `crm:E13_Attribute_Assignment` | `prov:Entity` |
| `sosa:Procedure`    | `crm:E29_Design_or_Procedure` | `prov:Plan` |
| `sosa:FeatureOfInterest` | `crm:E1_CRM_Entity` | — |

## Sources

Upstream: `https://www.w3.org/ns/sosa/` and `https://www.w3.org/ns/ssn/`.
W3C Recommendation. Shipped under the W3C Document Licence. Staged as
`sosa_ssn.ttl` (upstream Turtle) and `sosa_ssn.rdf` (converted RDF/XML).

# Riskine Quick Reference for Arches Modelling (Aonach Mór subset)

Scope of this reference: the insurance classes and class-scoped properties
that the Aonach Mór resilience demo binds onto the **InsurancePolicy**,
**InsuranceCoverage**, **Damage**, and **Risk** resource models.

Riskine is the Global Insurance Ontology maintained by Riskine GmbH.
Upstream publishes it as **JSON Schema**, not OWL. The Stage 0 build
(`build/riskine_to_owl.py`) mechanically renders the core schemas into
OWL with class-scoped property URIs. Everything here reflects the **OWL
rendering** as it exists in `vocab/riskine/riskine.rdf`, not the upstream
JSON Schema.

## Namespace prefixes

| Prefix | URI |
|--------|-----|
| `riskine`       | `https://rosmadair.example.org/riskine/` |
| `riskine-enum`  | `https://rosmadair.example.org/riskine/enum/` |

The `riskine` namespace is a Rós Madair local — it is **not** the upstream
Riskine namespace (`https://ontology.riskine.com/`). Each OWL class carries
a `dcterms:source` triple pointing back at the upstream JSON Schema URL,
so reviewers can trace any binding to the JSON Schema it was rendered
from.

## Property naming convention

All properties are **class-scoped**: `{ClassName}_{camelField}`. For
example, `Coverage_sumInsured`, `Damage_causedBy`, `Product_policyNumber`.
This avoids the usual collision bookkeeping that global property names
would need — upstream JSON Schema uses short field names per class
(`sum-insured`, `caused-by`, `policy-number`) that would collide across
classes if flattened.

## Core classes (26)

All 26 core classes from `schemas/core/*.json`. Each declares
`rdfs:isDefinedBy https://rosmadair.example.org/riskine/` and
`dcterms:source https://ontology.riskine.com/{stem}.json`.

| Class | Use for | Notes |
|-------|---------|-------|
| `riskine:Product`         | **Policy root** — a concrete insurance product / contract | Upstream name is `Product`; carries policy number, premium, start date, coverages |
| `riskine:Coverage`        | **Coverage root** — a single coverage line on a product   | `Coverage_sumInsured`, `Coverage_deductible`, `Coverage_covers → Damage` |
| `riskine:Damage`          | **Damage root** — a loss / claim event                    | Affected persons and objects, caused-by link to `Risk` |
| `riskine:Risk`            | Risk event driving probability and damage                 | `Risk_leadsTo → Damage`, `Risk_involves → Object` |
| `riskine:Person`          | Natural person (policyholder, affected, contact)          | Upstream has 50+ fields; bind only what the demo needs |
| `riskine:Organization`    | Legal entity (policyholder, insurer, employer)            | `Organization_businessName`, `Organization_industry`, `Organization_sites` |
| `riskine:Object`          | Any insured or affected object                            | Super-type of Vehicle, Animal, Structure; generic value + owners |
| `riskine:Property`        | Property holding (land + structure bundle)                | Distinct from OWL's `owl:Property` — the insurance sense |
| `riskine:Structure`       | Building or fixed structure                               | Riskine's nearest analogue to "insured building" |
| `riskine:Site`            | Business site / location                                  | Organisational operating locations |
| `riskine:Address`         | Postal address                                            | Shared across persons, organisations, sites |
| `riskine:Vehicle`         | Motor vehicle                                             | `Object` subtype |
| `riskine:Animal`          | Animal asset (pet, livestock)                             | `Object` subtype |
| `riskine:Identification`  | Identity document                                         | |
| `riskine:DrivingLicense`  | Driving licence                                           | Extends Identification |
| `riskine:Employee`        | Employment relationship                                   | |
| `riskine:Profession`      | Professional qualification                                | |
| `riskine:Education`       | Educational qualification                                 | |
| `riskine:Finances`        | Financial position snapshot                               | |
| `riskine:BankAccount`     | Bank account                                              | |
| `riskine:CreditCard`      | Credit card                                               | |
| `riskine:Revenue`         | Revenue stream                                            | |
| `riskine:BusinessProcess` | Named business process                                    | |
| `riskine:DataProcessing`  | Data processing activity (GDPR-shaped)                    | |
| `riskine:SecurityMeasure` | Security control applied to an object or site            | |
| `riskine:Preference`      | Customer preference                                       | |

## Core properties — Coverage

All properties are class-scoped (prefix `Coverage_`).

| Property | Kind → Range | Use for |
|----------|--------------|---------|
| `Coverage_sumInsured`             | datatype → xsd:decimal | Insured sum |
| `Coverage_deductible`             | datatype → xsd:decimal | Policyholder deductible |
| `Coverage_isIncluded`             | datatype → xsd:boolean | Whether the coverage is active on this product |
| `Coverage_amountPerInterval`      | datatype → xsd:decimal | Payout amount per interval |
| `Coverage_waitingTime`            | datatype → xsd:nonNegativeInteger | Waiting days before coverage starts |
| `Coverage_paymentPeriod`          | datatype → xsd:nonNegativeInteger | How long payments are made |
| `Coverage_reimbursementAmount`    | datatype → xsd:decimal | Absolute reimbursement cap |
| `Coverage_reimbursementPercentage` | datatype → xsd:decimal | Percentage cap (0..100) |
| `Coverage_annualLimitFactor`      | datatype → xsd:integer | Multiplier on annual limit |
| `Coverage_containedIn`            | object → `riskine:Product` | Parent product |
| `Coverage_covers`                 | object → `riskine:Damage` | Damage type covered |
| `Coverage_paymentInterval`        | object → `riskine-enum:TemporalInterval` | Enum: daily / monthly / annually / … |
| `Coverage_eligibilityPeriod`      | object → `riskine-enum:TemporalInterval` | Enum: eligibility window |

## Core properties — Damage

| Property | Kind → Range | Use for |
|----------|--------------|---------|
| `Damage_onetimeAmount`   | datatype → xsd:decimal | Single-payment loss value |
| `Damage_recurringAmount` | datatype → xsd:decimal | Recurring loss amount |
| `Damage_date`            | datatype → xsd:date    | Date of damage event |
| `Damage_affectedPersons` | object → `riskine:Person` | Persons harmed |
| `Damage_affectedObjects` | object → `riskine:Object` | Objects affected |
| `Damage_causedBy`        | object → `riskine:Risk`   | Risk event that caused the damage |
| `Damage_coveredBy`       | object → `riskine:Coverage` | Which coverage line pays out |

## Core properties — Risk

| Property | Kind → Range | Use for |
|----------|--------------|---------|
| `Risk_probability` | datatype → xsd:decimal | Yearly probability |
| `Risk_isRelevant`  | datatype → xsd:boolean | Whether the risk applies to this context |
| `Risk_leadsTo`     | object → `riskine:Damage` | Resulting damage type |
| `Risk_involves`    | object → `riskine:Object` | Objects the risk acts on |
| `Risk_causer`      | object → `riskine:Person` | Causer (for liability-shaped risks) |

## Core properties — Product

| Property | Kind → Range | Use for |
|----------|--------------|---------|
| `Product_policyNumber`            | datatype → xsd:string  | Contract identifier |
| `Product_premiumAmount`           | datatype → xsd:decimal | Premium amount |
| `Product_startDate`               | datatype → xsd:date    | Coverage start |
| `Product_anniversaryDate`         | datatype → xsd:date    | Annual renewal anchor |
| `Product_termTime`                | datatype → xsd:integer | Term length |
| `Product_cancellationPeriod`      | datatype → xsd:integer | Cancellation notice period |
| `Product_automaticRenewal`        | datatype → xsd:boolean | Rolls over automatically |
| `Product_deductible`              | datatype → xsd:decimal | Product-level deductible |
| `Product_policyLanguage`          | datatype → xsd:string  | Contract language |
| `Product_package`                 | datatype → xsd:string  | Package / tariff identifier |
| `Product_coverages`               | object → `riskine:Coverage` | Coverage lines on the product |
| `Product_policyholder`            | object → `riskine:Person` or `riskine:Organization` | Who holds the contract |
| `Product_contributor`             | object → `riskine:Person` | Co-contributor |
| `Product_premiumPaymentInterval`  | object → `riskine-enum:TemporalInterval` | Enum |
| `Product_premiumPaymentMethod`    | object → `riskine-enum:PaymentMethod`    | Enum |
| `Product_areaOfValidity`          | object → `riskine-enum:Area`             | Enum |

## Core properties — Organization (insurer / insured)

Partial — only what the demo binds. Full list is in `riskine.rdf`.

| Property | Kind → Range | Use for |
|----------|--------------|---------|
| `Organization_businessName`    | datatype → xsd:string  | Trading name |
| `Organization_foundingDate`    | datatype → xsd:date    | Incorporation date |
| `Organization_companyRegistryNumber` | datatype → xsd:string | Registry ID |
| `Organization_industry`        | datatype → xsd:string  | Industry code (free-text) |
| `Organization_economicActivity` | datatype → xsd:string | NACE / ISIC-shaped |
| `Organization_legalForm`       | datatype → xsd:string  | Legal form string |
| `Organization_address`         | object → `riskine:Address`    | Registered / operating address |
| `Organization_sites`           | object → `riskine:Site`       | Operating sites |
| `Organization_employs`         | object → `riskine:Employee`   | Employees |
| `Organization_revenue`         | object → `riskine:Revenue`    | Revenue facts |
| `Organization_securityMeasure` | object → `riskine:SecurityMeasure` | Risk controls |

## Enum classes (32 selected)

Every enum in `definitions.json` becomes both an `owl:Class` in the OWL
file **and** a `skos:ConceptScheme` in `vocab/skos/riskine_enums.xml`
with matching URIs. Enum members live under
`riskine-enum:{EnumName}/{slug}`.

Key enums the demo uses:

| Enum | Typical values | Use in |
|------|----------------|--------|
| `riskine-enum:TemporalInterval` | daily, weekly, monthly, quarterly, annually, every 2 years, every 3 years, bi-annually, not specified, one-off | Coverage + Product intervals |
| `riskine-enum:Gender`            | male, female, other | Person |
| `riskine-enum:PaymentMethod`     | direct-debit, bank-transfer, card, cash | Product |
| `riskine-enum:Area`              | local, national, europe, global | Product area of validity |
| `riskine-enum:LegalForm`         | upstream list of legal forms | Organization |
| `riskine-enum:RiskLevel`         | low, medium, high | Risk banding |
| `riskine-enum:ContractType`      | upstream list of contract types | Product |

See `vocab/skos/riskine_enums.xml` for the full 32 schemes (208 concepts).

## Common Arches modelling patterns (Riskine)

### InsurancePolicy (Product) pattern

```
InsurancePolicy ─riskine:Product_policyNumber────► (string)
                ─riskine:Product_premiumAmount───► (number)
                ─riskine:Product_startDate───────► (date)
                ─riskine:Product_policyholder───► Organisation (resource-instance)
                ─riskine:Product_coverages──────► InsuranceCoverage (resource-instance-list)
                ─riskine:Product_premiumPaymentInterval──► E55_Type (concept from riskine-enum:TemporalInterval)
                ─P1_is_identified_by────────────► E41_Appellation (string: human label)
                ─P2_has_type────────────────────► E55_Type (concept: CoverageType)
```

### InsuranceCoverage pattern

```
InsuranceCoverage ─riskine:Coverage_sumInsured────► (number)
                  ─riskine:Coverage_deductible───► (number)
                  ─riskine:Coverage_isIncluded──► (boolean)
                  ─riskine:Coverage_containedIn─► InsurancePolicy (resource-instance)
                  ─riskine:Coverage_covers──────► Damage (resource-instance) [or a concept describing damage type]
                  ─riskine:Coverage_paymentInterval─► E55_Type (concept from riskine-enum:TemporalInterval)
                  ─P2_has_type──────────────────► E55_Type (concept: LimitBand)
```

### Damage → Building link

Riskine doesn't bind buildings directly — `Coverage_covers` points at
`Damage`, and `Damage_affectedObjects` points at `Object`. For the demo
we project buildings onto `riskine:Object`:

```
Damage ─riskine:Damage_affectedObjects─► Building (resource-instance-list)
       ─riskine:Damage_causedBy───────► ScenarioEvent / HazardModel (resource-instance)
       ─riskine:Damage_date───────────► (date)
       ─riskine:Damage_onetimeAmount──► (number)
```

This is the chain the insurer view queries: `Product → Coverage → Damage
→ affectedObjects (Building) → Exposure`.

## Crosswalk to CIDOC-CRM and FIBO

| Riskine class | CIDOC-CRM equivalent | FIBO equivalent |
|---------------|---------------------|-----------------|
| `riskine:Product`      | `crm:E73_Information_Object` | — (FIBO has no policy class in the staged subset) |
| `riskine:Coverage`     | `crm:E73_Information_Object` | — |
| `riskine:Damage`       | `crm:E5_Event`               | — |
| `riskine:Organization` | `crm:E74_Group`              | `cmns-org:FormalOrganization` |
| `riskine:Person`       | `crm:E21_Person`             | `fibo-fnd-aap-ppl:Person` |
| `riskine:Object`       | `crm:E22_Human-Made_Object`  | — |
| `riskine:Address`      | `crm:E45_Address`            | — |

## Out of scope for Aonach Mór v1

- `riskine:BankAccount`, `riskine:CreditCard`, `riskine:Finances`,
  `riskine:Revenue` — financial detail not needed for exposure
  queries
- `riskine:Preference`, `riskine:DataProcessing` — customer marketing
  and GDPR axes not relevant to the demo
- Riskine `schemas/reference/**/*.json` — those are dotted-path form
  projections, not class definitions; the transform ignores them

## Sources

Upstream: `riskine/ontology` at commit hash in `SOURCE.md`. Licence:
Apache-2.0 (upstream), AGPL-3.0-or-later (Flax & Teal OWL rendering).
The rendering is **not** an official Riskine artefact.

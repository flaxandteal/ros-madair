#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 Flax & Teal Limited
"""Automated transform of riskine/ontology (JSON Schema, Apache-2.0) → OWL + SKOS.

Upstream Riskine ships as JSON Schema, not OWL. This script reads the core
schemas and emits:

  - An OWL 2 ontology in RDF/XML (classes, datatype + object properties)
  - A SKOS XML file for the 36 enums defined in definitions.json

The transform is deliberately *approximate* — mechanical rules, no
per-schema judgement — so it can be re-run against any Riskine commit
without manual intervention. Specifically:

  - File stems in schemas/core/ map 1:1 to owl:Classes named in PascalCase
    (coverage.json → riskine:Coverage).
  - definitions.json is *not* a class; it is a symbol table of primitives
    and enums.
  - schemas/reference/**/*.json are *forms* (dotted-path projections onto
    the core class graph), not class definitions. They are ignored.
  - Properties are class-scoped in the emitted RDF:
      riskine:Coverage_sumInsured, riskine:Person_firstName
    Avoids any shared-property disambiguation logic.
  - Arrays are treated identically to scalars (OWL has no list primitive;
    cardinality constraints are not worth the complexity for a demo).
  - $ref into definitions.json#/<name> is resolved against a small
    primitive-to-xsd table. Unknown primitives fall back to xsd:string.
  - $ref into an enum (definitions.json#/<enum>) emits an object property
    whose range is an owl:Class with the same URI as the SKOS
    ConceptScheme, so type-checking and label lookup both work.
  - $ref into another core schema emits an object property whose range is
    that schema's PascalCase class URI.
  - rdfs:label is the space-separated title-case of the property / class
    name. rdfs:comment is the JSON Schema `description` field if present.

Outputs:

  <out-owl>   default: vocab/riskine/riskine.rdf
  <out-skos>  default: vocab/skos/riskine_enums.xml

Usage:

  python example/aonach_mor/build/riskine_to_owl.py \\
      --source example/aonach_mor/vocab/riskine/source \\
      --out-owl example/aonach_mor/vocab/riskine/riskine.rdf \\
      --out-skos example/aonach_mor/vocab/skos/riskine_enums.xml \\
      --upstream-commit <sha>

The `--upstream-commit` value is stamped into the ontology header so the
provenance chain from upstream → RDF is always reconstructable.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import json
import re
import sys
from pathlib import Path
from typing import Any, Iterable
from xml.sax.saxutils import escape as xml_escape

# --------------------------------------------------------------------------- #
# Namespaces + constants                                                      #
# --------------------------------------------------------------------------- #

NS_RISKINE = "https://rosmadair.example.org/riskine/"
NS_ENUM = "https://rosmadair.example.org/riskine/enum/"
NS_UPSTREAM = "https://ontology.riskine.com/"
UPSTREAM_REPO = "https://github.com/riskine/ontology"
UPSTREAM_LICENCE = "https://www.apache.org/licenses/LICENSE-2.0"

# JSON Schema 'type' → xsd datatype (short form for registry lookup).
PRIMITIVE_TO_XSD = {
    "string": "xsd:string",
    "integer": "xsd:integer",
    "number": "xsd:decimal",
    "boolean": "xsd:boolean",
}

# definitions.json entries treated as primitive datatypes (not enums, not
# nested class refs). The value is an (xsd short form, free-text note) pair;
# the note is appended to rdfs:comment so the transformation is traceable.
DEF_PRIMITIVE: dict[str, tuple[str, str]] = {
    "string": ("xsd:string", ""),
    "integer": ("xsd:integer", ""),
    "integer:": ("xsd:integer", ""),  # definitions.json has a typo key
    "number": ("xsd:decimal", ""),
    "boolean": ("xsd:boolean", ""),
    "money": ("xsd:decimal", "currency amount (upstream format=money)"),
    "percentage": ("xsd:decimal", "percentage (0..100)"),
    "date": ("xsd:date", ""),
    "year": ("xsd:gYear", ""),
    "nonnegative": ("xsd:decimal", "nonnegative real number"),
    "nonnegative-integer": ("xsd:nonNegativeInteger", ""),
    "days-occupied": ("xsd:nonNegativeInteger", "days in [0,365]"),
    "temporal-interval": ("xsd:string", "temporal interval (upstream enum)"),
}


# --------------------------------------------------------------------------- #
# Helpers                                                                     #
# --------------------------------------------------------------------------- #


def _log(msg: str) -> None:
    print(f"[riskine_to_owl] {msg}", flush=True)


def _die(msg: str, code: int = 1) -> None:
    print(f"[riskine_to_owl] ERROR: {msg}", file=sys.stderr, flush=True)
    sys.exit(code)


def camel(s: str) -> str:
    """kebab-case / snake_case / dotted → camelCase."""
    parts = re.split(r"[-_.]+", s)
    parts = [p for p in parts if p]
    if not parts:
        return "value"
    return parts[0].lower() + "".join(p[:1].upper() + p[1:] for p in parts[1:])


def pascal(s: str) -> str:
    """kebab-case / snake_case → PascalCase."""
    parts = re.split(r"[-_.]+", s)
    parts = [p for p in parts if p]
    return "".join(p[:1].upper() + p[1:] for p in parts)


def humanise(s: str) -> str:
    """kebab-case → 'space separated', no casing change."""
    return re.sub(r"[-_.]+", " ", s).strip()


def _xe(s: str | None) -> str:
    """XML-escape, None-safe."""
    return xml_escape(s or "")


# --------------------------------------------------------------------------- #
# Load + classify schemas                                                     #
# --------------------------------------------------------------------------- #


def load_definitions(core_dir: Path) -> tuple[dict[str, Any], dict[str, Any]]:
    """Return (primitives, enums) partitioned from definitions.json."""
    with (core_dir / "definitions.json").open() as fh:
        data = json.load(fh)
    data.pop("$id", None)
    primitives, enums = {}, {}
    for name, spec in data.items():
        if not isinstance(spec, dict):
            continue
        if "enum" in spec:
            enums[name] = spec
        else:
            primitives[name] = spec
    return primitives, enums


def load_core_classes(core_dir: Path) -> dict[str, dict[str, Any]]:
    """Return {file_stem: {filename, upstream_id, properties}} for every
    schemas/core/*.json except definitions.json."""
    classes: dict[str, dict[str, Any]] = {}
    for f in sorted(core_dir.glob("*.json")):
        if f.name == "definitions.json":
            continue
        with f.open() as fh:
            data = json.load(fh)
        if not isinstance(data, dict):
            continue
        classes[f.stem] = {
            "filename": f.name,
            "upstream_id": data.get("$id", f"{NS_UPSTREAM}{f.name}"),
            "properties": data.get("properties", {}),
        }
    return classes


# --------------------------------------------------------------------------- #
# Property-spec resolver                                                      #
# --------------------------------------------------------------------------- #


def resolve_ref(
    ref: str, defs_primitives: dict, defs_enums: dict
) -> tuple[str, str, str]:
    """Resolve a $ref to (kind, range_uri_fragment, extra_comment).

    kind ∈ {"datatype", "object", "enum"}.

    The range_uri_fragment is a CURIE-style short form understood by the
    emit layer (e.g. "xsd:decimal", "riskine:Person",
    "riskine-enum:Gender").
    """
    # Forms:
    #   "definitions.json#/money"         ← cross-file ref (core schemas)
    #   "#/definitions/money"             ← local ref    (reference schemas)
    #   "person.json"                     ← whole-schema ref
    if not ref:
        return "datatype", "xsd:string", "unresolved empty $ref"

    if ref.endswith(".json"):
        stem = ref[:-5]
        return "object", f"riskine:{pascal(stem)}", ""

    if "#" in ref:
        target, fragment = ref.split("#", 1)
        fragment = fragment.lstrip("/")
        # Accept '/definitions/money' and '/money' equivalently.
        parts = [p for p in fragment.split("/") if p and p != "definitions"]
        name = parts[-1] if parts else ""
        if not name:
            return "datatype", "xsd:string", "unresolved $ref (no name)"
        # When target is something other than definitions.json (we only
        # process core schemas and definitions lives there), fall through
        # as a definitions.json lookup anyway — all known $ refs with a
        # fragment go into definitions.
        if name in defs_primitives:
            xsd, note = DEF_PRIMITIVE.get(
                name, ("xsd:string", f"riskine primitive '{name}'")
            )
            return "datatype", xsd, note
        if name in defs_enums:
            return "enum", f"riskine-enum:{pascal(name)}", ""
        # Best-effort fallback if the primitive name is one of the
        # json-schema native primitives.
        if name in DEF_PRIMITIVE:
            xsd, note = DEF_PRIMITIVE[name]
            return "datatype", xsd, note
        return "datatype", "xsd:string", f"unknown definitions entry '{name}'"

    # Bare identifier — shouldn't happen in Riskine but handle gracefully.
    return "datatype", "xsd:string", f"unrecognised $ref form '{ref}'"


def resolve_property(
    spec: dict,
    defs_primitives: dict,
    defs_enums: dict,
) -> tuple[str, str, str]:
    """(kind, range_short, extra_comment) for a JSON-Schema property spec.

    Array wrappers are unwrapped recursively. `type: object` (inline) is
    treated as xsd:string because the demo never instantiates inline
    objects — they appear only in the reference/ forms we skip."""
    if not isinstance(spec, dict):
        return "datatype", "xsd:string", "malformed property spec"

    # Unwrap arrays.
    if spec.get("type") == "array":
        items = spec.get("items", {})
        return resolve_property(items, defs_primitives, defs_enums)

    if "$ref" in spec:
        return resolve_ref(spec["$ref"], defs_primitives, defs_enums)

    t = spec.get("type")
    if t in PRIMITIVE_TO_XSD:
        return "datatype", PRIMITIVE_TO_XSD[t], ""
    if t == "object":
        return "datatype", "xsd:string", "inline object (collapsed to string)"

    return "datatype", "xsd:string", "no type/ref in property spec"


# --------------------------------------------------------------------------- #
# Emitters                                                                    #
# --------------------------------------------------------------------------- #


def _curie_to_uri(curie: str) -> str:
    if curie.startswith("xsd:"):
        return "http://www.w3.org/2001/XMLSchema#" + curie[4:]
    if curie.startswith("riskine-enum:"):
        return NS_ENUM + curie.split(":", 1)[1]
    if curie.startswith("riskine:"):
        return NS_RISKINE + curie.split(":", 1)[1]
    return curie


def render_owl(
    classes: dict,
    defs_primitives: dict,
    defs_enums: dict,
    upstream_commit: str,
) -> str:
    today = _dt.date.today().isoformat()
    out: list[str] = []
    out.append('<?xml version="1.0" encoding="UTF-8"?>')
    out.append(
        '<rdf:RDF\n'
        '  xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"\n'
        '  xmlns:rdfs="http://www.w3.org/2000/01/rdf-schema#"\n'
        '  xmlns:owl="http://www.w3.org/2002/07/owl#"\n'
        '  xmlns:xsd="http://www.w3.org/2001/XMLSchema#"\n'
        '  xmlns:dcterms="http://purl.org/dc/terms/"\n'
        '  xmlns:skos="http://www.w3.org/2004/02/skos/core#"\n'
        f'  xmlns:riskine="{NS_RISKINE}"\n'
        f'  xmlns:riskine-enum="{NS_ENUM}">'
    )
    # Ontology header.
    out.append(f'  <owl:Ontology rdf:about="{NS_RISKINE}">')
    out.append(
        '    <rdfs:label xml:lang="en">Riskine Insurance Ontology '
        "(OWL rendering)</rdfs:label>"
    )
    out.append(
        '    <rdfs:comment xml:lang="en">Automated OWL rendering of the '
        f"upstream Riskine JSON Schema ontology, pinned at commit "
        f"{_xe(upstream_commit)}. Generated by "
        "example/aonach_mor/build/riskine_to_owl.py as part of the Rós "
        "Madair Aonach Mór demo. This is an approximate, mechanical "
        "translation — see the script header for the mapping rules.</rdfs:comment>"
    )
    out.append(f'    <dcterms:source rdf:resource="{UPSTREAM_REPO}"/>')
    out.append(
        f'    <dcterms:license rdf:resource="{UPSTREAM_LICENCE}"/>'
    )
    out.append(f'    <dcterms:modified>{today}</dcterms:modified>')
    out.append("  </owl:Ontology>")

    # Enum declarations — one owl:Class per enum. Membership + labels
    # live in the SKOS file; this is just the type declaration so
    # rdfs:range references resolve.
    for enum_name in sorted(defs_enums):
        uri = NS_ENUM + pascal(enum_name)
        out.append(f'  <owl:Class rdf:about="{uri}">')
        out.append(
            f'    <rdfs:label xml:lang="en">{_xe(humanise(enum_name))}</rdfs:label>'
        )
        out.append(
            '    <rdfs:comment xml:lang="en">Enum class. Members are '
            "published as skos:Concepts in the companion SKOS file."
            "</rdfs:comment>"
        )
        out.append(f'    <rdfs:isDefinedBy rdf:resource="{NS_RISKINE}"/>')
        out.append("  </owl:Class>")

    # Core classes.
    for stem in sorted(classes):
        cls = classes[stem]
        class_uri = NS_RISKINE + pascal(stem)
        out.append(f'  <owl:Class rdf:about="{class_uri}">')
        out.append(
            f'    <rdfs:label xml:lang="en">{_xe(humanise(stem))}</rdfs:label>'
        )
        out.append(
            '    <rdfs:comment xml:lang="en">Automatically translated '
            f"from upstream {_xe(cls['filename'])}.</rdfs:comment>"
        )
        out.append(
            f'    <dcterms:source rdf:resource="{_xe(cls["upstream_id"])}"/>'
        )
        out.append(f'    <rdfs:isDefinedBy rdf:resource="{NS_RISKINE}"/>')
        out.append("  </owl:Class>")

    # Properties — class-scoped to sidestep collision bookkeeping.
    for stem in sorted(classes):
        cls = classes[stem]
        domain_uri = NS_RISKINE + pascal(stem)
        for prop_name, prop_spec in cls["properties"].items():
            kind, range_short, note = resolve_property(
                prop_spec, defs_primitives, defs_enums
            )
            scoped = f"{pascal(stem)}_{camel(prop_name)}"
            prop_uri = NS_RISKINE + scoped
            range_uri = _curie_to_uri(range_short)
            element = (
                "owl:DatatypeProperty"
                if kind == "datatype"
                else "owl:ObjectProperty"
            )
            out.append(f'  <{element} rdf:about="{prop_uri}">')
            out.append(
                f'    <rdfs:label xml:lang="en">'
                f"{_xe(humanise(prop_name))}</rdfs:label>"
            )
            desc = (
                prop_spec.get("description", "")
                if isinstance(prop_spec, dict)
                else ""
            )
            comment_bits: list[str] = []
            if desc:
                comment_bits.append(desc)
            if note:
                comment_bits.append(f"[{note}]")
            if isinstance(prop_spec, dict) and prop_spec.get("type") == "array":
                comment_bits.append("[upstream: array]")
            if comment_bits:
                out.append(
                    '    <rdfs:comment xml:lang="en">'
                    f"{_xe(' '.join(comment_bits))}</rdfs:comment>"
                )
            out.append(f'    <rdfs:domain rdf:resource="{domain_uri}"/>')
            out.append(f'    <rdfs:range rdf:resource="{range_uri}"/>')
            out.append(f'    <rdfs:isDefinedBy rdf:resource="{NS_RISKINE}"/>')
            out.append(f"  </{element}>")

    out.append("</rdf:RDF>")
    out.append("")
    return "\n".join(out)


def render_skos(defs_enums: dict, upstream_commit: str) -> str:
    today = _dt.date.today().isoformat()
    out: list[str] = []
    out.append('<?xml version="1.0" encoding="UTF-8"?>')
    out.append(
        '<rdf:RDF\n'
        '  xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"\n'
        '  xmlns:rdfs="http://www.w3.org/2000/01/rdf-schema#"\n'
        '  xmlns:skos="http://www.w3.org/2004/02/skos/core#"\n'
        '  xmlns:dcterms="http://purl.org/dc/terms/">'
    )
    out.append(
        "  <!-- Automatically generated from definitions.json enum entries "
        f"in Riskine commit {_xe(upstream_commit)} on {today}. "
        "Do not hand-edit: run example/aonach_mor/build/riskine_to_owl.py -->"
    )
    for enum_name in sorted(defs_enums):
        spec = defs_enums[enum_name]
        scheme_uri = NS_ENUM + pascal(enum_name)
        out.append(f'  <skos:ConceptScheme rdf:about="{scheme_uri}">')
        out.append(
            f'    <skos:prefLabel xml:lang="en">{_xe(humanise(enum_name))}</skos:prefLabel>'
        )
        out.append(
            f'    <dcterms:source rdf:resource="{NS_UPSTREAM}definitions.json"/>'
        )
        out.append("  </skos:ConceptScheme>")
        values = spec.get("enum", [])
        descs = spec.get("enum-description", [])
        for idx, value in enumerate(values):
            label = descs[idx] if idx < len(descs) else str(value)
            # Concept URI must be stable + URL-safe.
            notation = str(value)
            safe = re.sub(r"[^A-Za-z0-9]+", "_", str(label)).strip("_") or f"v{idx}"
            concept_uri = f"{scheme_uri}/{safe}"
            out.append(f'  <skos:Concept rdf:about="{concept_uri}">')
            out.append(f'    <skos:inScheme rdf:resource="{scheme_uri}"/>')
            out.append(
                f'    <skos:prefLabel xml:lang="en">{_xe(str(label))}</skos:prefLabel>'
            )
            out.append(f"    <skos:notation>{_xe(notation)}</skos:notation>")
            out.append("  </skos:Concept>")
    out.append("</rdf:RDF>")
    out.append("")
    return "\n".join(out)


# --------------------------------------------------------------------------- #
# CLI                                                                         #
# --------------------------------------------------------------------------- #


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Transform Riskine JSON Schema into OWL + SKOS."
    )
    parser.add_argument(
        "--source",
        required=True,
        help="Path to a Riskine checkout (contains schemas/core/).",
    )
    parser.add_argument(
        "--out-owl",
        required=True,
        help="Path to write the OWL RDF/XML output.",
    )
    parser.add_argument(
        "--out-skos",
        required=True,
        help="Path to write the SKOS XML output.",
    )
    parser.add_argument(
        "--upstream-commit",
        default="unknown",
        help="Upstream git commit (stamped into the ontology header).",
    )
    args = parser.parse_args(argv)

    source = Path(args.source)
    core_dir = source / "schemas" / "core"
    if not core_dir.is_dir():
        _die(f"expected schemas/core/ under {source}")

    primitives, enums = load_definitions(core_dir)
    classes = load_core_classes(core_dir)
    _log(
        f"loaded {len(classes)} core classes, "
        f"{len(primitives)} primitives, {len(enums)} enums"
    )

    owl_xml = render_owl(classes, primitives, enums, args.upstream_commit)
    skos_xml = render_skos(enums, args.upstream_commit)

    out_owl = Path(args.out_owl)
    out_skos = Path(args.out_skos)
    out_owl.parent.mkdir(parents=True, exist_ok=True)
    out_skos.parent.mkdir(parents=True, exist_ok=True)
    out_owl.write_text(owl_xml, encoding="utf-8")
    out_skos.write_text(skos_xml, encoding="utf-8")
    _log(f"wrote OWL  → {out_owl}  ({len(owl_xml):,} chars)")
    _log(f"wrote SKOS → {out_skos}  ({len(skos_xml):,} chars)")
    return 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 Flax & Teal Limited
"""Build Sparnatural query UI assets for the Rós Madair demo.

Generates:
  - example/static/index/sparnatural/config.ttl
  - example/static/index/sparnatural/concepts/{collection_id}.json
  - example/static/index/aliz/resource_models/_all.json
  - example/static/index/aliz/resource_models/{graph_id}.json    (copy or symlink)
  - example/static/index/aliz/business_data/{graph_id}.json       (copy or symlink)
  - example/static/index/aliz/business_data/_index.json
  - example/pkg/alizarin/{alizarin.js, alizarin_bg.wasm}         (copy or symlink)

Run from the RosMadair repository root:
    python example/build_sparnatural_assets.py            # copies (deploy-safe)
    python example/build_sparnatural_assets.py --symlink  # symlinks (faster local dev)

Path overrides (for CI):
    PREBUILD_PKG=/path/to/arches_prebuild/pkg
    ALIZARIN_DIST=/path/to/alizarin/dist

Assumes example/build_from_prebuild has already produced static/index/.
"""

import argparse
import json
import os
import shutil
import sys
from pathlib import Path
from typing import Any, Iterable

# --------------------------------------------------------------------------- #
# Configuration                                                               #
# --------------------------------------------------------------------------- #

REPO_ROOT = Path(__file__).resolve().parent.parent
EXAMPLE_DIR = REPO_ROOT / "example"
PREBUILD_PKG = Path(
    os.environ.get(
        "PREBUILD_PKG",
        str(Path.home() / "Cód" / "Cliant" / "Arches" / "arches_prebuild" / "arches_prebuild" / "pkg"),
    )
)
ALIZARIN_DIST = Path(
    os.environ.get("ALIZARIN_DIST", str(REPO_ROOT.parent / "alizarin" / "dist"))
)

OUT_INDEX = EXAMPLE_DIR / "static" / "index"
OUT_SPARNATURAL = OUT_INDEX / "sparnatural"
OUT_ALIZ = OUT_INDEX / "aliz"
OUT_PKG_ALIZARIN = EXAMPLE_DIR / "pkg" / "alizarin"

BASE_URI = "https://example.org/"
NODE_URI = f"{BASE_URI}node/"
CLASS_URI = f"{BASE_URI}class/"
CONCEPT_URI = f"{BASE_URI}concept/"
RESOURCE_URI = f"{BASE_URI}resource/"

# Datatypes that the Rós Madair index actually stores. Anything else is
# excluded from the SHACL config (it would be unqueryable).
INDEXED_DATATYPES = {
    "concept",
    "concept-list",
    "boolean",
    "date",
    "geojson-feature-collection",
    # Resource references — handled differently (autocomplete) but we omit
    # in v1 because we'd need a per-graph resource list.
    # "resource-instance",
    # "resource-instance-list",
}

# Whitelist of (graph_name, alias) we expose in v1. Each entry must be a node
# whose datatype is in INDEXED_DATATYPES. Adding more is straightforward —
# the build script handles them automatically.
NODE_WHITELIST: dict[str, list[str]] = {
    "Heritage Asset": [
        "monument_type_n1",  # concept (Monument Type)
        "grade",             # concept (Grade)
        "townland",          # concept-list (Townland)
    ],
}

# Collections larger than this threshold use AutocompleteProperty (server-side
# substring filter via fetch interceptor) instead of ListProperty (select2's
# in-memory filter, which doesn't scale beyond a few thousand DOM options).
AUTOCOMPLETE_THRESHOLD = 1000

# How many resource files (HeritageAsset_N.json) per graph. We don't try to
# auto-discover this in JS — we list them explicitly in business_data/_index.json.

# --------------------------------------------------------------------------- #
# Arches graph parsing                                                         #
# --------------------------------------------------------------------------- #


def load_graph_from_path(path: Path) -> dict[str, Any]:
    with open(path) as fh:
        raw = json.load(fh)
    graph_obj = raw.get("graph", raw)
    if isinstance(graph_obj, list):
        graph_obj = graph_obj[0]
    return graph_obj


def load_graph(name: str) -> dict[str, Any]:
    path = PREBUILD_PKG / "graphs" / "resource_models" / f"{name}.json"
    return load_graph_from_path(path)


def graph_display_name(graph: dict[str, Any], fallback: str) -> str:
    name_field = graph.get("name")
    if isinstance(name_field, dict):
        return name_field.get("en", fallback)
    return str(name_field) if name_field else fallback


def auto_discover_graphs() -> tuple[
    "list[tuple[str, dict[str, Any], Path]]",
    "dict[str, list[dict[str, Any]]]",
    "dict[str, str]",
]:
    """Auto-discover all graphs and select nodes with indexed datatypes."""
    rm_dir = PREBUILD_PKG / "graphs" / "resource_models"
    if not rm_dir.exists():
        print(f"  warning: {rm_dir} not found", file=sys.stderr)
        return [], {}, {}

    graph_specs: list[tuple[str, dict[str, Any], Path]] = []
    selected: dict[str, list[dict[str, Any]]] = {}
    used_collections: dict[str, str] = {}

    for graph_path in sorted(rm_dir.glob("*.json")):
        if graph_path.name.startswith("_"):
            continue
        try:
            graph = load_graph_from_path(graph_path)
        except (json.JSONDecodeError, KeyError, OSError) as e:
            print(f"  warning: skipping {graph_path.name}: {e}")
            continue

        display_name = graph_display_name(graph, graph_path.stem)
        graph_specs.append((display_name, graph, graph_path))

        nodes_for_graph: list[dict[str, Any]] = []
        for node in graph.get("nodes", []):
            dt = node.get("datatype")
            if dt not in INDEXED_DATATYPES:
                continue
            nodes_for_graph.append(node)
            if dt in ("concept", "concept-list"):
                cfg = node.get("config") or {}
                col_id = cfg.get("rdmCollection")
                if col_id:
                    used_collections[col_id] = node.get("name") or node.get("alias", "")

        if nodes_for_graph:
            selected[display_name] = nodes_for_graph
            print(f"  {display_name}: {len(nodes_for_graph)} indexed nodes")
        else:
            print(f"  {display_name}: no indexed nodes (skipped from config)")

    return graph_specs, selected, used_collections


def find_node(graph: dict[str, Any], alias: str) -> dict[str, Any] | None:
    for n in graph.get("nodes", []):
        if n.get("alias") == alias:
            return n
    return None


def slugify(s: str) -> str:
    return "".join(ch if ch.isalnum() else "_" for ch in s)


# --------------------------------------------------------------------------- #
# Concept collection extraction                                               #
# --------------------------------------------------------------------------- #


def extract_concept_values(collection_id: str) -> list[dict[str, str]]:
    """Walk a collection JSON and produce {uri, label} pairs.

    The "uri" is the prefLabel value-id (which is what the Rós Madair index
    actually stores in object slots), and "label" is its human-readable text.
    """
    path = PREBUILD_PKG / "reference_data" / "collections" / f"{collection_id}.json"
    if not path.exists():
        print(f"  warning: collection {collection_id} not found at {path}")
        return []

    with open(path) as fh:
        col = json.load(fh)

    out: list[dict[str, str]] = []
    seen_value_ids: set[str] = set()

    def walk(concepts: dict[str, Any]) -> None:
        for cid, c in concepts.items():
            pref = c.get("prefLabels", {})
            # Prefer English label; fall back to first available
            label_obj = pref.get("en") or next(iter(pref.values()), None)
            if not label_obj:
                continue
            value_id = label_obj.get("id")
            label = label_obj.get("value", "").strip()
            if not value_id or not label or value_id in seen_value_ids:
                pass
            else:
                seen_value_ids.add(value_id)
                out.append({
                    "uri": f"{CONCEPT_URI}{value_id}",
                    "label": label,
                })
            children = c.get("concepts")
            if isinstance(children, dict):
                walk(children)

    walk(col.get("concepts", {}))
    out.sort(key=lambda x: x["label"].lower())
    return out


# --------------------------------------------------------------------------- #
# SHACL/Sparnatural config generation                                         #
# --------------------------------------------------------------------------- #

TTL_PRELUDE = """\
@prefix : <https://example.org/sparnatural#> .
@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix dash: <http://datashapes.org/dash#> .
@prefix sparnatural: <http://data.sparna.fr/ontologies/sparnatural-config-core#> .
@prefix base: <https://example.org/> .
@prefix rmnode: <https://example.org/node/> .
@prefix rmclass: <https://example.org/class/> .

<https://example.org/sparnatural> a owl:Ontology ;
  rdfs:label "Arches / Rós Madair"@en .

"""


def turtle_string(s: str) -> str:
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


def build_property_shape(
    graph_label: str,
    node: dict[str, Any],
    target_class_iri: str,
    use_autocomplete: bool = False,
) -> tuple[str, str]:
    """Build a sh:PropertyShape for a single indexed node.

    Returns (property_iri_local, ttl_block).
    """
    alias = node["alias"]
    name = node.get("name") or alias
    datatype = node["datatype"]
    pred_iri = f"rmnode:{alias}"

    prop_local = f":has_{alias}"

    if datatype in ("concept", "concept-list"):
        editor = "AutocompleteProperty" if use_autocomplete else "ListProperty"
        ttl = f"""\
{prop_local} a sh:PropertyShape ;
  sh:path {pred_iri} ;
  sh:name {turtle_string(name)}@en ;
  sh:nodeKind sh:IRI ;
  sh:class <{target_class_iri}> ;
  dash:editor sparnatural:{editor} .

"""
    elif datatype == "boolean":
        ttl = f"""\
{prop_local} a sh:PropertyShape ;
  sh:path {pred_iri} ;
  sh:name {turtle_string(name)}@en ;
  dash:editor sparnatural:BooleanProperty .

"""
    elif datatype == "date":
        ttl = f"""\
{prop_local} a sh:PropertyShape ;
  sh:path {pred_iri} ;
  sh:name {turtle_string(name)}@en ;
  dash:editor sparnatural:TimeProperty-Date .

"""
    else:
        ttl = ""

    return prop_local, ttl


def build_target_class(
    collection_id: str,
    label: str,
    autocomplete: bool = False,
) -> tuple[str, str]:
    """Build a sh:NodeShape used as the value class for a list/autocomplete.

    The class IRI encodes the collection ID so the fetch interceptor can
    identify which list to serve. For autocomplete, the queryString embeds
    a custom triple `?uri <urn:rosmadair:search> "$key"` which Sparnatural
    substitutes into a literal-bearing triple. The literal survives the
    sparqljs round-trip and is extracted by the fetch interceptor.
    """
    class_iri = f"{CLASS_URI}collection_{collection_id}"
    if autocomplete:
        query = (
            "PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>\n"
            "SELECT DISTINCT ?uri ?label WHERE { "
            "?uri a $range . ?uri rdfs:label ?label . "
            '?uri <urn:rosmadair:search> "$key" . } ORDER BY ?label LIMIT 50'
        )
    else:
        query = (
            "PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>\n"
            "SELECT DISTINCT ?uri ?label WHERE { "
            "?uri a $range . ?uri rdfs:label ?label . } ORDER BY ?label"
        )
    ttl = f"""\
<{class_iri}> a sh:NodeShape, sparnatural:OntologyClass ;
  sh:targetClass <{class_iri}> ;
  rdfs:label {turtle_string(label)}@en ;
  sparnatural:datasource [
    sparnatural:queryString \"\"\"{query}\"\"\"
  ] .

"""
    return class_iri, ttl


def build_shacl_config(
    selected: dict[str, list[dict[str, Any]]],
    collection_sizes: dict[str, int],
) -> str:
    """Build the full SHACL/Sparnatural config TTL.

    `selected` maps graph display name to a list of node dicts (each from the
    Arches graph's nodes array). `collection_sizes` maps collection_id to value
    count, used to decide ListProperty vs AutocompleteProperty.
    """
    parts: list[str] = [TTL_PRELUDE]
    seen_class_ttl: set[str] = set()

    for graph_name, nodes in selected.items():
        graph_class_iri = f"{CLASS_URI}{slugify(graph_name)}"
        graph_local = f":{slugify(graph_name)}"
        prop_locals: list[str] = []
        prop_ttls: list[str] = []

        for node in nodes:
            datatype = node["datatype"]
            alias = node["alias"]
            if datatype not in INDEXED_DATATYPES:
                continue

            target_class_iri = ""
            use_autocomplete = False
            if datatype in ("concept", "concept-list"):
                cfg = node.get("config") or {}
                col_id = cfg.get("rdmCollection")
                if not col_id:
                    print(f"  skipping {alias}: no rdmCollection")
                    continue
                label = node.get("name") or alias
                use_autocomplete = collection_sizes.get(col_id, 0) > AUTOCOMPLETE_THRESHOLD
                target_class_iri, class_ttl = build_target_class(
                    col_id, label, autocomplete=use_autocomplete,
                )
                if target_class_iri not in seen_class_ttl:
                    parts.append(class_ttl)
                    seen_class_ttl.add(target_class_iri)

            prop_local, prop_ttl = build_property_shape(
                graph_name, node, target_class_iri, use_autocomplete=use_autocomplete,
            )
            if not prop_ttl:
                continue
            prop_locals.append(prop_local)
            prop_ttls.append(prop_ttl)

        if not prop_locals:
            continue

        graph_ttl = f"""\
{graph_local} a sh:NodeShape, sparnatural:OntologyClass ;
  sh:targetClass <{graph_class_iri}> ;
  rdfs:label {turtle_string(graph_name)}@en ;
  sparnatural:order 1 ;
  sh:property {", ".join(prop_locals)} .

"""
        parts.append(graph_ttl)
        parts.extend(prop_ttls)

    return "".join(parts)


# --------------------------------------------------------------------------- #
# Alizarin layout                                                             #
# --------------------------------------------------------------------------- #


def force_link_or_copy(src: Path, dst: Path, use_symlink: bool) -> None:
    """Materialize src at dst, replacing any existing entry.

    Symlinks are convenient for local dev (no duplicated bytes) but break on
    GitHub Pages because git serializes them as text blobs containing the
    target path. Copy by default; opt back into symlinks for local iteration.
    """
    if dst.is_symlink() or dst.exists():
        dst.unlink()
    if use_symlink:
        dst.symlink_to(src)
    else:
        # copy2 preserves metadata; --reflink-style speedups happen at the FS layer.
        shutil.copy2(src, dst)


def build_alizarin_layout(
    graph_specs: list[tuple[str, dict[str, Any], Path]],
    use_symlink: bool,
) -> None:
    """Materialize the resource_models/ and business_data/ trees expected by
    ArchesClientRemoteStatic. Files are copied by default (deploy-safe) or
    symlinked into the Arches pkg directory when --symlink is passed.

    graph_specs is a list of (display_name, graph_obj, source_path) tuples.
    """
    rm_dir = OUT_ALIZ / "resource_models"
    bd_dir = OUT_ALIZ / "business_data"
    rm_dir.mkdir(parents=True, exist_ok=True)
    bd_dir.mkdir(parents=True, exist_ok=True)

    models_index: dict[str, Any] = {}
    file_index: dict[str, list[str]] = {}
    resource_index: dict[str, str] = {}  # resource_id -> file basename

    def _index_bd_file(src_bd: Path, bd_files: list[str]) -> None:
        dst_bd = bd_dir / src_bd.name
        force_link_or_copy(src_bd.resolve(), dst_bd, use_symlink)
        bd_files.append(f"business_data/{src_bd.name}")
        try:
            with open(src_bd) as fh:
                bd_data = json.load(fh)
            for r in bd_data.get("business_data", {}).get("resources", []):
                rid = (
                    r.get("resourceinstance", {}).get("resourceinstanceid")
                    or r.get("resourceinstanceid")
                )
                if rid:
                    resource_index[rid] = src_bd.name
        except (json.JSONDecodeError, OSError) as e:
            print(f"  warning: indexing {src_bd.name}: {e}")

    for graph_name, graph, src_path in graph_specs:
        graph_id = graph["graphid"]

        # Materialize the graph file from its actual source path
        dst = rm_dir / f"{graph_id}.json"
        force_link_or_copy(src_path.resolve(), dst, use_symlink)

        # Add to _all.json
        name_field = graph.get("name")
        if isinstance(name_field, dict):
            name_dict = name_field
        else:
            name_dict = {"en": str(name_field) if name_field else graph_name}
        models_index[graph_id] = {
            "graphid": graph_id,
            "name": name_dict,
            "isresource": True,
        }

        # Materialize business_data files for this graph
        bd_files: list[str] = []
        bd_src_dir = PREBUILD_PKG / "business_data"
        if bd_src_dir.exists():
            # csv-to-prebuild uses {graph_id}.json
            bd_by_id = bd_src_dir / f"{graph_id}.json"
            if bd_by_id.exists():
                _index_bd_file(bd_by_id, bd_files)
            # Arches prebuild uses {GraphName}_N.json
            bd_glob_name = graph_name.replace(" ", "")
            for src_bd in sorted(bd_src_dir.glob(f"{bd_glob_name}_*.json")):
                _index_bd_file(src_bd, bd_files)

        file_index[graph_id] = bd_files
        print(
            f"  {graph_name}: {len(bd_files)} business_data files, "
            f"{len(resource_index)} resources indexed"
        )

    with open(rm_dir / "_all.json", "w") as fh:
        json.dump({"models": models_index}, fh, indent=2)

    with open(bd_dir / "_index.json", "w") as fh:
        json.dump(file_index, fh, indent=2)

    with open(bd_dir / "_resource_index.json", "w") as fh:
        json.dump(resource_index, fh)


def copy_alizarin_dist(use_symlink: bool) -> None:
    OUT_PKG_ALIZARIN.mkdir(parents=True, exist_ok=True)
    for name in ("alizarin.js", "alizarin_bg.wasm"):
        src = (ALIZARIN_DIST / name).resolve()
        if not src.exists():
            print(f"  warning: {src} does not exist — run `npm run build` in alizarin/")
            continue
        dst = OUT_PKG_ALIZARIN / name
        force_link_or_copy(src, dst, use_symlink)
    verb = "linked" if use_symlink else "copied"
    print(f"  alizarin pkg: {verb} into {OUT_PKG_ALIZARIN}")


# --------------------------------------------------------------------------- #
# Main                                                                        #
# --------------------------------------------------------------------------- #


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument(
        "--symlink",
        action="store_true",
        help=(
            "Symlink large files instead of copying. Faster for local dev but "
            "breaks GitHub Pages and any other static host that doesn't follow "
            "symlinks. Default is to copy."
        ),
    )
    args = parser.parse_args()

    if not PREBUILD_PKG.exists():
        print(f"error: Arches pkg not found at {PREBUILD_PKG}", file=sys.stderr)
        print("       Set PREBUILD_PKG=/path/to/arches_prebuild/pkg", file=sys.stderr)
        return 1
    if not OUT_INDEX.exists():
        print(
            f"error: {OUT_INDEX} not found — run build_from_prebuild first",
            file=sys.stderr,
        )
        return 1

    OUT_SPARNATURAL.mkdir(parents=True, exist_ok=True)
    (OUT_SPARNATURAL / "concepts").mkdir(exist_ok=True)

    # 1. Load graphs and select nodes — auto-discover or use whitelist
    graph_specs: list[tuple[str, dict[str, Any], Path]] = []
    selected: dict[str, list[dict[str, Any]]] = {}
    used_collections: dict[str, str] = {}  # collection_id -> label

    # Try whitelist first; fall back to auto-discovery if no whitelisted
    # graphs are found in the prebuild directory.
    whitelist_ok = False
    if NODE_WHITELIST:
        rm_dir = PREBUILD_PKG / "graphs" / "resource_models"
        for graph_name, aliases in NODE_WHITELIST.items():
            # Try name-based path (Arches prebuild) first
            path = rm_dir / f"{graph_name}.json"
            if not path.exists():
                continue
            whitelist_ok = True
            graph = load_graph_from_path(path)
            graph_specs.append((graph_name, graph, path))
            nodes_for_graph: list[dict[str, Any]] = []
            for alias in aliases:
                node = find_node(graph, alias)
                if not node:
                    print(f"  warning: {graph_name}/{alias} not found in graph")
                    continue
                dt = node.get("datatype")
                if dt not in INDEXED_DATATYPES:
                    print(f"  warning: {graph_name}/{alias} datatype '{dt}' is not indexed")
                    continue
                nodes_for_graph.append(node)
                if dt in ("concept", "concept-list"):
                    cfg = node.get("config") or {}
                    col_id = cfg.get("rdmCollection")
                    if col_id:
                        used_collections[col_id] = node.get("name") or alias
            selected[graph_name] = nodes_for_graph

    if not whitelist_ok:
        print("Auto-discovering graphs from prebuild directory ...")
        graph_specs, selected, used_collections = auto_discover_graphs()

    # 2. Generate concept JSON files (need sizes before SHACL config)
    print("Generating concept value lists ...")
    collection_sizes: dict[str, int] = {}
    for col_id, label in used_collections.items():
        values = extract_concept_values(col_id)
        collection_sizes[col_id] = len(values)
        out_path = OUT_SPARNATURAL / "concepts" / f"{col_id}.json"
        with open(out_path, "w") as fh:
            json.dump({"id": col_id, "label": label, "values": values}, fh, indent=2)
        kind = "autocomplete" if len(values) > AUTOCOMPLETE_THRESHOLD else "list"
        print(f"  {label}: {len(values)} values -> {out_path.name} ({kind})")

    # 3. Generate SHACL config (uses collection_sizes to pick widget)
    print("Generating Sparnatural config.ttl ...")
    ttl = build_shacl_config(selected, collection_sizes)
    with open(OUT_SPARNATURAL / "config.ttl", "w") as fh:
        fh.write(ttl)
    print(f"  wrote {OUT_SPARNATURAL / 'config.ttl'} ({len(ttl)} bytes)")

    # 4. Build Alizarin layout
    mode = "symlink" if args.symlink else "copy"
    print(f"Building Alizarin static layout ({mode}) ...")
    build_alizarin_layout(graph_specs, use_symlink=args.symlink)

    # 5. Materialize Alizarin dist
    print(f"Materializing Alizarin dist ({mode}) ...")
    copy_alizarin_dist(use_symlink=args.symlink)

    print("Done.")
    return 0


if __name__ == "__main__":
    sys.exit(main())

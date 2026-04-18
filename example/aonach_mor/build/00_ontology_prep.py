#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 Flax & Teal Limited
"""Stage 0 — ontology prep for the Aonach Mór resilience demo.

Fetches and stages the seven third-party ontologies that the Aonach Mór
Arches resource models bind to, in a form Arches can load:

  - CIDOC-CRM 7.1.3        (RDF/XML, upstream)
  - FIBO FND + BE subset   (OWL XML, edmcouncil/fibo git clone, pinned;
                            organisation + agent + person classes only —
                            FIBO has no insurance module despite the
                            confusingly-named IND domain, which is
                            Indices and Indicators, not Industry>Insurance)
  - Riskine (as OWL)       (upstream is Apache-2.0 JSON Schema at
                            github.com/riskine/ontology; we clone it and
                            run `riskine_to_owl.py` to produce a
                            mechanical OWL + SKOS rendering)
  - SOSA / SSN             (Turtle upstream → RDF/XML via rdflib)
  - PROV-O                 (Turtle upstream → RDF/XML via rdflib)
  - BOT                    (Turtle upstream → RDF/XML via rdflib)
  - GeoSPARQL 1.1          (RDF/XML, upstream)
  - GEM Building Taxonomy  (SKOS XML, upstream catalogue)

For each ontology we write:

  vocab/<short_name>/<file>            — the ontology artefact(s)
  vocab/<short_name>/SOURCE.md         — URL, version/commit, licence, fetched-on

Run from the RosMadair repository root:

    python example/aonach_mor/build/00_ontology_prep.py            # fetch all
    python example/aonach_mor/build/00_ontology_prep.py --only fibo,bot
    python example/aonach_mor/build/00_ontology_prep.py --dry-run  # show plan
    python example/aonach_mor/build/00_ontology_prep.py --force    # re-fetch

Env overrides (CI-friendly):

    AONACH_VOCAB_DIR   target dir (default: example/aonach_mor/vocab)
    FIBO_COMMIT        pinned FIBO commit hash (default: see FIBO_DEFAULT_COMMIT)
    NETWORK=0          skip all network calls (useful for CI cache priming)

This script is idempotent: re-running is safe. Already-present artefacts are
left alone unless --force is passed. A short SOURCE.md is rewritten every run
so "last fetched on" stays current.

Dependencies:
  - rdflib                   (Turtle → RDF/XML conversion)
  - requests                 (HTTP fetch with sensible UA)
  - git (cli)                (FIBO clone — full repo is ~200 MB, so we shallow
                              clone and sparse-checkout only the modules we
                              need)
"""

from __future__ import annotations

import argparse
import datetime as _dt
import os
import shutil
import subprocess
import sys
import textwrap
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

# --------------------------------------------------------------------------- #
# Configuration                                                               #
# --------------------------------------------------------------------------- #

REPO_ROOT = Path(__file__).resolve().parents[3]
AONACH_DIR = REPO_ROOT / "example" / "aonach_mor"
VOCAB_DIR = Path(
    os.environ.get("AONACH_VOCAB_DIR", str(AONACH_DIR / "vocab"))
).resolve()

USER_AGENT = "RosMadair-AonachMor/0.1 (+https://github.com/flaxandteal/RosMadair)"
HTTP_TIMEOUT = 60  # seconds

# Pin FIBO to a known-good commit so the demo is reproducible. Bump
# deliberately; a moving target here would let upstream renames break the
# Arches class bindings silently.
FIBO_DEFAULT_COMMIT = "master_2025Q4"  # tag; override via FIBO_COMMIT env var

# Subset of FIBO we actually need, as directory paths (git sparse-checkout
# cone mode only accepts directories). Staging slightly more than the bare
# minimum so transitive imports resolve offline — Arches will only load the
# files we explicitly point it at, so the extra coverage is cheap.
#
# No IND/Insurance entry because FIBO has no insurance module. "IND" in
# FIBO stands for Indices and Indicators (interest rates, FX, equity
# indices) — a point of lasting frustration for anyone who assumed the
# three-letter code followed the pattern of the other domains. Insurance
# classes come from the Riskine ontology instead; see fetch_riskine.
FIBO_MODULES = [
    "FND/AgentsAndPeople",
    "FND/Organizations",
    "BE/LegalEntities",
]

# Pinned Riskine commit. Upstream publishes JSON Schema, not OWL, so we
# run riskine_to_owl.py against a checked-out tree to produce the OWL +
# SKOS rendering. Bump deliberately — a moving HEAD here would let
# upstream rename a property and break the Arches class bindings silently.
RISKINE_DEFAULT_COMMIT = "4cd6876ac305e5e10adeefc400548dc6e96b0a0f"

# --------------------------------------------------------------------------- #
# Small helpers                                                               #
# --------------------------------------------------------------------------- #


def _log(msg: str) -> None:
    print(f"[00_ontology_prep] {msg}", flush=True)


def _die(msg: str, code: int = 1) -> None:
    print(f"[00_ontology_prep] ERROR: {msg}", file=sys.stderr, flush=True)
    sys.exit(code)


def _today() -> str:
    return _dt.date.today().isoformat()


def _http_get(url: str) -> bytes:
    if os.environ.get("NETWORK") == "0":
        _die(f"NETWORK=0 set but fetch needed for {url}")
    try:
        import requests  # lazy: rdflib isn't needed for every path either
    except ImportError:
        _die("requests is required: pip install requests rdflib")
    r = requests.get(
        url,
        headers={"User-Agent": USER_AGENT, "Accept": "*/*"},
        timeout=HTTP_TIMEOUT,
    )
    r.raise_for_status()
    return r.content


def _write_bytes(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    _log(f"wrote {path.relative_to(REPO_ROOT)}  ({len(data):,} bytes)")


def _write_source_md(
    vocab_subdir: Path,
    *,
    name: str,
    url: str,
    version: str,
    licence: str,
    attribution: str,
    notes: str = "",
) -> None:
    body = textwrap.dedent(
        f"""\
        # {name}

        - **Source**: {url}
        - **Version / commit**: {version}
        - **Licence**: {licence}
        - **Attribution**: {attribution}
        - **Fetched on**: {_today()}
        - **Fetched by**: `example/aonach_mor/build/00_ontology_prep.py`
        """
    )
    if notes:
        body += "\n" + notes.rstrip() + "\n"
    (vocab_subdir / "SOURCE.md").write_text(body, encoding="utf-8")


def _turtle_to_rdfxml(ttl_bytes: bytes, base_iri: str) -> bytes:
    try:
        from rdflib import Graph
    except ImportError:
        _die("rdflib is required: pip install rdflib")
    g = Graph()
    g.parse(data=ttl_bytes, format="turtle", publicID=base_iri)
    # rdflib's pretty-xml serializer emits harmless UserWarnings when it
    # encounters SHACL-style lists on blank nodes ("Assertions on BNode …
    # other than RDF.first and RDF.rest are ignored … including RDF.List").
    # They do not affect the emitted RDF/XML — silence them so the fetch
    # log stays useful.
    import warnings

    with warnings.catch_warnings():
        warnings.filterwarnings(
            "ignore",
            message=r"Assertions on rdflib\.term\.BNode.*including RDF\.List",
            category=UserWarning,
        )
        return g.serialize(format="pretty-xml").encode("utf-8")


def _already_present(vocab_subdir: Path, expected_files: list[str]) -> bool:
    return all((vocab_subdir / f).is_file() for f in expected_files)


# --------------------------------------------------------------------------- #
# Per-ontology fetchers                                                       #
# --------------------------------------------------------------------------- #


@dataclass
class Ontology:
    key: str  # CLI filter key, matches vocab/ subdir name
    name: str  # human label
    fetch: Callable[[Path, bool], None]


def fetch_cidoc_crm(vocab_subdir: Path, force: bool) -> None:
    """CIDOC-CRM 7.1.3 as RDF/XML, direct download."""
    files = ["cidoc_crm_v7.1.3.rdf"]
    if not force and _already_present(vocab_subdir, files):
        _log("cidoc_crm: present, skipping (use --force to refresh)")
    else:
        url = "https://cidoc-crm.org/rdfs/7.1.3/CIDOC_CRM_v7.1.3.rdf"
        data = _http_get(url)
        _write_bytes(vocab_subdir / files[0], data)
    _write_source_md(
        vocab_subdir,
        name="CIDOC Conceptual Reference Model",
        url="https://cidoc-crm.org/",
        version="v7.1.3 (RDFS serialisation)",
        licence="CC-BY 4.0",
        attribution="CIDOC Conceptual Reference Model, ICOM / CIDOC",
        notes=(
            "Arches loads this as an ontology (class + property definitions). "
            "Concept vocabularies for CIDOC-using models live under "
            "`vocab/skos/` as separate SKOS XML files."
        ),
    )


def fetch_riskine(vocab_subdir: Path, force: bool) -> None:
    """Clone Riskine, run the JSON-Schema → OWL transform, stage outputs.

    Writes three things:

      vocab/riskine/source/           ← upstream repo, pinned
      vocab/riskine/riskine.rdf       ← generated OWL (classes + properties)
      vocab/skos/riskine_enums.xml    ← generated SKOS (36 enums)

    The SKOS file lives alongside the other SKOS collections (not under
    vocab/riskine/) so the Stage 0 SKOS loader can pick it up with one
    glob.
    """
    commit = os.environ.get("RISKINE_COMMIT", RISKINE_DEFAULT_COMMIT)
    source_dir = vocab_subdir / "source"
    marker = vocab_subdir / ".commit"
    owl_out = vocab_subdir / "riskine.rdf"
    skos_out = VOCAB_DIR / "skos" / "riskine_enums.xml"

    need_clone = (
        force
        or not marker.is_file()
        or marker.read_text().strip() != commit
        or not source_dir.is_dir()
    )
    if need_clone:
        if os.environ.get("NETWORK") == "0":
            _die("NETWORK=0 set but Riskine clone needed")
        if not shutil.which("git"):
            _die("git is required to stage Riskine")
        if source_dir.exists():
            shutil.rmtree(source_dir)
        source_dir.mkdir(parents=True, exist_ok=True)
        repo_url = "https://github.com/riskine/ontology.git"
        _log(f"riskine: cloning {repo_url} @ {commit[:12]} (depth=1)")
        # No sparse-checkout — the schemas directory is tiny (< 1 MB).
        subprocess.run(
            ["git", "clone", "--depth=1", repo_url, str(source_dir)],
            check=True,
        )
        subprocess.run(
            ["git", "-C", str(source_dir), "fetch", "--depth=1", "origin", commit],
            check=True,
        )
        subprocess.run(
            ["git", "-C", str(source_dir), "checkout", commit],
            check=True,
        )
        shutil.rmtree(source_dir / ".git", ignore_errors=True)
        marker.write_text(commit + "\n", encoding="utf-8")
    else:
        _log(f"riskine: source present at {commit[:12]}, skipping clone")

    # Transform — always re-run so the output always matches the pinned
    # commit, even if someone hand-edits vocab/riskine/source/.
    transform_script = (
        Path(__file__).resolve().parent / "riskine_to_owl.py"
    )
    _log(f"riskine: transforming to OWL via {transform_script.name}")
    subprocess.run(
        [
            sys.executable,
            str(transform_script),
            "--source",
            str(source_dir),
            "--out-owl",
            str(owl_out),
            "--out-skos",
            str(skos_out),
            "--upstream-commit",
            commit,
        ],
        check=True,
    )

    _write_source_md(
        vocab_subdir,
        name="Riskine Global Insurance Ontology (OWL rendering)",
        url="https://github.com/riskine/ontology",
        version=commit,
        licence="Apache-2.0 (upstream) / AGPL-3.0-or-later (our OWL rendering)",
        attribution="© Riskine GmbH (source JSON Schema); OWL rendering © Flax & Teal Limited",
        notes=(
            "Upstream publishes JSON Schema, not OWL. The OWL file at "
            "`riskine.rdf` is an automated, mechanical translation "
            "produced by `build/riskine_to_owl.py`. SKOS enums are "
            "published alongside the other concept collections at "
            "`../skos/riskine_enums.xml`. See the transform script for "
            "the exact mapping rules — this is an approximate rendering, "
            "not a semantic canonicalisation."
        ),
    )


def fetch_fibo(vocab_subdir: Path, force: bool) -> None:
    """FIBO insurance + formal-org subset via shallow git clone + sparse checkout."""
    commit = os.environ.get("FIBO_COMMIT", FIBO_DEFAULT_COMMIT)
    marker = vocab_subdir / ".commit"
    if (
        not force
        and marker.is_file()
        and marker.read_text().strip() == commit
        and (vocab_subdir / "IND").is_dir()
    ):
        _log(f"fibo: present at {commit}, skipping (use --force to refresh)")
    else:
        if os.environ.get("NETWORK") == "0":
            _die("NETWORK=0 set but FIBO clone needed")
        if not shutil.which("git"):
            _die("git is required to stage FIBO")
        if vocab_subdir.exists():
            shutil.rmtree(vocab_subdir)
        vocab_subdir.mkdir(parents=True, exist_ok=True)

        # Shallow, sparse: only the modules we need, at the pinned tag.
        repo_url = "https://github.com/edmcouncil/fibo.git"
        _log(f"fibo: cloning {repo_url} @ {commit} (sparse, depth=1)")
        subprocess.run(
            [
                "git",
                "clone",
                "--depth=1",
                "--branch",
                commit,
                "--filter=blob:none",
                "--sparse",
                repo_url,
                str(vocab_subdir),
            ],
            check=True,
        )
        subprocess.run(
            ["git", "-C", str(vocab_subdir), "sparse-checkout", "set", *FIBO_MODULES],
            check=True,
        )
        # Drop the .git dir to keep the demo tree lean.
        shutil.rmtree(vocab_subdir / ".git", ignore_errors=True)
        marker.write_text(commit + "\n", encoding="utf-8")

    _write_source_md(
        vocab_subdir,
        name="FIBO — Financial Industry Business Ontology",
        url="https://github.com/edmcouncil/fibo",
        version=commit,
        licence="MIT (specifications), CC-BY-4.0 (documentation)",
        attribution="© EDM Council, FIBO working groups",
        notes=(
            "Staged subset:\n\n"
            + "\n".join(f"  - `{m}`" for m in FIBO_MODULES)
            + "\n\n"
            "We bind `cmns-org:FormalOrganization` (reachable via FIBO's "
            "import of OMG Commons) on the Organisation resource model and "
            "`fibo-fnd-aap-ppl:Person` on the Person resource model. "
            "Transitive imports load from the same tree.\n\n"
            "FIBO has **no** insurance module — its `IND` namespace is "
            "Indices and Indicators, not Industry>Insurance. Insurance "
            "classes in Aonach Mór come from Riskine; see "
            "`vocab/riskine/REFERENCE.md`."
        ),
    )


def _fetch_turtle_as_rdfxml(
    vocab_subdir: Path,
    *,
    filename: str,
    url: str,
    base_iri: str,
    force: bool,
) -> None:
    target = vocab_subdir / filename
    if not force and target.is_file():
        _log(f"{vocab_subdir.name}: {filename} present, skipping")
        return
    ttl = _http_get(url)
    # Stash the original Turtle alongside the RDF/XML for traceability.
    _write_bytes(vocab_subdir / (filename.rsplit(".", 1)[0] + ".ttl"), ttl)
    xml = _turtle_to_rdfxml(ttl, base_iri=base_iri)
    _write_bytes(target, xml)


def fetch_sosa_ssn(vocab_subdir: Path, force: bool) -> None:
    _fetch_turtle_as_rdfxml(
        vocab_subdir,
        filename="sosa_ssn.rdf",
        url="https://www.w3.org/ns/ssn/rdf",
        base_iri="http://www.w3.org/ns/ssn/",
        force=force,
    )
    _write_source_md(
        vocab_subdir,
        name="SOSA / SSN — Semantic Sensor Network",
        url="https://www.w3.org/TR/vocab-ssn/",
        version="W3C Recommendation 2017-10-19",
        licence="W3C Document Licence",
        attribution="© W3C, Spatial Data on the Web Working Group",
        notes=(
            "Upstream is served as Turtle from `https://www.w3.org/ns/ssn/rdf`; "
            "we convert to RDF/XML via rdflib so Arches can load it with the "
            "same pipeline as CIDOC-CRM."
        ),
    )


def fetch_prov_o(vocab_subdir: Path, force: bool) -> None:
    _fetch_turtle_as_rdfxml(
        vocab_subdir,
        filename="prov_o.rdf",
        url="https://www.w3.org/ns/prov-o",
        base_iri="http://www.w3.org/ns/prov#",
        force=force,
    )
    _write_source_md(
        vocab_subdir,
        name="PROV-O — The PROV Ontology",
        url="https://www.w3.org/TR/prov-o/",
        version="W3C Recommendation 2013-04-30",
        licence="W3C Document Licence",
        attribution="© W3C, Provenance Working Group",
        notes=(
            "Used on the HazardModel resource model "
            "(`prov:Activity` / `prov:wasGeneratedBy` / `prov:wasDerivedFrom`) "
            "to trace model run → sensor input → footprint output."
        ),
    )


def fetch_bot(vocab_subdir: Path, force: bool) -> None:
    _fetch_turtle_as_rdfxml(
        vocab_subdir,
        filename="bot.rdf",
        url="https://w3id.org/bot/bot.ttl",
        base_iri="https://w3id.org/bot#",
        force=force,
    )
    _write_source_md(
        vocab_subdir,
        name="BOT — Building Topology Ontology",
        url="https://w3id.org/bot",
        version="W3C Community Group Report (latest)",
        licence="W3C Community Group Report",
        attribution="© W3C Linked Building Data Community Group",
        notes=(
            "Used on the Building resource model for zone / storey / element "
            "decomposition. Intended to complement CIDOC-CRM (physical-thing "
            "identity) rather than replace it."
        ),
    )


def fetch_geosparql(vocab_subdir: Path, force: bool) -> None:
    # GeoSPARQL 1.1 is published as Turtle in the standards repo; we convert
    # it to RDF/XML via rdflib so Arches loads it with the same pipeline as
    # CIDOC-CRM.
    _fetch_turtle_as_rdfxml(
        vocab_subdir,
        filename="geosparql_v1_1.rdf",
        url="https://opengeospatial.github.io/ogc-geosparql/geosparql11/geo.ttl",
        base_iri="http://www.opengis.net/ont/geosparql#",
        force=force,
    )
    _write_source_md(
        vocab_subdir,
        name="GeoSPARQL 1.1",
        url="https://www.ogc.org/standards/geosparql",
        version="OGC 22-047r1 (GeoSPARQL 1.1)",
        licence="OGC Document Licence",
        attribution="© Open Geospatial Consortium",
        notes=(
            "Provides `geo:Feature`, `geo:Geometry`, `geo:hasGeometry` used "
            "on the Site and HazardFootprint models. Geometry literals in "
            "Aonach Mór use the rebranded local TM CRS, not WGS84."
        ),
    )


def fetch_gem_taxonomy(vocab_subdir: Path, force: bool) -> None:
    """GEM Building Taxonomy v3 SKOS.

    Upstream does not publish a canonical SKOS XML; the catalogue is a
    spreadsheet + PDF. Stage 0 step 20 (`20_skos_collections.py`) emits a
    hand-picked subset to `vocab/skos/GEMTaxonomy.xml` from an embedded
    data table. This placeholder just records the provenance note so
    reviewers can find the source.
    """
    placeholder = vocab_subdir / "README.md"
    if not force and placeholder.is_file():
        _log("gem_taxonomy: placeholder present, skipping")
    else:
        placeholder.parent.mkdir(parents=True, exist_ok=True)
        placeholder.write_text(
            textwrap.dedent(
                """\
                # GEM Building Taxonomy v3 — staging area

                Upstream (https://www.globalquakemodel.org/gem) publishes the
                taxonomy as a specification document + spreadsheet rather
                than as a downloadable SKOS XML. Stage 0 step 20
                (`build/20_skos_collections.py`) emits a hand-picked subset
                of the primary material+lateral-load-resisting-system
                codes as `../skos/GEMTaxonomy.xml` from an embedded data
                table.

                A future iteration that needs the full taxonomy should
                drop a CSV into `source/` and have `20_skos_collections.py`
                read it in preference to the embedded table.
                """
            ),
            encoding="utf-8",
        )
        (vocab_subdir / "source").mkdir(exist_ok=True)
    _write_source_md(
        vocab_subdir,
        name="GEM Building Taxonomy v3",
        url="https://www.globalquakemodel.org/gem",
        version="v3 (catalogue)",
        licence="GEM Foundation terms of use",
        attribution="© GEM Foundation",
        notes=(
            "The SKOS XML shipped under `vocab/skos/GEMTaxonomy.xml` is "
            "generated at Stage 0 by `build/20_skos_collections.py` from "
            "an embedded subset; it is a SKOS-ification of a small slice "
            "of the upstream catalogue, not an upstream artefact. "
            "Reviewers who need the canonical definitions should consult "
            "the GEM specification document."
        ),
    )


# --------------------------------------------------------------------------- #
# Registry + CLI                                                              #
# --------------------------------------------------------------------------- #

ONTOLOGIES: list[Ontology] = [
    Ontology("cidoc_crm", "CIDOC-CRM 7.1.3", fetch_cidoc_crm),
    Ontology("fibo", "FIBO (FND + BE subset, organisation/agent/person)", fetch_fibo),
    Ontology("riskine", "Riskine Insurance Ontology (OWL rendering)", fetch_riskine),
    Ontology("sosa_ssn", "SOSA / SSN", fetch_sosa_ssn),
    Ontology("prov_o", "PROV-O", fetch_prov_o),
    Ontology("bot", "Building Topology Ontology", fetch_bot),
    Ontology("geosparql", "GeoSPARQL 1.1", fetch_geosparql),
    Ontology("gem_taxonomy", "GEM Building Taxonomy v3", fetch_gem_taxonomy),
]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Stage 0 — fetch and stage Aonach Mór ontologies."
    )
    parser.add_argument(
        "--only",
        default="",
        help="Comma-separated subset of ontology keys to process "
        f"(default: all). Keys: {', '.join(o.key for o in ONTOLOGIES)}",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="Re-fetch even if artefacts are already present.",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print the plan and exit without fetching or writing anything.",
    )
    args = parser.parse_args(argv)

    selected_keys = (
        {k.strip() for k in args.only.split(",") if k.strip()}
        if args.only
        else {o.key for o in ONTOLOGIES}
    )
    unknown = selected_keys - {o.key for o in ONTOLOGIES}
    if unknown:
        _die(f"unknown ontology keys: {', '.join(sorted(unknown))}")

    _log(f"repo root: {REPO_ROOT}")
    _log(f"vocab dir: {VOCAB_DIR}")
    _log(f"selected:  {', '.join(sorted(selected_keys))}")
    if args.dry_run:
        for o in ONTOLOGIES:
            if o.key in selected_keys:
                _log(f"  would fetch: {o.key}  ({o.name})")
        return 0

    VOCAB_DIR.mkdir(parents=True, exist_ok=True)
    for o in ONTOLOGIES:
        if o.key not in selected_keys:
            continue
        _log(f"=== {o.key}: {o.name} ===")
        subdir = VOCAB_DIR / o.key
        subdir.mkdir(parents=True, exist_ok=True)
        o.fetch(subdir, args.force)

    _log("done.")
    return 0


if __name__ == "__main__":
    sys.exit(main())

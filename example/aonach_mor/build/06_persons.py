#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# © Flax & Teal Limited. Aonach Mór resilience demo — person synthesis (stage 06).
#
# The responder/staff roster the demo's "doctors who work near ground zero" query
# runs over. Per RosMadair-ResilienceDemo-Plan.md §6.2 persons are synthesised from
# public-domain common-name pools as explicitly fictional combinations — never a
# real individual. Names follow the Aonach Mór Irish veneer (Irish given name +
# anglicised-Irish surname), matching the geography's language. role_titles binds
# to the PersonRole controlled list (E21 Person → E55 Type via P2 has type), so a
# "doctor" is a prunable Concept, and clinical roles are stationed at hospital-
# cluster wards so the spatial anchor is meaningful. Deterministic (single seed).
#
#   python example/aonach_mor/build/06_persons.py --count 4000 [--seed 1904] [--stdout]

import argparse
import csv
import json
import random
import sys
import unicodedata
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
WARDS = json.loads((ROOT / "geo/processed/iconic_wards.json").read_text())["wards"]
WARD_BY_KEY = {w["key"]: w for w in WARDS}

# Public-domain common Irish given names (both genders) + anglicised-Irish
# surnames (very common, and side-stepping the Ó/Ní gendered-surname grammar).
# Fictional individuals = free combinations of these pools.
GIVEN = [
    "Aoife", "Siobhán", "Niamh", "Caoimhe", "Róisín", "Méabh", "Sorcha", "Órla",
    "Gráinne", "Nuala", "Deirdre", "Síle", "Bríd", "Eithne", "Clíona", "Aisling",
    "Áine", "Fiadh", "Saoirse", "Éabha", "Laoise", "Doireann", "Muireann", "Ríona",
    "Treasa", "Bláthnaid", "Cáit", "Mairéad", "Úna", "Neasa", "Sadhbh", "Líadan",
    "Séamus", "Cillian", "Oisín", "Fionn", "Pádraig", "Cormac", "Dara", "Eoin",
    "Ruairí", "Tadhg", "Lorcán", "Diarmuid", "Conor", "Breandán", "Colm", "Fearghus",
    "Dónal", "Odhrán", "Cian", "Daithí", "Peadar", "Rónán", "Tomás", "Ciarán",
    "Antaine", "Barra", "Caoimhín", "Daire", "Éamon", "Faolán", "Gearóid", "Iarla",
    "Naoise", "Páidí", "Seosamh", "Ultán", "Cathal", "Déaglán", "Fintan", "Lonán",
]
# Anglicised-Irish + Irish-form surnames — a wide pool so collisions in a 4k roster
# stay occasional (realistic) rather than pervasive.
SURNAME = [
    "Ó Briain", "Ní Mhurchú", "Mac Cárthaigh", "Ó Súilleabháin", "Ní Dhálaigh",
    "Ó Conchúir", "Mac Gabhann", "Ó Ceallaigh", "Ní Riain", "Ó Dónaill",
    "Ó Máille", "Ní Chonaill", "Mac Diarmada", "Ó Floinn", "Ní Bhraonáin",
    "Walsh", "Byrne", "Ryan", "O'Connor", "Gallagher", "Doyle", "Kennedy", "Lynch",
    "Murray", "Quinn", "Moore", "Healy", "Nolan", "Fitzgerald", "Brennan", "Keane",
    "Dolan", "Clancy", "Hogan", "Flynn", "Boland", "Devlin", "Sheehan", "Mallon",
    "Kavanagh", "Fahey", "Noonan", "Hartnett", "Casey", "Dillon", "Egan", "Gorman",
    "Hennessy", "Keating", "Lenihan", "Markey", "Scanlon", "Tierney", "Whelan", "Breen",
    "Carey", "Curran", "Donnelly", "Farrell", "Guerin", "Hayes", "Kiely", "Madden",
    "Phelan", "Riordan", "Slattery", "Troy", "Ahearne", "Cullinane", "Daly", "Fogarty",
]

# PersonRole mix. Weighted to a plausible roster — more teachers/nurses than
# harbour masters. Each role carries the ward-clusters it is plausibly stationed
# in, so clinical staff sit near hospitals (coastal/commercial/riverine), port
# roles at the quays, etc. — the spatial anchor for "near ground zero".
ROLES = [
    # (role, weight, [ward clusters], title-noun-for-notes)
    ("Doctor", 7, ["coastal", "commercial", "riverine"], "clinician"),
    ("Nurse", 12, ["coastal", "commercial", "riverine", "heritage"], "nurse"),
    ("Paramedic", 6, ["coastal", "commercial", "port"], "paramedic"),
    ("Fire officer", 6, ["commercial", "riverine", "port"], "fire officer"),
    ("Police officer", 8, ["commercial", "riverine"], "officer"),
    ("Emergency coordinator", 3, ["commercial"], "coordinator"),
    ("Structural engineer", 5, ["commercial", "industrial", "port", "peri-urban"], "engineer"),
    ("Shelter warden", 7, ["coastal", "residential", "peri-urban"], "warden"),
    ("Harbour master", 1, ["port"], "harbour master"),
    ("Utility technician", 6, ["industrial", "peri-urban", "port"], "technician"),
    ("Port operative", 6, ["port"], "docker"),
    ("Teacher", 13, ["residential", "commercial", "coastal", "heritage"], "teacher"),
]

# Contact-handle shapes — varied, fictional.
CALLSIGN_WORDS = ["Delta", "Echo", "Foxtrot", "Kilo", "Oscar", "Sierra", "Tango",
                  "Victor", "Whiskey", "Bravo", "Lima", "November"]


def ward_for(clusters, rng):
    pool = [w for w in WARDS if w["cluster"] in clusters] or WARDS
    return rng.choice(pool)


def site_id_for_ward(ward):
    # site-NNN in iconic_wards order (site.csv is the 13 wards 1:1 in that order)
    return f"site-{WARDS.index(ward) + 1:03d}"


def _ascii(s):
    # strip accents + non-alphanumerics → an email-safe token from a surname
    nfkd = unicodedata.normalize("NFKD", s)
    return "".join(c for c in nfkd if c.isascii() and c.isalnum()).lower()


def contact_handle(role, name, rng):
    """A contact string derived from the person — radio callsign for field
    responders, a fictional-domain email or an extension for the rest. Never a
    sequential placeholder."""
    if role in ("Fire officer", "Police officer", "Paramedic", "Emergency coordinator", "Harbour master"):
        return f"{rng.choice(CALLSIGN_WORDS)}-{rng.randint(1, 48)}"
    given, *rest = name.split()
    gi = (_ascii(given) or "x")[0]
    surname = _ascii(rest[-1]) if rest else "x"
    if rng.random() < 0.6:
        return f"{gi}.{surname}@aonachmor.example"
    return f"ext. {rng.randint(200, 989)}"


def notes_for(role, title, ward_label, rng):
    # Many persons carry no note (realistic sparsity); those that do are specific.
    if rng.random() < 0.45:
        return ""
    bits = {
        "Doctor": [
            f"Consultant at the {ward_label} hospital, on the surge-response rota.",
            "Registrar in emergency medicine, flood-season on-call.",
            f"GP covering the {ward_label} ward, trained in triage.",
        ],
        "Nurse": [
            f"Senior nurse, {ward_label} inpatient ward.",
            "Community nurse on the evacuation-assist list.",
            "Theatre nurse, secondary on-call.",
        ],
        "Paramedic": [
            "Advanced paramedic, coastal ambulance station.",
            f"Paramedic crewing the {ward_label} rapid-response unit.",
        ],
        "Fire officer": [
            f"Station officer, {ward_label} approaches.",
            "Watch commander, water-rescue qualified.",
        ],
        "Police officer": [f"Community officer for the {ward_label} sector.",
                           "Traffic and cordon duties during events."],
        "Emergency coordinator": ["Duty coordinator at the city EOC.",
                                  "Multi-agency liaison, flood cell."],
        "Structural engineer": ["Rapid post-event building-assessment engineer.",
                                f"Structural reviewer for {ward_label} heritage fabric."],
        "Shelter warden": [f"Warden for the {ward_label} muster-and-shelter point.",
                           "Keyholder for two designated shelters."],
        "Harbour master": [f"Harbour master, {ward_label} quays."],
        "Utility technician": ["Substation switching technician, on standby.",
                               "Pumping-station duty technician."],
        "Port operative": [f"Crane and quay operative, {ward_label}.",
                           "Container-yard operative, night shift."],
        "Teacher": [f"Primary teacher, {ward_label} ward.",
                    "Secondary teacher and shelter volunteer."],
    }
    return rng.choice(bits.get(role, [f"{title.capitalize()} based in {ward_label}."]))


def generate(count, seed):
    rng = random.Random(seed)
    roles = [r[0] for r in ROLES]
    weights = [r[1] for r in ROLES]
    rolemeta = {r[0]: (r[2], r[3]) for r in ROLES}
    rows = []
    used_names = set()
    for i in range(1, count + 1):
        role = rng.choices(roles, weights)[0]
        clusters, title = rolemeta[role]
        ward = ward_for(clusters, rng)
        # name: fictional combination, keep (name) mostly unique by retrying
        for _ in range(6):
            name = f"{rng.choice(GIVEN)} {rng.choice(SURNAME)}"
            if name not in used_names:
                break
        used_names.add(name)
        # a minority hold a second role (e.g. Doctor + Emergency coordinator)
        role_list = [role]
        if rng.random() < 0.08:
            second = rng.choices(roles, weights)[0]
            if second != role:
                role_list.append(second)
        rows.append({
            "ResourceID": f"pers-{i:05d}",
            "name": name,
            "role_titles": "|".join(role_list),  # concept-list is pipe-delimited (seed convention)
            "contact_handle": contact_handle(role, name, rng),
            "primary_site": site_id_for_ward(ward),
            "notes": notes_for(role, title, ward["aonach_mor_label"], rng),
        })
    return rows


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--count", type=int, default=4000)
    ap.add_argument("--seed", type=int, default=1904)
    ap.add_argument("--out", type=Path, default=ROOT / "business_data/person.csv")
    ap.add_argument("--stdout", action="store_true")
    args = ap.parse_args()
    rows = generate(args.count, args.seed)
    fields = ["ResourceID", "name", "role_titles", "contact_handle", "primary_site", "notes"]
    out = sys.stdout if args.stdout else args.out.open("w", newline="")
    w = csv.DictWriter(out, fieldnames=fields)
    w.writeheader()
    for r in rows:
        w.writerow(r)
    if not args.stdout:
        out.close()
        print(f"wrote {len(rows)} persons to {args.out}", file=sys.stderr)


if __name__ == "__main__":
    main()

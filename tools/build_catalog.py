#!/usr/bin/env python3
"""Builds assets/emulators.json, the bundled emulator catalog.

Sources (both MIT licensed):
  - ES-DE's Linux system definitions (resources/systems/linux/es_systems.xml): every system
    with the emulators ES-DE offers for it. Labels ending in "(Standalone)" are standalone
    emulators; the rest are RetroArch cores, whose command names the core file.
  - libretro-core-info: per-core metadata. `corename` is the folder RetroArch saves into when
    "sort saves by core" is on, so it gives us the save path for free.

Usage:  python3 -I tools/build_catalog.py [output path]
Network access is needed; nothing from the downloads is executed, only parsed.
"""

import io
import json
import re
import sys
import tarfile
import urllib.request
import xml.etree.ElementTree as ET
from datetime import date

ESDE_URL = "https://gitlab.com/es-de/emulationstation-de/-/raw/master/resources/systems/linux/es_systems.xml"
CORE_INFO_URL = "https://codeload.github.com/libretro/libretro-core-info/tar.gz/refs/heads/master"

CORE_RE = re.compile(r"([A-Za-z0-9_\-]+)_libretro\.so")
EMULATOR_RE = re.compile(r"%EMULATOR_([A-Z0-9_\-]+)%")
# Launchers ES-DE lists as "emulators" that don't emulate anything or keep saves.
NOT_EMULATORS = {"os-shell", "wine", "proton", "umu"}


def clean_label(label: str) -> str:
    """'MAME [Diskette ADFS] (Standalone)' -> 'MAME'."""
    label = re.sub(r"\s*\(Standalone\)$", "", label)
    return re.sub(r"\s*\[[^\]]*\]", "", label).strip()


def fetch(url: str) -> bytes:
    with urllib.request.urlopen(url, timeout=60) as r:
        return r.read()


def core_info() -> dict:
    """core file stem -> {key: value} from every *_libretro.info file."""
    out = {}
    with tarfile.open(fileobj=io.BytesIO(fetch(CORE_INFO_URL)), mode="r:gz") as tar:
        for m in tar.getmembers():
            name = m.name.rsplit("/", 1)[-1]
            if not (m.isfile() and name.endswith("_libretro.info")):
                continue
            fields = {}
            for line in tar.extractfile(m).read().decode("utf-8", "replace").splitlines():
                key, sep, value = line.partition("=")
                if sep:
                    fields[key.strip()] = value.strip().strip('"')
            out[name[: -len("_libretro.info")]] = fields
    return out


def slug(s: str) -> str:
    return re.sub(r"[^a-z0-9]+", "-", s.lower()).strip("-")


def main() -> None:
    out_path = sys.argv[1] if len(sys.argv) > 1 else "assets/emulators.json"
    root = ET.fromstring(fetch(ESDE_URL))
    info = core_info()

    systems = []
    emulators = {}  # id -> entry
    labels = {}  # id -> every label seen; the shortest becomes the name

    for system in root.findall("system"):
        sys_id = system.findtext("name")
        systems.append({"id": sys_id, "name": system.findtext("fullname")})
        for cmd in system.findall("command"):
            label = (cmd.get("label") or "").strip()
            text = cmd.text or ""
            core = CORE_RE.search(text)
            if core:
                stem = core.group(1)
                emu_id = "ra-" + stem
                meta = info.get(stem, {})
                entry = emulators.setdefault(emu_id, {
                    "id": emu_id,
                    "name": label or meta.get("corename") or stem,
                    "kind": "retroarch",
                    "core": stem,
                    "corename": meta.get("corename"),
                    "systems": [],
                })
            else:
                found = [e for e in EMULATOR_RE.findall(text) if e != "RETROARCH"]
                if not found:
                    continue  # shortcuts, scripts, Steam launchers: not emulators
                emu_id = slug(found[0])
                if emu_id in NOT_EMULATORS:
                    continue
                entry = emulators.setdefault(emu_id, {
                    "id": emu_id,
                    "name": found[0].title(),
                    "kind": "standalone",
                    "systems": [],
                })
            if label:
                labels.setdefault(emu_id, set()).add(clean_label(label))
            if sys_id not in entry["systems"]:
                entry["systems"].append(sys_id)

    for emu_id, seen in labels.items():
        seen.discard("")
        if seen:
            emulators[emu_id]["name"] = min(seen, key=lambda l: (len(l), l))

    catalog = {
        "generated": date.today().isoformat(),
        "sources": {"es-de": ESDE_URL, "libretro-core-info": CORE_INFO_URL},
        "systems": systems,
        "emulators": sorted(emulators.values(), key=lambda e: e["id"]),
    }
    with open(out_path, "w") as f:
        json.dump(catalog, f, indent=1, ensure_ascii=False)
        f.write("\n")
    cores = sum(e["kind"] == "retroarch" for e in emulators.values())
    print(f"{out_path}: {len(systems)} systems, {len(emulators)} emulators ({cores} RetroArch cores)")


if __name__ == "__main__":
    main()

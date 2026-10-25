#!/usr/bin/env python3
"""One-time migration: for every multi-disc set marked "(Disc N)", write <Game>.m3u listing the
discs without extension, and rename each "<...> (Disc N) <...><ext>" to drop the extension (--ext, default .cue).

Usage: migrate_m3u.py ROOT SYSTEM [SYSTEM...] [--apply] --undo-log PATH
Dry-run unless --apply. Never overwrites anything.
"""
import argparse, collections, os, re, sys

DISC = re.compile(r" \(Disc (\d+)\)")

ap = argparse.ArgumentParser()
ap.add_argument("root")
ap.add_argument("systems", nargs="+")
ap.add_argument("--apply", action="store_true")
ap.add_argument("--undo-log", required=True)
ap.add_argument("--ext", default=".cue", help="disc file extension to strip, e.g. .cue or .iso")
a = ap.parse_args()

sets = collections.defaultdict(list)
for system in a.systems:
    d = os.path.join(a.root, system)
    for f in os.listdir(d):
        if not f.endswith(a.ext):
            continue
        m = DISC.search(f)
        if not m:
            continue
        prefix, suffix = f[: m.start()], f[m.end() : -len(a.ext)]
        sets[(d, prefix)].append((int(m.group(1)), suffix, f))

plan, errors = [], []
for (d, prefix), discs in sorted(sets.items()):
    discs.sort()
    nums = [n for n, _, _ in discs]
    if nums != list(range(1, len(nums) + 1)) or len(nums) < 2:
        errors.append(f"incomplete set {d}/{prefix}: discs {nums}")
        continue
    suffixes = {s for _, s, _ in discs}
    # Shared trailing tags (e.g. "(v1.1)") belong to the game; per-disc ones (e.g. "(GDI)") don't.
    name = prefix + (suffixes.pop() if len(suffixes) == 1 else "")
    m3u = os.path.join(d, name + ".m3u")
    if os.path.lexists(m3u):
        errors.append(f"exists: {m3u}")
    lines = []
    for _, _, f in discs:
        stem = f[: -len(a.ext)]
        src, dst = os.path.join(d, f), os.path.join(d, stem)
        if os.path.lexists(dst):
            errors.append(f"exists: {dst}")
        lines.append(stem)
        plan.append(("rename", src, dst))
    plan.append(("m3u", m3u, "\n".join(lines) + "\n"))

for e in errors:
    print("ERROR", e, file=sys.stderr)
for op in plan:
    if op[0] == "m3u":
        print(f"M3U    {op[1]}\n       " + op[2].rstrip("\n").replace("\n", "\n       "))
    else:
        print(f"RENAME {op[1]} -> {os.path.basename(op[2])}")
print(f"\n{len(sets)} sets, {sum(o[0]=='rename' for o in plan)} renames, "
      f"{sum(o[0]=='m3u' for o in plan)} m3u, {len(errors)} errors")

if errors or not a.apply:
    sys.exit(1 if errors else 0)

with open(a.undo_log, "x") as undo:
    for op in plan:
        if op[0] == "rename":
            _, src, dst = op
            if os.path.lexists(dst):
                sys.exit(f"refusing to overwrite {dst}")
            os.rename(src, dst)
            undo.write(f"RENAME\t{dst}\t{src}\n")
        else:
            _, path, body = op
            with open(path, "x", newline="\n") as fh:
                fh.write(body)
            undo.write(f"DELETE\t{path}\n")
        undo.flush()
print("applied; undo log:", a.undo_log)

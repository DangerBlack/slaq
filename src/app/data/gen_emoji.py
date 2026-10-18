#!/usr/bin/env python3
"""Rigenera emoji.tsv da iamcal/emoji-data.

È la stessa fonte da cui derivano i nomi di Slack, quindi copre gli alias che
altri set non hanno e le varianti di tono della pelle.

    curl -sO https://raw.githubusercontent.com/iamcal/emoji-data/master/emoji.json
    python3 gen_emoji.py emoji.json > emoji.tsv
"""
import json, sys

def ch(unified):
    return "".join(chr(int(p, 16)) for p in unified.split("-"))

# Slack chiama i toni skin-tone-2..6; Unicode usa Fitzpatrick 1F3FB..1F3FF.
TONES = {f"1F3F{c}": i for i, c in enumerate("BCDEF", start=2)}

rows, seen = [], set()
for e in json.load(open(sys.argv[1])):
    base = ch(e["unified"])
    for n in e.get("short_names", []):
        if n not in seen:
            seen.add(n); rows.append((n, base))
    for key, var in (e.get("skin_variations") or {}).items():
        # Le chiavi composte (due toni, per le coppie) non hanno nome Slack.
        if key not in TONES:
            continue
        glyph = ch(var["unified"])
        for n in e.get("short_names", []):
            name = f"{n}::skin-tone-{TONES[key]}"
            if name not in seen:
                seen.add(name); rows.append((name, glyph))

for n, g in sorted(rows):
    print(f"{n}\t{g}")

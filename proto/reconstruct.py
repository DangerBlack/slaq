"""Ricostruisce SignalingProtocol.proto dal modulo statico protobufjs.

Il repo amazon-chime-sdk-js versiona solo il generato, non lo schema. Le
funzioni `encode` però contengono tutto: numero di campo, wire type, nome e
— dalla presenza o meno della guardia `if (message.x != null)` — se il campo
è `required` o `optional` (lo schema è proto2).
"""
import re, sys
from collections import OrderedDict

src = open('SignalingProtocol.js', encoding='utf8').read()

WIRE = {
    'int32':'int32','uint32':'uint32','sint32':'sint32','int64':'int64',
    'uint64':'uint64','sint64':'sint64','bool':'bool','string':'string',
    'bytes':'bytes','double':'double','float':'float','fixed32':'fixed32',
    'sfixed32':'sfixed32','fixed64':'fixed64','sfixed64':'sfixed64',
}

def enum_types():
    """Estrae gli enum: `values[valuesById[N] = "NAME"] = N;`"""
    out = OrderedDict()
    for m in re.finditer(r'\$root\.(\w+) = \(function\(\) \{\s*var valuesById = \{\}, values = Object\.create\(valuesById\);(.*?)return values;', src, re.S):
        name, body = m.group(1), m.group(2)
        vals = re.findall(r'values\[valuesById\[(\d+)\] = "(\w+)"\] = \d+', body)
        if vals:
            out[name] = [(v, int(n)) for n, v in vals]
    return out

ENUMS = enum_types()

def parse_encode(body):
    """Un campo per riga logica; l'ordine di `id` non è garantito crescente."""
    fields = []
    # message / enum / scalar annidati via $root
    for m in re.finditer(
        r'(if \(message\.(\w+) != null[^\n]*\)\s*)?'
        r'\$root\.(\w+)\.encode\(message\.(\w+)(?:\[i\])?, writer\.uint32\(/\* id (\d+)',
        body):
        guarded, _, typ, fname, fid = m.groups()
        fields.append((int(fid), typ, fname, 'optional' if guarded else 'required'))

    # scalari: writer.uint32(/* id N, wireType W =*/K).TYPE(message.FIELD)
    for m in re.finditer(
        r'(if \(message\.(\w+) != null[^\n]*\)\s*)?'
        r'writer\.uint32\(/\* id (\d+), wireType \d+ =\*/\d+\)\.(\w+)\(message\.(\w+)\)',
        body):
        guarded, _, fid, wtype, fname = m.groups()
        if wtype in WIRE:
            fields.append((int(fid), WIRE[wtype], fname, 'optional' if guarded else 'required'))

    # repeated scalari packed: .fork(); for (...) writer.TYPE(message.FIELD[i])
    for m in re.finditer(
        r'writer\.uint32\(/\* id (\d+), wireType 2 =\*/\d+\)\.fork\(\);\s*'
        r'for \(var i = 0; i < message\.(\w+)\.length; \+\+i\)\s*'
        r'writer\.(\w+)\(message\.\2\[i\]\)', body):
        fid, fname, wtype = m.groups()
        if wtype in WIRE:
            fields.append((int(fid), WIRE[wtype], fname, 'repeated'))

    # repeated scalari non packed
    for m in re.finditer(
        r'for \(var i = 0; i < message\.(\w+)\.length; \+\+i\)\s*'
        r'writer\.uint32\(/\* id (\d+), wireType \d+ =\*/\d+\)\.(\w+)\(message\.\1\[i\]\)', body):
        fname, fid, wtype = m.groups()
        if wtype in WIRE:
            fields.append((int(fid), WIRE[wtype], fname, 'repeated'))

    # repeated messaggi: for (...) $root.T.encode(message.F[i], ...)
    for m in re.finditer(
        r'for \(var i = 0; i < message\.(\w+)\.length; \+\+i\)\s*'
        r'\$root\.(\w+)\.encode\(message\.\1\[i\]', body):
        fname, typ = m.groups()
        fields = [f for f in fields if f[2] != fname or f[1] != typ]
        mm = re.search(r'\$root\.%s\.encode\(message\.%s\[i\], writer\.uint32\(/\* id (\d+)' % (typ, fname), body)
        if mm:
            fields.append((int(mm.group(1)), typ, fname, 'repeated'))

    # dedup su (id, nome), preferendo repeated
    best = {}
    for fid, typ, fname, rule in fields:
        k = (fid, fname)
        if k not in best or rule == 'repeated':
            best[k] = (fid, typ, fname, rule)
    return sorted(best.values())

messages = OrderedDict()
for m in re.finditer(r'\$root\.(\w+) = \(function\(\) \{', src):
    name = m.group(1)
    if name in ENUMS:
        continue
    em = re.search(r'\n    %s\.encode = function encode\(message, writer, q\) \{(.*?)\n        return writer;' % re.escape(name), src, re.S)
    if em:
        messages[name] = parse_encode(em.group(1))

# enum annidati dichiarati dentro un messaggio: X.Y
nested_enums = {}
for m in re.finditer(r'\n    (\w+)\.(\w+) = \(function\(\) \{\s*var valuesById = \{\}, values = Object\.create\(valuesById\);(.*?)return values;', src, re.S):
    parent, ename, body = m.groups()
    vals = re.findall(r'values\[valuesById\[(\d+)\] = "(\w+)"\] = \d+', body)
    if vals:
        nested_enums.setdefault(parent, []).append((ename, [(v, int(n)) for n, v in vals]))

out = ['// RICOSTRUITO da SignalingProtocol.js (amazon-chime-sdk-js, Apache-2.0).',
       '// Il repo upstream versiona solo il codice generato, non lo schema.',
       '// Generato da reconstruct.py — non modificare a mano.',
       '',
       'syntax = "proto2";', '']

for name, vals in ENUMS.items():
    if any(name in [e[0] for e in v] for v in nested_enums.values()):
        continue
    out.append(f'enum {name} {{')
    for v, n in vals:
        out.append(f'  {v} = {n};')
    out.append('}\n')

for name, fields in messages.items():
    out.append(f'message {name} {{')
    for ename, vals in nested_enums.get(name, []):
        out.append(f'  enum {ename} {{')
        for v, n in vals:
            out.append(f'    {v} = {n};')
        out.append('  }')
    for fid, typ, fname, rule in fields:
        out.append(f'  {rule} {typ} {fname} = {fid};')
    out.append('}\n')

open('SignalingProtocol.proto','w').write('\n'.join(out))
print(f"messaggi: {len(messages)}  enum top-level: {len(ENUMS)}  enum annidati: {sum(len(v) for v in nested_enums.values())}")
print(f"campi totali: {sum(len(f) for f in messages.values())}")

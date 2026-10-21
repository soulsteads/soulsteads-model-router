#!/usr/bin/env python3
"""Enable only the two independently qualified physical image routes.

No secrets are read or printed beyond preserving existing policy text.
"""
import argparse
from pathlib import Path
import yaml

parser = argparse.ArgumentParser()
parser.add_argument('source', type=Path)
parser.add_argument('destination', type=Path)
args = parser.parse_args()
text = args.source.read_text()
lines = text.splitlines(keepends=True)
section = None
name = None
changed = set()
for i, line in enumerate(lines):
    if line.startswith('routes:'):
        section = 'routes'
    elif line.startswith('aliases:'):
        section = 'aliases'
    elif line and not line[0].isspace():
        section = None
    if section == 'routes' and line.startswith('  - name: '):
        name = line.split(':', 1)[1].strip()
    elif section == 'aliases' and line.startswith('  ') and not line.startswith('    '):
        name = line.split(':', 1)[0].strip()
    if section in {'routes', 'aliases'} and name in {'flash-2', 'qwen-4', 'qwen38auto', 'qwen38flash', 'qwen38'} and line.lstrip().startswith('capabilities: '):
        assert '[text,' in line and 'image' not in line, (section, name)
        lines[i] = line.replace('[text,', '[text, image,', 1)
        changed.add((section, name))
assert changed == {('routes', 'flash-2'), ('routes', 'qwen-4'), ('aliases', 'qwen38auto'), ('aliases', 'qwen38flash'), ('aliases', 'qwen38')}, changed
candidate = ''.join(lines)
yaml.safe_load(candidate)
args.destination.write_text(candidate)
print('qualified image declarations:', sorted(changed))

#!/usr/bin/env python3
"""Merge benchmark-levers.sh runs into one table per corpus.

    ./spikes/corpus/merge-levers.py benchmarks/fuse-levers/*.md

Each input holds one or more `## <repo> — <label>` sections with a two-column
table; the output has one column per input, in argument order.
"""
import re, sys
from collections import OrderedDict

runs = OrderedDict()   # repo -> OrderedDict(label -> OrderedDict(step -> value))
meta = {}
for path in sys.argv[1:]:
    repo = label = None
    for line in open(path, encoding="utf-8"):
        m = re.match(r"^## (\S+) — (.+)$", line)
        if m:
            repo, label = m.group(1), m.group(2).strip()
            runs.setdefault(repo, OrderedDict())[label] = OrderedDict()
            continue
        if repo and line.startswith("mount flags:") and repo not in meta:
            meta[repo] = line.strip()
        m = re.match(r"^\| (.+?) \| (.+?) \|$", line)
        if m and repo and m.group(1) not in ("step", "---"):
            runs[repo][label][m.group(1)] = m.group(2)
for repo, labels in runs.items():
    print(f"### {repo}\n")
    if repo in meta:
        print(meta[repo] + "\n")
    names = list(labels)
    print("| step | " + " | ".join(names) + " |")
    print("| --- | " + " | ".join("---:" for _ in names) + " |")
    steps = OrderedDict()
    for table in labels.values():
        for step in table:
            steps[step] = True
    for step in steps:
        cells = [labels[n].get(step, "–") for n in names]
        print(f"| {step} | " + " | ".join(cells) + " |")
    print()

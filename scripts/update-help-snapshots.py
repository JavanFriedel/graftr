#!/usr/bin/env python3
"""Explicit golden-file update; review the catalog/help changes before committing."""
import gzip
import json
import os
from pathlib import Path
import subprocess

root = Path(__file__).resolve().parents[1]
catalog = json.loads((root / "help-catalog.json").read_text())
pages = []
for topic in [None, *catalog["pages"], "help", *catalog["topics"]]:
    for columns in [1, 24, 32, 40, 60, 80, 120]:
        command = [str(root / "dist/graftr"), *(["--help"] if topic is None else ["help", topic])]
        text = subprocess.check_output(command, env={**os.environ, "COLUMNS": str(columns), "NO_COLOR": "1"}, text=True)
        pages.append({"topic": topic, "columns": columns, "text": text.removesuffix("\n")})
(root / "tests/fixtures/help.json.gz").write_bytes(gzip.compress(json.dumps(pages, ensure_ascii=False).encode(), mtime=0))
print(f"Updated {len(pages)} snapshots; review the help/catalog diff before committing.")

#!/usr/bin/env python3
"""Time commands and print nearest-rank percentiles.

Usage:
  measure.py N COMMAND [ARG...]
  measure.py --pair N "ENV=VAL COMMAND ARGS" "ENV=VAL COMMAND ARGS"

Drops 5 warmup runs (3 in pair mode), sends stdin, stdout and stderr to
/dev/null, and prints p50, p95 and p99.

Pair mode runs the two commands round-robin, one after the other, so a slow
drift in the machine hits both equally.
"""

import math
import os
import shlex
import subprocess
import sys
import time

null = subprocess.DEVNULL


def parse(spec):
    parts = shlex.split(spec)
    env = {}
    while parts and "=" in parts[0] and parts[0].split("=")[0].isupper():
        key, value = parts.pop(0).split("=", 1)
        env[key] = value
    label = os.path.basename(parts[0])
    return {"label": label, "cmd": parts, "env": env}


def percentile(values, q):
    return values[max(0, math.ceil(q / 100 * len(values)) - 1)]


def run(spec):
    start = time.perf_counter()
    proc = subprocess.run(
        spec["cmd"],
        stdin=null,
        stdout=null,
        stderr=subprocess.PIPE,
        env=dict(os.environ, **spec["env"]),
        text=True,
    )
    elapsed = (time.perf_counter() - start) * 1000
    if proc.returncode != 0:
        if not spec.get("warned"):
            last = proc.stderr.strip().splitlines()[-1:] or [""]
            print(f"{spec['label']}: rc={proc.returncode} {last[0]}")
            spec["warned"] = True
        return None
    return elapsed


def report(spec, samples):
    samples = sorted(samples)
    if not samples:
        print(f"{spec['label']:<12} no successful run")
        return
    print(
        f"{spec['label']:<12} n={len(samples):<3} p50={percentile(samples, 50):8.2f} ms  "
        f"p95={percentile(samples, 95):8.2f} ms  p99={percentile(samples, 99):8.2f} ms  "
        f"max={samples[-1]:8.2f} ms"
    )


if sys.argv[1] == "--pair":
    n = int(sys.argv[2])
    specs = [parse(spec) for spec in sys.argv[3:]]
    samples = [[] for _ in specs]
    for _ in range(3):
        for spec in specs:
            run(spec)
    for _ in range(n):
        for i, spec in enumerate(specs):
            elapsed = run(spec)
            if elapsed is not None:
                samples[i].append(elapsed)
    for spec, values in zip(specs, samples):
        report(spec, values)
else:
    n = int(sys.argv[1])
    spec = {"label": os.path.basename(sys.argv[2]), "cmd": sys.argv[2:], "env": {}}
    for _ in range(5):
        run(spec)
    report(spec, [v for v in (run(spec) for _ in range(n)) if v is not None])

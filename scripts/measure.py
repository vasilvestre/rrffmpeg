#!/usr/bin/env python3
"""Time a command N times and print nearest-rank percentiles.

Usage: measure.py N COMMAND [ARG...]

Drops 5 warmup runs, keeps N samples, and sends stdin, stdout and stderr to
/dev/null so the terminal does not pollute the measurement.
"""

import math
import subprocess
import sys
import time

n = int(sys.argv[1])
command = sys.argv[2:]
null = subprocess.DEVNULL

for _ in range(5):
    subprocess.run(command, stdin=null, stdout=null, stderr=null, check=True)

samples = []
for _ in range(n):
    start = time.perf_counter()
    subprocess.run(command, stdin=null, stdout=null, stderr=null, check=True)
    samples.append((time.perf_counter() - start) * 1000)

samples.sort()


def percentile(q):
    return samples[max(0, math.ceil(q / 100 * len(samples)) - 1)]


print(
    f"n={n}  p50={percentile(50):8.2f} ms  p95={percentile(95):8.2f} ms  "
    f"p99={percentile(99):8.2f} ms  max={samples[-1]:8.2f} ms"
)

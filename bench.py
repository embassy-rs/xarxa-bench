#!/usr/bin/env python3
"""Run the xarxa/smoltcp/lwIP benchmark matrix and chart the results.

For every combination of stack (xarxa, smoltcp, lwip), benchmark (tcp/udp x tx/rx) and
IP version (ipv4, ipv6) this builds the firmware, runs it on the Nucleo-F429ZI through
teleprobe, and reads back the `result` line the board prints after its measured window
(see `src/bench.rs`). Code size is `codesize.py`'s job, not this script's: it is not a
property of the benchmark firmware but of the feature set a firmware is built with.

    ./bench.py                      # run whatever is missing, then draw the charts
    ./bench.py --rerun              # re-run everything
    ./bench.py --only udp-rx        # just those cells (repeatable, matches any axis)
    ./bench.py --sweep              # measure each cell at every .text alignment, keep the best
    ./bench.py --render-only        # redraw from bench-results.json, run nothing

Results accumulate in bench-results.json, so an interrupted matrix resumes, and the
chart lands in bench-throughput.svg.

teleprobe needs TELEPROBE_HOST and TELEPROBE_TOKEN in the environment; the board it
runs on is named by `teleprobe_meta::target!` in src/main.rs. No ssh, no probe-rs, and
nothing to clean up afterwards: the firmware halts itself when it is done, so a
transmitting benchmark stops loading the link at the end of its run.

On this board a cell's throughput depends on *where* its code lands, not only on what
the code is: the F429's flash accelerator prefetches 128-bit lines, so a hot loop that
straddles one badly can cost half the throughput. It is not noise — every placement is
perfectly repeatable — and it has a period of 16 bytes. `TEXT_SHIFT=<n>` (see build.rs)
moves the whole of `.text` up by n bytes without changing a single instruction, and
`--sweep` runs each cell at all four 4-byte residues and keeps the best, so that no bar
in the chart is a placement artifact. The winning shift is recorded per cell.
"""

import argparse
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

from chart import LWIP, LWIP_LIGHTER, SMOLTCP, SMOLTCP_LIGHTER, XARXA, XARXA_LIGHTER, bar_chart

HERE = Path(__file__).parent.resolve()
ELF = HERE / "target/thumbv7em-none-eabihf/release/xarxa-example-stm32f429zi"
RESULTS = HERE / "bench-results.json"

STACKS = ["xarxa", "smoltcp", "lwip"]
BENCHES = ["udp-rx", "udp-tx", "tcp-rx", "tcp-tx"]
IPVS = ["ipv4", "ipv6"]

# ---------------------------------------------------------------------------
# Running
# ---------------------------------------------------------------------------

# What the board prints once its measured window is over, e.g.
#   INFO - 21.512101 result xarxa ipv4 udp rx: 95709 kbit/s over 15 s
RESULT_RE = re.compile(r"result (\w+) (ipv[46]) (\w+ \w+): (\d+) kbit/s")


# The .text placements a sweep tries. The effect has a period of 16 bytes (the flash
# accelerator's line), so these four cover every distinct placement there is.
SHIFTS = [0, 4, 8, 12]


def build(stack, bench, ipv, shift):
    """Build one cell at one .text placement."""
    features = f"stack-{stack},bench-{bench},bench-{ipv}"
    run(["cargo", "build", "--release", "--features", features], env={"TEXT_SHIFT": str(shift)})


def measure(stack, bench, ipv, retries, shifts):
    """Build and run one cell at each placement. Returns (best Mbit/s, shift)."""
    best = None
    for shift in shifts:
        build(stack, bench, ipv, shift)
        mbits = run_once(stack, bench, ipv, retries)
        if len(shifts) > 1:
            print(f"   shift={shift}: {mbits:.1f} Mbit/s", file=sys.stderr)
        if best is None or mbits > best[0]:
            best = (mbits, shift)
    return best


def run_once(stack, bench, ipv, retries):
    """Flash whatever is built and read back its result line, in Mbit/s."""
    for attempt in range(retries + 1):
        p = run(["teleprobe", "client", "run", "-s", str(ELF)], check=False)
        out = p.stdout + p.stderr
        for line in out.splitlines():
            m = RESULT_RE.search(line)
            if m:
                return int(m.group(4)) / 1000
        print(out, file=sys.stderr)
        print(f"!! no result line ({stack} {bench} {ipv}), attempt {attempt + 1}", file=sys.stderr)
        time.sleep(5)
    raise SystemExit(f"giving up on {stack} {bench} {ipv}")


def run(cmd, check=True, env=None):
    print(f"$ {' '.join(cmd)}", file=sys.stderr)
    p = subprocess.run(cmd, cwd=HERE, capture_output=True, text=True, env={**os.environ, **(env or {})})
    if check and p.returncode != 0:
        print(p.stdout + p.stderr, file=sys.stderr)
        raise SystemExit(f"command failed: {' '.join(cmd)}")
    return p


# One bar per (stack, IP version): the label, the two result keys, and the two colour
# steps chart.py defines for that stack.
SERIES = [
    ("xarxa v4", "xarxa", "ipv4", *XARXA),
    ("xarxa v6", "xarxa", "ipv6", *XARXA_LIGHTER),
    ("smoltcp v4", "smoltcp", "ipv4", *SMOLTCP),
    ("smoltcp v6", "smoltcp", "ipv6", *SMOLTCP_LIGHTER),
    ("lwip v4", "lwip", "ipv4", *LWIP),
    ("lwip v6", "lwip", "ipv6", *LWIP_LIGHTER),
]

# What `bar_chart` takes: label plus the colour steps, without the result keys.
CHART_SERIES = [(label, light, dark) for label, _, _, light, dark in SERIES]


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def key(stack, bench, ipv):
    return f"{stack}/{bench}/{ipv}"


def render(results):
    def cell(bench, field):
        row = []
        for _, stack, ipv, _, _ in SERIES:
            r = results.get(key(stack, bench, ipv))
            row.append(r[field] if r else None)
        return row

    have = [b for b in BENCHES if any(v is not None for v in cell(b, "mbits"))]
    if not have:
        print("no results yet", file=sys.stderr)
        return

    bar_chart(
        HERE / "bench-throughput.svg",
        "xarxa vs. smoltcp vs. lwIP on a Nucleo-F429ZI",
        f"payload throughput, 180 MHz, 100 Mbit/s link, {len(have)} benchmarks x 3 stacks x 2 IP versions",
        have,
        CHART_SERIES,
        [cell(b, "mbits") for b in have],
        lambda v: f"{v:.1f}",
        "Mbit/s",
    )

    # The same numbers as text, so the charts are never the only copy.
    print()
    print("| benchmark | " + " | ".join(s[0] for s in SERIES) + " |")
    print("|---|" + "---|" * len(SERIES))
    for b in have:
        cells = [f"{v:.1f}" if v is not None else "-" for v in cell(b, "mbits")]
        print(f"| {b} | " + " | ".join(cells) + " |")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--only", action="append", default=[], metavar="FILTER",
                    help="only cells matching this (stack, bench or IP version); repeatable")
    ap.add_argument("--rerun", action="store_true", help="re-run cells that already have a result")
    ap.add_argument("--render-only", action="store_true", help="draw the charts from the saved results")
    ap.add_argument("--retries", type=int, default=2, help="retries per cell before giving up (default 2)")
    ap.add_argument("--sweep", action="store_true",
                    help="measure each cell at all four .text alignments and keep the best")
    ap.add_argument("--shift", type=int, default=0, metavar="N",
                    help="build at this .text shift in bytes (default 0; ignored with --sweep)")
    args = ap.parse_args()

    results = json.loads(RESULTS.read_text()) if RESULTS.exists() else {}

    if not args.render_only:
        for var in ("TELEPROBE_HOST", "TELEPROBE_TOKEN"):
            if not os.environ.get(var):
                raise SystemExit(f"{var} is not set — teleprobe needs it to reach the board farm")

        todo = [
            (stack, bench, ipv)
            for bench in BENCHES
            for stack in STACKS
            for ipv in IPVS
            if all(f in (stack, bench, ipv) for f in args.only)
            and (args.rerun or key(stack, bench, ipv) not in results)
        ]
        for i, (stack, bench, ipv) in enumerate(todo, 1):
            print(f"\n=== [{i}/{len(todo)}] {stack} {bench} {ipv}", file=sys.stderr)
            shifts = SHIFTS if args.sweep else [args.shift]
            mbits, shift = measure(stack, bench, ipv, args.retries, shifts)
            print(f"=== {stack} {bench} {ipv}: {mbits:.1f} Mbit/s (shift={shift})", file=sys.stderr)
            results[key(stack, bench, ipv)] = {"mbits": mbits, "shift": shift}
            RESULTS.write_text(json.dumps(results, indent=1, sort_keys=True) + "\n")

    render(results)


if __name__ == "__main__":
    main()

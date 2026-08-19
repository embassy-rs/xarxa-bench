#!/usr/bin/env python3
"""Run the xarxa/smoltcp/lwIP benchmark matrix and chart the results.

For every combination of stack (xarxa, smoltcp, lwip), benchmark (tcp/udp x tx/rx) and
IP version (ipv4, ipv6) this builds the firmware, runs it on the Nucleo-F429ZI through
teleprobe, and reads back the `result` line the board prints after its measured window
(see `src/bench.rs`). Code size is taken from the same ELFs.

    ./bench.py                      # run whatever is missing, then draw the charts
    ./bench.py --rerun              # re-run everything
    ./bench.py --only udp-rx        # just those cells (repeatable, matches any axis)
    ./bench.py --sweep              # measure each cell at every .text alignment, keep the best
    ./bench.py --render-only        # redraw from bench-results.json, run nothing

Results accumulate in bench-results.json, so an interrupted matrix resumes, and the
charts land in bench-throughput.svg and bench-codesize.svg.

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
import math
import os
import re
import struct
import subprocess
import sys
import time
from pathlib import Path

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
    """Build one cell at one .text placement, and return its flash footprint in bytes."""
    features = f"stack-{stack},bench-{bench},{ipv}"
    run(["cargo", "build", "--release", "--features", features], env={"TEXT_SHIFT": str(shift)})
    return flash_size(ELF)


def measure(stack, bench, ipv, retries, shifts):
    """Build and run one cell at each placement. Returns (best Mbit/s, flash bytes, shift)."""
    best = None
    for shift in shifts:
        size = build(stack, bench, ipv, shift)
        mbits = run_once(stack, bench, ipv, retries)
        if len(shifts) > 1:
            print(f"   shift={shift}: {mbits:.1f} Mbit/s", file=sys.stderr)
        if best is None or mbits > best[0]:
            best = (mbits, size, shift)
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


def flash_size(path):
    """Bytes the ELF occupies in flash: every allocated section that has contents.

    That is .text + .rodata + the initializers for .data — i.e. what actually gets
    programmed — and it excludes .bss, which only takes RAM. Parsed straight out of the
    section headers so this script keeps its no-dependencies property.
    """
    data = path.read_bytes()
    assert data[:4] == b"\x7fELF" and data[4] == 1, "expected a 32-bit ELF"
    endian = "<" if data[5] == 1 else ">"
    e_shoff, _flags, _ehsize, _phentsize, _phnum, e_shentsize, e_shnum = struct.unpack_from(
        endian + "II5H", data, 0x20
    )
    total = 0
    SHT_NOBITS, SHF_ALLOC = 8, 0x2
    for i in range(e_shnum):
        off = e_shoff + i * e_shentsize
        _, sh_type, sh_flags, _, _, sh_size = struct.unpack_from(endian + "6I", data, off)
        if sh_flags & SHF_ALLOC and sh_type != SHT_NOBITS:
            total += sh_size
    return total


# ---------------------------------------------------------------------------
# Charts
#
# Hand-written SVG rather than a plotting library, so the script has no dependencies at
# all. Both charts are grouped bars: one group per benchmark, one bar per
# (stack, IP version).
#
# Colour carries the comparison the charts are about: hue is the *stack* — blue for
# xarxa, red-orange for smoltcp, green for lwIP — and within a stack the IPv6 bar is the
# lighter step of the same hue, so the three stacks separate at a glance and the family
# is a second-order read. Each mode has its own six steps (light bars sit on a light
# surface, dark on dark), swapped by CSS. Both sets clear the standard palette checks —
# lightness band, chroma floor, adjacent CVD and normal-vision separation — and the steps
# that fall below 3:1 against their surface are covered by the direct label every bar
# carries, so nothing is ever encoded by colour alone.
# ---------------------------------------------------------------------------

SERIES = [
    ("xarxa v4", "xarxa", "ipv4", "#2874d8", "#2858d0"),
    ("xarxa v6", "xarxa", "ipv6", "#7eace8", "#7392e0"),
    ("smoltcp v4", "smoltcp", "ipv4", "#dc4834", "#ac3420"),
    ("smoltcp v6", "smoltcp", "ipv6", "#ec9a8f", "#c97b6e"),
    ("lwip v4", "lwip", "ipv4", "#1d8a5c", "#137048"),
    ("lwip v6", "lwip", "ipv6", "#83c9ac", "#5fa98c"),
]

W, H = 1120, 520
PAD_L, PAD_R, PAD_T, PAD_B = 64, 24, 96, 76
PLOT_W = W - PAD_L - PAD_R
PLOT_H = H - PAD_T - PAD_B


def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def nice_ticks(vmax, count=5):
    """A tick step that is 1, 2 or 5 times a power of ten, covering vmax."""
    raw = vmax / count
    mag = 10 ** math.floor(math.log10(raw))
    step = next(mult * mag for mult in (1, 2, 2.5, 5, 10) if mult * mag >= raw)
    top = step * (int(vmax / step) + 1)
    ticks, t = [], 0.0
    while t <= top + step / 2:
        ticks.append(t)
        t += step
    return ticks, ticks[-1]


def bar_chart(path, title, subtitle, groups, values, fmt, axis_label):
    """values[group][series] -> number (or None to leave the bar out)."""
    flat = [v for row in values for v in row if v is not None]
    ticks, vmax = nice_ticks(max(flat))

    def y(v):
        return PAD_T + PLOT_H - PLOT_H * v / vmax

    n_groups, n_series = len(groups), len(SERIES)
    group_w = PLOT_W / n_groups
    # 2px of surface between neighbouring bars, and a gutter of roughly a quarter of a
    # group between groups.
    inner = group_w * 0.76
    bar_w = inner / n_series - 2

    out = []
    out.append(f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" font-family="system-ui, -apple-system, Segoe UI, Roboto, sans-serif">')
    out.append("<style>")
    out.append("""
    :root { color-scheme: light dark; }
    .surface { fill: #fcfcfb; }
    .t1 { fill: #0b0b0b; }
    .t2 { fill: #52514e; }
    .grid { stroke: #0b0b0b; stroke-opacity: 0.10; stroke-width: 1; }
    .axis { stroke: #0b0b0b; stroke-opacity: 0.28; stroke-width: 1; }
    """)
    for i, (_, _, _, light, _) in enumerate(SERIES):
        out.append(f".s{i} {{ fill: {light}; }}")
    out.append("""
    @media (prefers-color-scheme: dark) {
      .surface { fill: #1a1a19; }
      .t1 { fill: #ffffff; }
      .t2 { fill: #c3c2b7; }
      .grid { stroke: #ffffff; stroke-opacity: 0.12; }
      .axis { stroke: #ffffff; stroke-opacity: 0.32; }
    """)
    for i, (_, _, _, _, dark) in enumerate(SERIES):
        out.append(f".s{i} {{ fill: {dark}; }}")
    out.append("}")
    out.append("</style>")
    out.append(f'<rect width="{W}" height="{H}" class="surface"/>')

    out.append(f'<text x="{PAD_L}" y="34" class="t1" font-size="19" font-weight="600">{esc(title)}</text>')
    out.append(f'<text x="{PAD_L}" y="55" class="t2" font-size="13">{esc(subtitle)}</text>')

    # Legend, one row under the subtitle.
    lx = PAD_L
    for i, (label, *_rest) in enumerate(SERIES):
        out.append(f'<rect x="{lx}" y="{PAD_T - 27}" width="10" height="10" rx="2" class="s{i}"/>')
        out.append(f'<text x="{lx + 15}" y="{PAD_T - 18}" class="t2" font-size="12">{esc(label)}</text>')
        lx += 20 + 7.2 * len(label) + 18

    # Gridlines and the value axis.
    for t in ticks:
        out.append(f'<line x1="{PAD_L}" y1="{y(t):.1f}" x2="{PAD_L + PLOT_W}" y2="{y(t):.1f}" class="grid"/>')
        out.append(f'<text x="{PAD_L - 10}" y="{y(t) + 4:.1f}" class="t2" font-size="11" text-anchor="end">{t:g}</text>')
    out.append(f'<line x1="{PAD_L}" y1="{y(0):.1f}" x2="{PAD_L + PLOT_W}" y2="{y(0):.1f}" class="axis"/>')
    out.append(f'<text x="{PAD_L - 46}" y="{PAD_T + PLOT_H / 2:.1f}" class="t2" font-size="12" text-anchor="middle" transform="rotate(-90 {PAD_L - 46} {PAD_T + PLOT_H / 2:.1f})">{esc(axis_label)}</text>')

    for g, group in enumerate(groups):
        gx = PAD_L + g * group_w
        for s in range(n_series):
            v = values[g][s]
            if v is None:
                continue
            x = gx + (group_w - inner) / 2 + s * (bar_w + 2)
            h = max(PLOT_H * v / vmax, 3)
            # 4px rounded data-end, squared off at the baseline.
            out.append(
                f'<path d="M{x:.1f} {y(0):.1f} v{-(h - 4):.1f} a4 4 0 0 1 4 -4 h{bar_w - 8:.1f} '
                f'a4 4 0 0 1 4 4 v{h - 4:.1f} z" class="s{s}"/>'
            )
            out.append(
                f'<text x="{x + bar_w / 2:.1f}" y="{y(v) - 7:.1f}" class="t1" font-size="11" '
                f'text-anchor="middle">{fmt(v)}</text>'
            )
        out.append(
            f'<text x="{gx + group_w / 2:.1f}" y="{PAD_T + PLOT_H + 22}" class="t1" font-size="13" '
            f'text-anchor="middle">{esc(group)}</text>'
        )

    out.append("</svg>")
    path.write_text("\n".join(out))
    print(f"wrote {path.relative_to(HERE)}")


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
        [cell(b, "mbits") for b in have],
        lambda v: f"{v:.1f}",
        "Mbit/s",
    )
    bar_chart(
        HERE / "bench-codesize.svg",
        "Firmware flash footprint",
        "whole image: stack + ethernet driver + benchmark, .text + .rodata + .data initializers",
        have,
        [[v / 1024 if v else None for v in cell(b, "flash")] for b in have],
        lambda v: f"{v:.1f}",
        "KiB",
    )

    # The same numbers as text, so the charts are never the only copy.
    print()
    print("| benchmark | " + " | ".join(s[0] for s in SERIES) + " |")
    print("|---|" + "---|" * len(SERIES))
    for b in have:
        cells = [f"{v:.1f}" if v is not None else "-" for v in cell(b, "mbits")]
        print(f"| {b} | " + " | ".join(cells) + " |")
    print()
    print("flash, KiB:")
    for b in have:
        cells = [f"{v / 1024:.1f}" if v else "-" for v in cell(b, "flash")]
        print(f"  {b:8} " + "  ".join(f"{c:>7}" for c in cells))


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
            mbits, flash, shift = measure(stack, bench, ipv, args.retries, shifts)
            print(
                f"=== {stack} {bench} {ipv}: {mbits:.1f} Mbit/s, {flash / 1024:.1f} KiB flash"
                f" (shift={shift})",
                file=sys.stderr,
            )
            results[key(stack, bench, ipv)] = {"mbits": mbits, "flash": flash, "shift": shift}
            RESULTS.write_text(json.dumps(results, indent=1, sort_keys=True) + "\n")

    render(results)


if __name__ == "__main__":
    main()

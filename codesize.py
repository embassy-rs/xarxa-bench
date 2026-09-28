#!/usr/bin/env python3
"""Compare the flash footprint of xarxa, smoltcp and lwIP across representative builds.

    ./codesize.py                    # build the whole ladder, all three stacks
    ./codesize.py --only bare,full   # some rungs of it
    ./codesize.py --render-only      # redraw from codesize-results.json
    ./codesize.py ipv4,udp,tcp       # ad-hoc feature combinations, not charted

The features are the protocol features of `Cargo.toml`, which mirror xarxa's own. Each
enables the closest equivalent in smoltcp and lwIP. `medium-ethernet` is always added.

The binary is `src/codesize/main.rs`, which makes plain use of every enabled feature so
the linker keeps it. Sizes are whole-image flash bytes (.text + .rodata + .data), driver
and runtime included. Results cache in codesize-results.json; the chart lands in
bench-codesize.svg.
"""

import argparse
import json
import struct
import subprocess
import sys
from pathlib import Path

from chart import LWIP, SMOLTCP, XARXA, bar_chart

HERE = Path(__file__).parent.resolve()
ELF = HERE / "target/thumbv7em-none-eabihf/codesize/codesize"
RESULTS = HERE / "codesize-results.json"

SERIES = [("xarxa", *XARXA), ("smoltcp", *SMOLTCP), ("lwip", *LWIP)]
STACKS = [s[0] for s in SERIES]

# Each rung is a superset of the previous one, and holds only features all three stacks
# have (listening is part of TCP on smoltcp and lwIP, lwIP's TCP is always Reno, and it
# answers pings whenever ICMP is on). `full` is the whole three-way intersection.
_BARE = ["ipv4", "udp"]
_TCP = _BARE + ["tcp", "tcp-listener", "tcp-reno"]
_CLIENT = _TCP + ["dhcpv4", "dns", "icmp-ping-reply", "ipv4-reassembly"]
_FULL = _CLIENT + ["ipv6", "slaac", "mdns", "multicast", "ipv4-fragmentation", "raw-ip"]

LADDER = [
    ("bare", "IPv4 + UDP", _BARE),
    ("tcp", "+ TCP, listener, Reno", _TCP),
    ("client", "+ DHCP, DNS, ping, reassembly", _CLIENT),
    ("full", "+ IPv6, SLAAC, mDNS, multicast, fragmentation, raw IP", _FULL),
]


def build(stack, features):
    """Build the probe; return flash bytes, or None if the stack can't build it."""
    feats = ",".join(["stack-" + stack, "codesize", "medium-ethernet"] + features)
    # The `codesize` profile aborts on panic, which needs core rebuilt to match.
    cmd = [
        "cargo", "build", "--profile", "codesize", "-Zbuild-std=core",
        "--bin", "codesize", "--features", feats,
    ]
    print(f"$ {' '.join(cmd)}", file=sys.stderr)
    p = subprocess.run(cmd, cwd=HERE, capture_output=True, text=True)
    if p.returncode != 0:
        print(p.stdout + p.stderr, file=sys.stderr)
        print(f"!! {stack} cannot build this combination, skipping", file=sys.stderr)
        return None
    return flash_size(ELF)


def flash_size(path):
    """Sum of allocated sections with contents (excludes .bss)."""
    data = path.read_bytes()
    assert data[:4] == b"\x7fELF" and data[4] == 1, "expected a 32-bit ELF"
    endian = "<" if data[5] == 1 else ">"
    e_shoff, _, _, _, _, e_shentsize, e_shnum = struct.unpack_from(endian + "II5H", data, 0x20)
    total = 0
    SHT_NOBITS, SHF_ALLOC = 8, 0x2
    for i in range(e_shnum):
        _, sh_type, sh_flags, _, _, sh_size = struct.unpack_from(endian + "6I", data, e_shoff + i * e_shentsize)
        if sh_flags & SHF_ALLOC and sh_type != SHT_NOBITS:
            total += sh_size
    return total


def render(rows, chart):
    """Print a table of `rows` (name, subtitle, {stack: bytes or None}), optionally chart it."""
    if chart:
        bar_chart(
            HERE / "bench-codesize.svg",
            "Firmware flash footprint, by feature set",
            "whole image: stack + ethernet driver + probe, .text + .rodata + .data initializers",
            [name for name, _, _ in rows],
            SERIES,
            [[None if sizes[s] is None else sizes[s] / 1024 for s in STACKS] for _, _, sizes in rows],
            lambda v: f"{v:.1f}",
            "KiB",
        )

    width = max(len("feature set"), *(len(name) for name, _, _ in rows))
    sub_width = max(len(sub) for _, sub, _ in rows)
    print()
    print(f"{"feature set":<{width}}  {"":<{sub_width}}  " + "".join(f"{s:>10}" for s in STACKS) + "   (flash KiB)")
    for name, sub, sizes in rows:
        cells = "".join(f"{sizes[s] / 1024:>10.1f}" if sizes[s] is not None else f"{'n/a':>10}" for s in STACKS)
        print(f"{name:<{width}}  {sub:<{sub_width}}  " + cells)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("combos", nargs="*", metavar="FEATURES", help="ad-hoc comma-separated feature sets")
    ap.add_argument("--only", default="", metavar="NAMES", help="comma-separated rung names to build")
    ap.add_argument("--rerun", action="store_true", help="rebuild rungs that already have a result")
    ap.add_argument("--render-only", action="store_true", help="draw the chart from the saved results")
    args = ap.parse_args()

    if args.combos:
        rows = []
        for c in args.combos:
            features = sorted({f.strip() for f in c.split(",") if f.strip()} - {"medium-ethernet"})
            rows.append((",".join(features), "ad-hoc", {s: build(s, features) for s in STACKS}))
        render(rows, chart=False)
        return

    names = [n.strip() for n in args.only.split(",") if n.strip()]
    known = [name for name, _, _ in LADDER]
    for n in names:
        if n not in known:
            raise SystemExit(f"unknown rung {n!r} (choose from {', '.join(known)})")
    wanted = [r for r in LADDER if not names or r[0] in names]

    results = json.loads(RESULTS.read_text()) if RESULTS.exists() else {}
    if not args.render_only:
        for name, _, features in wanted:
            for stack in STACKS:
                k = f"{stack}/{name}"
                # A cached result is only reused if it was built from the same features.
                old = results.get(k)
                if old is not None and old.get("features") == features and not args.rerun:
                    continue
                results[k] = {"features": features, "flash": build(stack, features)}
                RESULTS.write_text(json.dumps(results, indent=1, sort_keys=True) + "\n")

    rows = []
    for name, sub, _ in wanted:
        sizes = {s: (results.get(f"{s}/{name}") or {}).get("flash") for s in STACKS}
        if any(v is not None for v in sizes.values()):
            rows.append((name, sub, sizes))
    if not rows:
        print("no results yet", file=sys.stderr)
        return
    render(rows, chart=True)


if __name__ == "__main__":
    main()

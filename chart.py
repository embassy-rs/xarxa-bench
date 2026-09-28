"""Grouped-bar SVG charts, hand-written so the benchmark scripts have no dependencies.

Both `bench.py` (throughput) and `codesize.py` (flash footprint) draw the same shape:
one group per benchmark or feature set, one bar per series, a direct label on every bar.

Colour carries the comparison the charts are about: hue is the *stack* — blue for xarxa,
red-orange for smoltcp, green for lwIP. Where a chart splits a stack further (bench.py
puts IPv4 and IPv6 side by side) the second bar is the lighter step of the same hue, so
the three stacks separate at a glance and the subdivision is a second-order read. Each
mode has its own steps (light bars sit on a light surface, dark on dark), swapped by CSS.
Both sets clear the standard palette checks — lightness band, chroma floor, adjacent CVD
and normal-vision separation — and the steps that fall below 3:1 against their surface
are covered by the direct label every bar carries, so nothing is ever encoded by colour
alone.

A series is `(label, light, dark)`. Callers that key results off more than the label
(bench.py needs the stack and IP version) keep that mapping on their own side.
"""

import math

# The per-stack hues, each as (light-mode, dark-mode). `LIGHTER` is the second step of
# the same hue, for charts that put two bars per stack.
XARXA = ("#2874d8", "#2858d0")
SMOLTCP = ("#dc4834", "#ac3420")
LWIP = ("#1d8a5c", "#137048")
XARXA_LIGHTER = ("#7eace8", "#7392e0")
SMOLTCP_LIGHTER = ("#ec9a8f", "#c97b6e")
LWIP_LIGHTER = ("#83c9ac", "#5fa98c")

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


def bar_chart(path, title, subtitle, groups, series, values, fmt, axis_label):
    """Write a grouped bar chart.

    `groups` labels the x axis, `series` is a list of `(label, light, dark)`, and
    `values[group][series]` is the number for one bar, or None to leave it out (which
    is how a stack that cannot build a combination is shown: a gap and an "n/a").
    """
    flat = [v for row in values for v in row if v is not None]
    ticks, vmax = nice_ticks(max(flat))

    def y(v):
        return PAD_T + PLOT_H - PLOT_H * v / vmax

    n_groups, n_series = len(groups), len(series)
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
    for i, (_, light, _) in enumerate(series):
        out.append(f".s{i} {{ fill: {light}; }}")
    out.append("""
    @media (prefers-color-scheme: dark) {
      .surface { fill: #1a1a19; }
      .t1 { fill: #ffffff; }
      .t2 { fill: #c3c2b7; }
      .grid { stroke: #ffffff; stroke-opacity: 0.12; }
      .axis { stroke: #ffffff; stroke-opacity: 0.32; }
    """)
    for i, (_, _, dark) in enumerate(series):
        out.append(f".s{i} {{ fill: {dark}; }}")
    out.append("}")
    out.append("</style>")
    out.append(f'<rect width="{W}" height="{H}" class="surface"/>')

    out.append(f'<text x="{PAD_L}" y="34" class="t1" font-size="19" font-weight="600">{esc(title)}</text>')
    out.append(f'<text x="{PAD_L}" y="55" class="t2" font-size="13">{esc(subtitle)}</text>')

    # Legend, one row under the subtitle.
    lx = PAD_L
    for i, (label, *_rest) in enumerate(series):
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
            x = gx + (group_w - inner) / 2 + s * (bar_w + 2)
            v = values[g][s]
            if v is None:
                # No bar, but say so where the bar would have been, so a gap is never
                # read as a zero.
                out.append(
                    f'<text x="{x + bar_w / 2:.1f}" y="{y(0) - 7:.1f}" class="t2" font-size="10" '
                    f'text-anchor="middle">n/a</text>'
                )
                continue
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
    print(f"wrote {path.name}")

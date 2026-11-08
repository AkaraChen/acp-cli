#!/usr/bin/env python3
"""Render tmux `capture-pane -e -N` frames into site/index.html.

Usage: site/tools/ansi2html.py   (reads site/frames/N.ans, rewrites the
<!-- frame:N --> ... <!-- /frame:N --> regions of site/index.html)

Colours become classes (t-green, t-blue, ...) so the page theme decides
the actual values.
"""

import html
import pathlib
import re

SITE = pathlib.Path(__file__).resolve().parent.parent
SGR = re.compile(r"\x1b\[([0-9;]*)m")

BASIC = {1: "red", 2: "green", 3: "yellow", 4: "blue", 5: "magenta", 6: "cyan"}
ANCHORS = {
    "red": (243, 139, 168),
    "green": (166, 227, 161),
    "yellow": (246, 226, 183),
    "blue": (99, 168, 248),
    "magenta": (203, 166, 247),
    "cyan": (137, 220, 235),
}


def rgb_class(r, g, b):
    if max(r, g, b) - min(r, g, b) < 24:
        return None  # greys read as the default foreground
    return min(ANCHORS, key=lambda k: sum((a - c) ** 2 for a, c in zip(ANCHORS[k], (r, g, b))))


def render(text):
    state = {"b": False, "d": False, "i": False, "u": False, "r": False, "fg": None, "bg": False}
    out = []

    def classes():
        c = [k for k in ("b", "d", "i", "u", "r") if state[k]]
        c = ["t-" + k for k in c]
        if state["fg"]:
            c.append("t-" + state["fg"])
        if state["bg"]:
            c.append("t-bg")
        return " ".join(c)

    def apply(params):
        p = [int(x) if x else 0 for x in params.split(";")] if params else [0]
        i = 0
        while i < len(p):
            n = p[i]
            if n == 0:
                state.update(b=False, d=False, i=False, u=False, r=False, fg=None, bg=False)
            elif n == 1:
                state["b"] = True
            elif n == 2:
                state["d"] = True
            elif n == 3:
                state["i"] = True
            elif n == 4:
                state["u"] = True
            elif n == 7:
                state["r"] = True
            elif n == 22:
                state["b"] = state["d"] = False
            elif n == 23:
                state["i"] = False
            elif n == 24:
                state["u"] = False
            elif n == 27:
                state["r"] = False
            elif 30 <= n <= 37:
                state["fg"] = BASIC.get(n - 30)
            elif 90 <= n <= 97:
                state["fg"] = BASIC.get(n - 90)
            elif n == 39:
                state["fg"] = None
            elif 40 <= n <= 47 or 100 <= n <= 107:
                state["bg"] = True
            elif n == 49:
                state["bg"] = False
            elif n in (38, 48) and i + 1 < len(p):
                if p[i + 1] == 5 and i + 2 < len(p):
                    if n == 38:
                        state["fg"] = BASIC.get(p[i + 2] % 8)
                    else:
                        state["bg"] = True
                    i += 2
                elif p[i + 1] == 2 and i + 4 < len(p):
                    if n == 38:
                        state["fg"] = rgb_class(*p[i + 2 : i + 5])
                    else:
                        state["bg"] = True
                    i += 4
            i += 1

    rows = []
    for line in text.rstrip("\n").split("\n"):
        cells = []
        pos = 0
        for m in SGR.finditer(line):
            chunk = line[pos : m.start()]
            if chunk:
                cells.append(span(chunk, classes()))
            apply(m.group(1))
            pos = m.end()
        chunk = line[pos:]
        if chunk:
            cells.append(span(chunk, classes()))
        # One block per terminal row, so background runs fill the row height.
        rows.append('<span class="t-row">' + "".join(cells) + "</span>")
    return "".join(rows)


def span(text, cls):
    text = html.escape(text, quote=False)
    return f'<span class="{cls}">{text}</span>' if cls else text


def main():
    index = SITE / "index.html"
    page = index.read_text()
    for frame in sorted((SITE / "frames").glob("*.ans")):
        n = frame.stem
        body = render(frame.read_text())
        page, count = re.subn(
            rf"(<!-- frame:{n} -->).*?(<!-- /frame:{n} -->)",
            lambda m: m.group(1) + body + m.group(2),
            page,
            flags=re.S,
        )
        if not count:
            raise SystemExit(f"no <!-- frame:{n} --> region in {index}")
    index.write_text(page)


if __name__ == "__main__":
    main()

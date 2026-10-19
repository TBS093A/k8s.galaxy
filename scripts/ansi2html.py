#!/usr/bin/env python3
"""Render a `tmux capture-pane -e` dump (ANSI colours) to an HTML page for screenshots.

Usage: ansi2html.py capture.ansi out.html   (see scripts/screenshot.sh)
"""

import html
import re
import sys

SGR = re.compile(r"\x1b\[([0-9;]*)m")
BG, FG = "#04070c", "#c8dce6"
PAL = ["#000000", "#cd0000", "#00cd00", "#cdcd00", "#0000ee", "#cd00cd", "#00cdcd", "#e5e5e5",
       "#7f7f7f", "#ff0000", "#00ff00", "#ffff00", "#5c5cff", "#ff00ff", "#00ffff", "#ffffff"]


def x256(n: int) -> str:
    if n < 16:
        return PAL[n]
    if n < 232:
        n -= 16
        c = [0, 95, 135, 175, 215, 255]
        return "#%02x%02x%02x" % (c[n // 36], c[n // 6 % 6], c[n % 6])
    v = 8 + (n - 232) * 10
    return "#%02x%02x%02x" % (v, v, v)


def convert(text: str) -> str:
    st = {"fg": None, "bg": None, "b": False}
    out = []
    for line in text.rstrip("\n").split("\n"):
        pos = 0
        parts = []
        for m in list(SGR.finditer(line)) + [None]:
            chunk = line[pos:m.start() if m else len(line)]
            if chunk:
                css = []
                if st["fg"]:
                    css.append("color:" + st["fg"])
                if st["bg"]:
                    css.append("background:" + st["bg"])
                if st["b"]:
                    css.append("font-weight:bold")
                e = html.escape(chunk)
                parts.append(f'<span style="{";".join(css)}">{e}</span>' if css else e)
            if not m:
                break
            pos = m.end()
            codes = [int(c) if c else 0 for c in m.group(1).split(";")] or [0]
            i = 0
            while i < len(codes):
                c = codes[i]
                if c == 0:
                    st = {"fg": None, "bg": None, "b": False}
                elif c == 1:
                    st["b"] = True
                elif c == 22:
                    st["b"] = False
                elif c in (38, 48) and i + 1 < len(codes):
                    key = "fg" if c == 38 else "bg"
                    if codes[i + 1] == 2:
                        st[key] = "#%02x%02x%02x" % tuple(codes[i + 2:i + 5])
                        i += 4
                    elif codes[i + 1] == 5:
                        st[key] = x256(codes[i + 2])
                        i += 2
                elif c == 39:
                    st["fg"] = None
                elif c == 49:
                    st["bg"] = None
                elif 30 <= c <= 37:
                    st["fg"] = PAL[c - 30]
                elif 90 <= c <= 97:
                    st["fg"] = PAL[c - 82]
                elif 40 <= c <= 47:
                    st["bg"] = PAL[c - 40]
                i += 1
        out.append("".join(parts))
    return "\n".join(out)


def main(src: str, dst: str) -> None:
    body = convert(open(src, encoding="utf-8").read())
    with open(dst, "w", encoding="utf-8") as f:
        f.write(f"""<!doctype html><meta charset="utf-8"><style>
html,body{{margin:0;background:{BG}}}
pre{{margin:0;padding:12px 14px;color:{FG};font:15px/1.13 'CaskaydiaCove NFM','CaskaydiaCove Nerd Font Mono',monospace;white-space:pre}}
</style><pre>{body}</pre>""")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])

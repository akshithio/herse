"""Builds herse-theme.json: GPUI Kit's default theme with Herse's palette.

Herse's design language (see DESIGN.md): warm neutrals, paper and ink, and
one accent, "dew" (a deep teal), for where you are and what is live:
selection, editing, focus, the caret, links. Hierarchy comes from weight,
size and value contrast; other colour is kept for information (syntax,
errors, charts).

    python3 make_theme.py   # from this directory
"""
import json, os, re

HERE = os.path.dirname(os.path.abspath(__file__))
SRC = os.path.join(HERE, "../../../vendor/gpui-component/src/theme/default-theme.json")

# Tailwind neutral -> warm stone-like tones.
NEUTRAL = {
    "neutral-50": "#FAF9F7", "neutral-100": "#F2F0ED", "neutral-200": "#E8E6E2",
    "neutral-300": "#D6D2CC", "neutral-400": "#A8A29C", "neutral-500": "#78736C",
    "neutral-600": "#57534E", "neutral-700": "#3F3B37", "neutral-800": "#2B2927",
    "neutral-900": "#1C1A17", "neutral-950": "#141312",
}

def warm_hex(m):
    """#rrggbb[aa] greys -> the same lightness with a warm tint."""
    h = m.group(0)
    r, g, b = (int(h[i:i + 2], 16) for i in (1, 3, 5))
    alpha = h[7:9]
    if not (abs(r - g) <= 2 and abs(g - b) <= 2):
        return h
    v = r
    dg, db = (2, 5) if v > 128 else (1, 3)
    if v in (0, 255):
        return h
    return "#%02X%02X%02X%s" % (v, max(v - dg, 0), max(v - db, 0), alpha)

def warm(value):
    if not isinstance(value, str):
        return value
    for name, hexv in NEUTRAL.items():
        value = re.sub(rf"\b{name}\b", hexv, value)
    return re.sub(r"#[0-9a-fA-F]{6}([0-9a-fA-F]{2})?\b", warm_hex, value)

LIGHT = {
    "background": "#FFFFFF",
    "foreground": "#1C1A17",
    "border": "#E8E6E2",
    "input.border": "#E2DFDA",
    "muted.background": "#F2F0ED",
    "muted.foreground": "#78736C",
    "accent.background": "#F2F0ED",
    "accent.foreground": "#1C1A17",
    "secondary.background": "#EAE7E3",
    "secondary.hover.background": "#E4E1DC",
    "secondary.active.background": "#DCD8D2",
    "secondary.foreground": "#1C1A17",
    "primary.background": "#0B7A75",
    "primary.hover.background": "#096A66",
    "primary.active.background": "#075955",
    "primary.foreground": "#FFFFFF",
    "popover.background": "#FFFFFF",
    "popover.foreground": "#1C1A17",
    "list.background": "#FFFFFF",
    "list.hover.background": "#F2F0ED",
    "list.active.background": "#EAE7E3",
    "list.active.border": "#00000000",
    "list.even.background": "#FAF9F7",
    "list.head.background": "#FAF9F7",
    "sidebar.background": "#F7F6F4",
    "sidebar.border": "#E8E6E2",
    "sidebar.foreground": "#1C1A17",
    "sidebar.accent.background": "#EAE7E3",
    "sidebar.accent.foreground": "#1C1A17",
    "tab_bar.background": "#F7F6F4",
    "tab.background": "#00000000",
    "tab.active.background": "#FFFFFF",
    "tab.foreground": "#78736C",
    "tab.active.foreground": "#1C1A17",
    "title_bar.background": "#F7F6F4",
    "title_bar.border": "#E8E6E2",
    "selection.background": "#CFE6E3",
    "caret": "#0B7A75",
    "ring": "#0B7A75",
    "link.foreground": "#0B7A75",
    "link.hover.foreground": "#075955",
    "link.active.foreground": "#075955",
    "scrollbar.background": "#FFFFFF00",
    "scrollbar.thumb.background": "#C9C4BDCC",
    "scrollbar.thumb.hover.background": "#B3ADA5",
    "drag_border": "#0B7A75",
    "drop_target.background": "#0B7A751F",
    "skeleton.background": "#F2F0ED",
    "table.background": "#FFFFFF",
    "table.head.foreground": "#78736C",
    "table.active.background": "#0B7A7514",
    "table.active.border": "#0B7A75",
}

DARK = {
    "background": "#191817",
    "foreground": "#ECEAE6",
    "border": "#2B2927",
    "input.border": "#34312E",
    "muted.background": "#23211F",
    "muted.foreground": "#9C968F",
    "accent.background": "#262422",
    "accent.foreground": "#ECEAE6",
    "secondary.background": "#2A2826",
    "secondary.hover.background": "#302D2A",
    "secondary.active.background": "#36332F",
    "secondary.foreground": "#ECEAE6",
    "primary.background": "#5CC8BE",
    "primary.hover.background": "#73D3CA",
    "primary.active.background": "#8ADDD5",
    "primary.foreground": "#0F1A19",
    "popover.background": "#1F1D1B",
    "popover.foreground": "#ECEAE6",
    "list.background": "#191817",
    "list.hover.background": "#23211F",
    "list.active.background": "#2E2B28",
    "list.active.border": "#00000000",
    "list.even.background": "#1C1B19",
    "list.head.background": "#1C1B19",
    "sidebar.background": "#141312",
    "sidebar.border": "#262422",
    "sidebar.foreground": "#ECEAE6",
    "sidebar.accent.background": "#2A2826",
    "sidebar.accent.foreground": "#ECEAE6",
    "tab_bar.background": "#141312",
    "tab.background": "#00000000",
    "tab.active.background": "#191817",
    "tab.foreground": "#9C968F",
    "tab.active.foreground": "#ECEAE6",
    "title_bar.background": "#141312",
    "title_bar.border": "#262422",
    "selection.background": "#2C4C48",
    "caret": "#5CC8BE",
    "ring": "#5CC8BE",
    "link.foreground": "#5CC8BE",
    "link.hover.foreground": "#8ADDD5",
    "link.active.foreground": "#8ADDD5",
    "scrollbar.background": "#19181700",
    "scrollbar.thumb.background": "#4A4642CC",
    "scrollbar.thumb.hover.background": "#5C5752",
    "drag_border": "#5CC8BE",
    "drop_target.background": "#5CC8BE1F",
    "skeleton.background": "#23211F",
    "table.background": "#191817",
    "table.head.background": "#1F1D1B",
    "table.head.foreground": "#B8B2AB",
    "table.even.background": "#1C1B19",
    "table.hover.background": "#23211F",
    "table.active.background": "#5CC8BE1F",
    "table.active.border": "#5CC8BE",
    "table.row.border": "#2B2927B3",
}

def syntax(p):
    return {
        "attribute": {"color": p["type"]},
        "boolean": {"color": p["number"]},
        "comment": {"color": p["comment"], "font_style": "italic"},
        "comment.doc": {"color": p["comment"], "font_style": "italic"},
        "constant": {"color": p["number"]},
        "constructor": {"color": p["type"]},
        "embedded": {"color": p["fg"]},
        "emphasis": {"font_style": "italic"},
        "emphasis.strong": {"font_weight": 700},
        "function": {"color": p["function"]},
        "keyword": {"color": p["keyword"]},
        "link_text": {"color": p["function"], "font_style": "normal"},
        "link_uri": {"color": p["comment"], "font_style": "italic"},
        "number": {"color": p["number"]},
        "operator": {"color": p["operator"]},
        "punctuation": {"color": p["punct"]},
        "property": {"color": p["fg"]},
        "string": {"color": p["string"]},
        "string.escape": {"color": p["escape"]},
        "string.regex": {"color": p["escape"]},
        "string.special": {"color": p["escape"]},
        "string.special.symbol": {"color": p["number"]},
        "tag": {"color": p["keyword"]},
        "text.literal": {"color": p["string"]},
        "text.code.span": {"color": p["keyword"]},
        "title": {"color": p["fg"], "font_weight": 600},
        "type": {"color": p["type"]},
        "variable": {"color": p["fg"]},
        "variable.special": {"color": p["number"]},
    }

LIGHT_SYNTAX = dict(fg="#1C1A17", keyword="#9A3EAF", function="#2F64C6", string="#3D8540",
                    number="#B2601E", comment="#A09A92", type="#A06B05", escape="#B03A5B",
                    operator="#57534E", punct="#78736C")
DARK_SYNTAX = dict(fg="#ECEAE6", keyword="#CB8FDC", function="#7DA9F2", string="#9DC889",
                   number="#E3A26B", comment="#77716A", type="#E0BE7B", escape="#E58BA5",
                   operator="#B8B2AB", punct="#9C968F")

def highlight(base, p, dark):
    h = dict(base)
    h.update({
        "editor.foreground": p["fg"],
        "editor.background": "#191817" if dark else "#FFFFFF",
        "editor.active_line.background": "#211F1D" if dark else "#F7F6F4",
        "editor.line_number": "#5F5A55" if dark else "#B8B2AB",
        "editor.active_line_number": p["fg"],
        "editor.invisible": "#5F5A5566" if dark else "#B8B2AB66",
    })
    h["syntax"] = syntax(p)
    return h

src = json.load(open(SRC))
out = {"$schema": src.get("$schema"), "name": "Herse", "author": "Herse", "themes": []}
for theme in src["themes"]:
    dark = theme["mode"] == "dark"
    t = json.loads(json.dumps(theme))
    t["name"] = "Herse Dark" if dark else "Herse Light"
    t["is_default"] = False
    t["colors"] = {k: warm(v) for k, v in t["colors"].items()}
    t["colors"].update(DARK if dark else LIGHT)
    t["highlight"] = highlight({k: warm(v) for k, v in t["highlight"].items() if k != "syntax"},
                               DARK_SYNTAX if dark else LIGHT_SYNTAX, dark)
    out["themes"].append(t)
json.dump(out, open(os.path.join(HERE, "herse-theme.json"), "w"), indent=2)
print("wrote herse-theme.json:", [t["name"] for t in out["themes"]])

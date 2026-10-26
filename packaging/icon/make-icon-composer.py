#!/usr/bin/env python3
"""Generate AppIcon.icon, an Icon Composer document (macOS 26+ Liquid Glass icon).

Only the artwork is ours: the speech bubble and the receipt checks, reusing the
geometry from make-icon.py. The squircle mask, background, glass, specular
highlights and shadows all come from Icon Composer / the system renderer, so
the icon adapts to light, dark, tinted and clear appearances.

Open AppIcon.icon in Icon Composer (Xcode > Open Developer Tool) to tweak it;
build-app.sh compiles it with actool.
"""
import contextlib
import io
import json
import pathlib
import runpy
import shutil
import sys

HERE = pathlib.Path(__file__).parent

# Pull BUBBLE / CHECK_A / CHECK_B out of the flat icon generator (argv makes it
# print instead of overwriting AppIcon.svg).
argv, sys.argv = sys.argv, ["make-icon.py", "1024"]
with contextlib.redirect_stdout(io.StringIO()):
    art = runpy.run_path(str(HERE / "make-icon.py"))
sys.argv = argv

SIZE = 1024
SVG_HEAD = f'<svg xmlns="http://www.w3.org/2000/svg" width="{SIZE}" height="{SIZE}" viewBox="0 0 {SIZE} {SIZE}">'

bubble_svg = f"""{SVG_HEAD}
  <path d="{art["BUBBLE"]}" fill="#2BD164"/>
</svg>
"""

checks_svg = f"""{SVG_HEAD}
  <g fill="none" stroke="#FFFFFF" stroke-width="46" stroke-linecap="round" stroke-linejoin="round">
    <path d="{art["CHECK_B"]}"/>
    <path d="{art["CHECK_A"]}"/>
  </g>
</svg>
"""

# Groups are listed front to back.
doc = {
    "fill": {"automatic-gradient": "extended-srgb:0.96078,0.98431,0.96863,1.00000"},
    "groups": [
        {
            "name": "Checks",
            "layers": [{"name": "checks", "image-name": "checks.svg", "glass": True}],
            "shadow": {"kind": "neutral", "opacity": 0.5},
            "translucency": {"enabled": False, "value": 0.5},
        },
        {
            "name": "Bubble",
            "layers": [
                {
                    "name": "bubble",
                    "image-name": "bubble.svg",
                    "glass": True,
                    "fill": {"automatic-gradient": "extended-srgb:0.16863,0.81961,0.39216,1.00000"},
                }
            ],
            "shadow": {"kind": "layer-color", "opacity": 0.5},
            "translucency": {"enabled": False, "value": 0.5},
        },
    ],
    "supported-platforms": {"squares": ["macOS"]},
}

out = HERE / "AppIcon.icon"
shutil.rmtree(out, ignore_errors=True)
(out / "Assets").mkdir(parents=True)
(out / "Assets" / "bubble.svg").write_text(bubble_svg)
(out / "Assets" / "checks.svg").write_text(checks_svg)
(out / "icon.json").write_text(json.dumps(doc, indent=2) + "\n")
print(out)

#!/usr/bin/env python3
"""Generate the app icon SVG source (AppIcon.svg).

Layout follows the macOS app icon grid: a 1024x1024 canvas with an 824x824
continuous-corner "squircle" body inset 100px, leaving room for the drop
shadow. The squircle is a superellipse (n=5), which is a close match for
Apple's continuous-curvature corners and stays smooth when scaled down.

Run via packaging/icon/render.sh, which also rasterizes and builds the .icns.
"""
import math
import pathlib
import sys

render_size = int(sys.argv[1]) if len(sys.argv) > 1 else 1024
# Optical weight keeps the receipt visible below one physical pixel.
GLYPH_WIDTH = 64 if render_size <= 16 else 54 if render_size <= 32 else 46

SIZE = 1024
INSET = 100
BODY = SIZE - 2 * INSET  # 824


def squircle_path(cx, cy, half, n=5.0, steps=720):
    pts = []
    for i in range(steps):
        t = 2 * math.pi * i / steps
        c, s = math.cos(t), math.sin(t)
        x = cx + half * math.copysign(abs(c) ** (2 / n), c)
        y = cy + half * math.copysign(abs(s) ** (2 / n), s)
        pts.append(f"{x:.2f},{y:.2f}")
    return "M" + " L".join(pts) + " Z"


BODY_PATH = squircle_path(SIZE / 2, SIZE / 2, BODY / 2)

# One continuous silhouette avoids seams where the tail meets the bowl.
BUBBLE = "M 812,482 C 812,338 679,230 512,230 C 345,230 212,338 212,482 C 212,564 251,633 318,679 C 307,719 280,752 246,777 C 320,782 386,759 433,725 C 458,731 485,734 512,734 C 679,734 812,626 812,482 Z"
# A full check and a parallel receipt stroke; no occlusion mask or loose fragments.
CHECK_A = "M 336,505 L 423,589 L 588,420"
CHECK_B = "M 553,589 L 718,420"

svg = f"""<svg xmlns="http://www.w3.org/2000/svg" width="{SIZE}" height="{SIZE}" viewBox="0 0 {SIZE} {SIZE}">
  <defs>
    <linearGradient id="bg" x1="0" y1="{INSET}" x2="0" y2="{SIZE - INSET}" gradientUnits="userSpaceOnUse">
      <stop offset="0" stop-color="#FFFFFF"/>
      <stop offset="0.55" stop-color="#F3F7F4"/>
      <stop offset="1" stop-color="#DDE9E1"/>
    </linearGradient>
    <linearGradient id="bgEdge" x1="0" y1="{INSET}" x2="0" y2="{SIZE - INSET}" gradientUnits="userSpaceOnUse">
      <stop offset="0" stop-color="#FFFFFF" stop-opacity="0.9"/>
      <stop offset="1" stop-color="#000000" stop-opacity="0.10"/>
    </linearGradient>
    <linearGradient id="bubble" x1="0" y1="230" x2="0" y2="790" gradientUnits="userSpaceOnUse">
      <stop offset="0" stop-color="#75EB9B"/>
      <stop offset="0.5" stop-color="#32D568"/>
      <stop offset="1" stop-color="#0CAD53"/>
    </linearGradient>
    <linearGradient id="gloss" x1="0" y1="230" x2="0" y2="490" gradientUnits="userSpaceOnUse">
      <stop offset="0" stop-color="#FFFFFF" stop-opacity="0.55"/>
      <stop offset="1" stop-color="#FFFFFF" stop-opacity="0"/>
    </linearGradient>
    <radialGradient id="pearl" cx="40%" cy="0%" r="85%">
      <stop offset="0" stop-color="#FFFFFF" stop-opacity="0.30"/>
      <stop offset="0.62" stop-color="#FFFFFF" stop-opacity="0.04"/>
      <stop offset="1" stop-color="#FFFFFF" stop-opacity="0"/>
    </radialGradient>
    <clipPath id="bubbleClip"><path d="{BUBBLE}"/></clipPath>
    <clipPath id="bodyClip"><path d="{BODY_PATH}"/></clipPath>
    <filter id="bodyShadow" x="-20%" y="-20%" width="140%" height="140%">
      <feGaussianBlur in="SourceAlpha" stdDeviation="14"/>
      <feOffset dy="12" result="b"/>
      <feComponentTransfer><feFuncA type="linear" slope="0.20"/></feComponentTransfer>
      <feMerge><feMergeNode/><feMergeNode in="SourceGraphic"/></feMerge>
    </filter>
    <filter id="bubbleShadow" x="-20%" y="-20%" width="140%" height="150%">
      <feGaussianBlur in="SourceAlpha" stdDeviation="12"/>
      <feOffset dy="14"/>
      <feComponentTransfer><feFuncA type="linear" slope="0.18"/></feComponentTransfer>
      <feColorMatrix type="matrix" values="0 0 0 0 0.02  0 0 0 0 0.35  0 0 0 0 0.12  0 0 0 1 0"/>
      <feMerge><feMergeNode/><feMergeNode in="SourceGraphic"/></feMerge>
    </filter>
  </defs>

  <!-- Body -->
  <g filter="url(#bodyShadow)">
    <path d="{BODY_PATH}" fill="url(#bg)"/>
  </g>
  <path d="{BODY_PATH}" fill="none" stroke="url(#bgEdge)" stroke-width="3"/>

  <g clip-path="url(#bodyClip)">
    <!-- Bubble -->
    <g filter="url(#bubbleShadow)">
      <path d="{BUBBLE}" fill="url(#bubble)"/>
    </g>
    <path d="{BUBBLE}" fill="url(#pearl)"/>
    <g clip-path="url(#bubbleClip)">
      <path d="{BUBBLE}" fill="none" stroke="#FFFFFF" stroke-opacity="0.22" stroke-width="4" transform="translate(0,2)"/>
    </g>
    <!-- Glyph -->
    <g fill="none" stroke-linecap="round" stroke-linejoin="round">
      <path d="{CHECK_B}" stroke="#FFFFFF" stroke-width="{GLYPH_WIDTH}"/>
      <path d="{CHECK_A}" stroke="#FFFFFF" stroke-width="{GLYPH_WIDTH}"/>
    </g>
  </g>
</svg>
"""

out = pathlib.Path(__file__).with_name("AppIcon.svg")
if len(sys.argv) > 1:
    print(svg)
else:
    out.write_text(svg)
    print(out)

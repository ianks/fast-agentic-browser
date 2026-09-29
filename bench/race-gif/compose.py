#!/usr/bin/env python3
"""Turns a recorded race (record.mjs) into side-by-side frames with labels and
live timers: left, the LLM driving chrome-devtools-mcp; right, fab.
  compose.py <record dir> <frames dir> [fps] [width per side]"""
import os
import re
import sys

from PIL import Image, ImageDraw, ImageFont

rec, frames_dir = sys.argv[1], sys.argv[2]
FPS = float(sys.argv[3]) if len(sys.argv) > 3 else 8
W = int(sys.argv[4]) if len(sys.argv) > 4 else 560
os.makedirs(frames_dir, exist_ok=True)

# Race timing: the arms start together; each event line carries seconds since
# its arm started, so start = arrival time - that offset. The ■ line has walls.
start = None
walls = {}
for line in open(os.path.join(rec, "race.log"), encoding="utf-8"):
    at, _, text = line.rstrip("\n").partition("\t")
    at = int(at) / 1000
    m = re.match(r"\s*(\d+\.\d)s\s+(LLM|JEV)\b", text)
    if m and start is None:
        start = at - float(m.group(1))
    m = re.search(r"LLM (PASS|FAIL) ([\d.]+)s .*JEV (PASS|FAIL) ([\d.]+)s", text)
    if m:
        walls = {"left": (m.group(1), float(m.group(2))), "right": (m.group(3), float(m.group(4)))}
if start is None or not walls:
    sys.exit("no race timing in race.log")

def shots(side):
    d = os.path.join(rec, side)
    return sorted((int(f[:-4]) / 1000, os.path.join(d, f)) for f in os.listdir(d) if f.endswith(".jpg"))

S = {s: shots(s) for s in ("left", "right")}
LABEL = {"left": "LLM + chrome-devtools-mcp", "right": "LLM + fab"}
font = ImageFont.truetype("/System/Library/Fonts/Supplemental/Arial Bold.ttf", 24)
small = ImageFont.truetype("/System/Library/Fonts/Supplemental/Arial.ttf", 20)
BAR = 56
lead, tail = 1.5, 2.5
end = max(w for _, w in walls.values()) + tail
cache = {}

def page(side, t):
    """The last screenshot taken at or before time t (absolute)."""
    best = None
    for ts, p in S[side]:
        if ts <= t:
            best = p
        else:
            break
    best = best or (S[side][0][1] if S[side] else None)
    if best not in cache:
        im = Image.open(best).convert("RGB") if best else Image.new("RGB", (W, W), "white")
        h = round(im.height * W / im.width)
        cache.clear()
        cache[best] = im.resize((W, h), Image.LANCZOS)
    return cache[best]

n = int((end + lead) * FPS)
for i in range(n):
    t = i / FPS - lead  # seconds since the race started
    sides = []
    for side in ("left", "right"):
        verdict, wall = walls[side]
        pg = page(side, start + max(t, 0))
        im = Image.new("RGB", (W, BAR + min(pg.height, round(W * 0.9))), "white")
        im.paste(pg.crop((0, 0, W, min(pg.height, round(W * 0.9)))), (0, BAR))
        d = ImageDraw.Draw(im)
        done = t >= wall
        d.rectangle([0, 0, W, BAR], fill=(22, 101, 52) if done and verdict == "PASS" else (24, 24, 27))
        d.text((16, 15), LABEL[side], font=font, fill="white")
        clock = f"{min(max(t, 0), wall):.1f} s"
        if done:
            clock = ("done in " if verdict == "PASS" else "failed after ") + f"{wall:.1f} s"
        tw = d.textlength(clock, font=small)
        d.text((W - tw - 16, 18), clock, font=small, fill="white")
        sides.append(im)
    h = max(x.height for x in sides)
    frame = Image.new("RGB", (W * 2 + 8, h), (24, 24, 27))
    frame.paste(sides[0], (0, 0))
    frame.paste(sides[1], (W + 8, 0))
    frame.save(os.path.join(frames_dir, f"{i:05d}.png"))
print(f"{n} frames · left {walls['left']} · right {walls['right']}")

#!/usr/bin/env python3
"""The DAIMON wordmark for the boot splash (daimon/src/splash.rs).

  python3 tools/mklogo.py <out_file>

Renders the name in URW Gothic Book (the Avant Garde clone shipped with ghostscript fonts), widely tracked,
supersampled. The splash scales it to the screen and animates it; the mark (rings, core) is drawn in code.

Format (little endian): b"ALF1", u16 w, u16 h, then w*h bytes of 8-bit alpha, row major.
"""
import struct, sys
from PIL import Image, ImageDraw, ImageFont

FONT = "/usr/share/fonts/opentype/urw-base35/URWGothic-Book.otf"
TEXT = "DAIMON"
CAP = 160          # cap height of the master, px; the splash only ever scales it down
TRACK = 0.42       # letter spacing, in cap heights
SS = 4

font = ImageFont.truetype(FONT, CAP * SS * 10 // 7)
cap = font.getbbox("H")[3] - font.getbbox("H")[1]
font = ImageFont.truetype(FONT, round(font.size * CAP * SS / cap))
gap = TRACK * CAP * SS
widths = [font.getlength(c) for c in TEXT]
W = int(sum(widths) + gap * (len(TEXT) - 1)) + 8 * SS
top = font.getbbox("H")[1]
img = Image.new("L", (W, CAP * SS + 8 * SS), 0)
d = ImageDraw.Draw(img)
x = 4 * SS
for c, w in zip(TEXT, widths):
    d.text((x, 4 * SS - top), c, font=font, fill=255)
    x += w + gap
img = img.crop(img.getbbox())
img = img.resize((img.width // SS, img.height // SS), Image.LANCZOS)
with open(sys.argv[1], "wb") as f:
    f.write(b"ALF1" + struct.pack("<HH", img.width, img.height) + img.tobytes())
if len(sys.argv) > 2:  # preview png
    img.save(sys.argv[2])

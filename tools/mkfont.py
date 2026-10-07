#!/usr/bin/env python3
"""Bitmap fonts for the Daimon framebuffer console (aios/src/fb.rs).

  python3 tools/mkfont.py <out_dir>

Renders DejaVu Sans Mono (regular + bold, anti-aliased) at a few sizes into cell-sized 4-bit glyphs.
Box drawing, block elements, braille, separators and meter bands (U+E0C0..E0C3) are drawn here instead of taken from the font,
so lines meet exactly at cell edges. Icons are small vector drawings (Lucide-like strokes) rendered with
supersampling into Private Use Area pairs: icon i is U+E100+2i (left half) and U+E101+2i (right half).

Format (little endian): b"AFN2", u16 cell_w, u16 cell_h, u32 n, u32 codepoints[n] (sorted),
then n regular glyphs and n bold glyphs, each ceil(w*h/2) bytes of 4-bit alpha (high nibble first).
"""
import math, struct, sys, unicodedata
from pathlib import Path
from PIL import Image, ImageDraw, ImageFont

DEJAVU = "/usr/share/fonts/truetype/dejavu/"
SIZES = [13, 16, 20, 25]  # font px; cells ~8x15, 10x19, 12x23, 15x29
RANGES = [(0x20, 0x7E), (0xA0, 0x24F), (0x2B0, 0x2FF), (0x370, 0x3FF), (0x400, 0x4FF), (0x1E00, 0x1EFF),
          (0x2000, 0x206F), (0x20A0, 0x20BF), (0x2100, 0x214F), (0x2190, 0x21FF), (0x2200, 0x22FF),
          (0x2300, 0x23FF), (0x2460, 0x24FF), (0x25A0, 0x25FF), (0x2600, 0x26FF), (0x2700, 0x27BF),
          (0xFFFD, 0xFFFD)]
SS = 4  # supersampling for curves and icons

ICONS = ["cpu", "memory", "network", "spark", "shield", "keyboard", "chat", "gear", "cube", "clock",
         "check", "cross", "alert", "terminal", "bolt", "user", "disk", "brain"]


# ------------------------------------------------------------------ procedural glyphs

def box_segments(cp):
    """BOX DRAWINGS name -> ({dir: weight}, extra) with weight 1 light, 2 heavy, 3 double"""
    words = unicodedata.name(chr(cp)).split()[2:]
    if "ARC" in words or "DIAGONAL" in words:
        return None, " ".join(words)
    dash = next((w for w in words if w in ("DOUBLE", "TRIPLE", "QUADRUPLE")), None) if "DASH" in words else None
    words = [w for w in words if w not in ("DASH", "TRIPLE", "QUADRUPLE") and not (dash == "DOUBLE" and w == "DOUBLE")]
    W = {"LIGHT": 1, "SINGLE": 1, "HEAVY": 2, "DOUBLE": 3}
    D = {"UP": ["u"], "DOWN": ["d"], "LEFT": ["l"], "RIGHT": ["r"], "VERTICAL": ["u", "d"], "HORIZONTAL": ["l", "r"]}
    seg, pending, cur = {}, [], 1
    for w in words:
        if w in W:
            if pending:
                seg.update({d: W[w] for d in pending})
                pending = []
            cur = W[w]
        elif w in D:
            pending += D[w]
    seg.update({d: cur for d in pending})
    return seg, dash


def draw_box(cp, w, h):
    img = Image.new("L", (w, h), 0)
    seg, extra = box_segments(cp)
    t = max(1, round(w / 8))
    cx, cy = (w - t) // 2, (h - t) // 2
    if seg is None:
        big = Image.new("L", (w * SS, h * SS), 0)
        d = ImageDraw.Draw(big)
        lw = t * SS
        X, Y = (cx + t / 2) * SS, (cy + t / 2) * SS
        if "ARC" in extra:
            # quarter circle from the cell centre to the two edges named in the char
            r = min(w, h) / 2 * SS
            down, right = "DOWN" in extra, "RIGHT" in extra
            ox, oy = (X + r if right else X - r), (Y + r if down else Y - r)
            start = {(True, True): 180, (True, False): 270, (False, False): 0, (False, True): 90}[(down, right)]
            d.arc([ox - r, oy - r, ox + r, oy + r], start, start + 90, fill=255, width=lw)
            # straight runs from the arc to the cell edges
            def run(x0, y0, x1, y1):
                x0, x1 = sorted((x0, x1))
                y0, y1 = sorted((y0, y1))
                if x1 > x0 and y1 > y0:
                    d.rectangle([x0, y0, x1, y1], fill=255)
            run(X - lw / 2, oy, X + lw / 2, h * SS if down else 0)
            run(ox, Y - lw / 2, w * SS if right else 0, Y + lw / 2)
        else:
            if "UPPER RIGHT TO LOWER LEFT" in extra or "CROSS" in extra:
                d.line([w * SS, 0, 0, h * SS], fill=255, width=lw)
            if "UPPER LEFT TO LOWER RIGHT" in extra or "CROSS" in extra:
                d.line([0, 0, w * SS, h * SS], fill=255, width=lw)
        return big.resize((w, h), Image.LANCZOS)
    d = ImageDraw.Draw(img)
    for dr, wt in seg.items():
        th = t * 2 if wt == 2 else t
        ox, oy = (w - th) // 2, (h - th) // 2
        offsets = [(-t, ), (t, )] if wt == 3 else [(0, )]
        for (o,) in offsets:
            if dr in "ud":
                x0 = ox + o
                y0, y1 = (0, oy + th - 1 + (t if wt == 3 else 0)) if dr == "u" else (oy - (t if wt == 3 else 0), h - 1)
                d.rectangle([x0, y0, x0 + th - 1, y1], fill=255)
            else:
                y0 = oy + o
                x0, x1 = (0, ox + th - 1 + (t if wt == 3 else 0)) if dr == "l" else (ox - (t if wt == 3 else 0), w - 1)
                d.rectangle([x0, y0, x1, y0 + th - 1], fill=255)
    if extra:  # dashed: punch gaps
        n = {"DOUBLE": 2, "TRIPLE": 3, "QUADRUPLE": 4}[extra]
        vertical = "u" in seg
        L = h if vertical else w
        for i in range(n):
            g0 = int((i + 1) * L / n) - max(1, L // (3 * n))
            g1 = int((i + 1) * L / n) - 1
            if vertical:
                d.rectangle([0, g0, w, g1], fill=0)
            else:
                d.rectangle([g0, 0, g1, h], fill=0)
    return img


def draw_block(cp, w, h):
    img = Image.new("L", (w, h), 0)
    d = ImageDraw.Draw(img)
    name = unicodedata.name(chr(cp))
    eighths = {"ONE": 1, "TWO": 2, "THREE": 3, "FOUR": 4, "FIVE": 5, "SIX": 6, "SEVEN": 7, "HALF": 4}
    if "SHADE" in name:
        level = {"LIGHT": 64, "MEDIUM": 128, "DARK": 192}[name.split()[0]]
        img.paste(level, [0, 0, w, h])
    elif name == "FULL BLOCK":
        img.paste(255, [0, 0, w, h])
    elif name.startswith("QUADRANT"):
        q = name[len("QUADRANT "):].replace(" AND", "").split()
        parts = {"UPPER LEFT": (0, 0), "UPPER RIGHT": (1, 0), "LOWER LEFT": (0, 1), "LOWER RIGHT": (1, 1)}
        for i in range(0, len(q), 2):
            x, y = parts[q[i] + " " + q[i + 1]]
            d.rectangle([x * w // 2, y * h // 2, (x + 1) * w // 2 - 1, (y + 1) * h // 2 - 1], fill=255)
    else:
        words = name.split()
        n = eighths[words[1]]
        n = 4 if words[1] == "HALF" else n
        side = words[0]
        if side == "UPPER":
            d.rectangle([0, 0, w - 1, round(h * n / 8) - 1], fill=255)
        elif side == "LOWER":
            d.rectangle([0, h - round(h * n / 8), w - 1, h - 1], fill=255)
        elif side == "LEFT":
            d.rectangle([0, 0, round(w * n / 8) - 1, h - 1], fill=255)
        elif side == "RIGHT":
            d.rectangle([w - round(w * n / 8), 0, w - 1, h - 1], fill=255)
    return img


def draw_braille(cp, w, h):
    big = Image.new("L", (w * SS, h * SS), 0)
    d = ImageDraw.Draw(big)
    bits = cp - 0x2800
    pos = [(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2), (0, 3), (1, 3)]
    r = min(w / 4, h / 8) * 0.8 * SS
    for i, (x, y) in enumerate(pos):
        if bits >> i & 1:
            cx, cy = (x + 0.5) * w / 2 * SS, (y + 0.5) * h / 4 * SS
            d.ellipse([cx - r, cy - r, cx + r, cy + r], fill=255)
    return big.resize((w, h), Image.LANCZOS)


def draw_meter(cp, w, h):
    """U+E0C0 meter band, U+E0C2 / U+E0C3 the band with a rounded left / right end"""
    big = Image.new("L", (w * SS, h * SS), 0)
    d = ImageDraw.Draw(big)
    W, H = w * SS, h * SS
    bh = max(3, round(h * 0.28)) * SS
    y0 = (H - bh) // 2
    x0, x1 = (bh // 2 if cp == 0xE0C2 else 0), (W - bh // 2 if cp == 0xE0C3 else W)
    d.rectangle([x0, y0, x1, y0 + bh - 1], fill=255)
    if cp == 0xE0C2:
        d.ellipse([0, y0, bh, y0 + bh - 1], fill=255)
    if cp == 0xE0C3:
        d.ellipse([W - bh, y0, W, y0 + bh - 1], fill=255)
    return big.resize((w, h), Image.LANCZOS)


def draw_separator(cp, w, h):
    """U+E0B0..E0B7 (powerline style): solid/half-circle caps for pills and tabs"""
    big = Image.new("L", (w * SS, h * SS), 0)
    d = ImageDraw.Draw(big)
    W, H = w * SS, h * SS
    k = cp - 0xE0B0
    if k == 0:
        d.polygon([(0, 0), (W, H / 2), (0, H)], fill=255)
    elif k == 2:
        d.polygon([(W, 0), (0, H / 2), (W, H)], fill=255)
    elif k == 4:
        d.ellipse([-W, 0, W, H], fill=255)
    elif k == 6:
        d.ellipse([0, 0, 2 * W, H], fill=255)
    return big.resize((w, h), Image.LANCZOS)


# ------------------------------------------------------------------ icons (2 cells wide)

def draw_icon(name, w, h):
    W, H = 2 * w * SS, h * SS
    big = Image.new("L", (W, H), 0)
    d = ImageDraw.Draw(big)
    s = min(W, H) * 0.86  # icon box
    ox, oy = (W - s) / 2, (H - s) / 2
    lw = max(SS, round(s * 0.09))

    def P(x, y):  # icon coordinates in 0..24 like a Lucide viewBox
        return (ox + x / 24 * s, oy + y / 24 * s)

    def line(*pts):
        d.line([P(*p) for p in pts], fill=255, width=lw, joint="curve")
        for p in pts:  # round caps
            x, y = P(*p)
            d.ellipse([x - lw / 2, y - lw / 2, x + lw / 2, y + lw / 2], fill=255)

    def rect(x0, y0, x1, y1, r=2, fill=False):
        d.rounded_rectangle([P(x0, y0), P(x1, y1)], radius=r / 24 * s, outline=255, width=lw, fill=255 if fill else None)

    def circle(cx, cy, r, fill=False):
        (x0, y0), (x1, y1) = P(cx - r, cy - r), P(cx + r, cy + r)
        d.ellipse([x0, y0, x1, y1], outline=255, width=lw, fill=255 if fill else None)

    def arc(cx, cy, r, a0, a1):
        (x0, y0), (x1, y1) = P(cx - r, cy - r), P(cx + r, cy + r)
        d.arc([x0, y0, x1, y1], a0, a1, fill=255, width=lw)

    if name == "cpu":
        rect(5, 5, 19, 19, 2)
        rect(9, 9, 15, 15, 1, fill=True)
        for v in (9, 15):
            line((v, 1.5), (v, 5)); line((v, 19), (v, 22.5)); line((1.5, v), (5, v)); line((19, v), (22.5, v))
    elif name == "memory":
        rect(2, 6, 22, 16, 1.5)
        for x in (6, 10, 14, 18):
            line((x, 16), (x, 19))
        rect(5.5, 9, 9, 13, 0.5, fill=True); rect(11, 9, 14.5, 13, 0.5, fill=True); rect(16.5, 9, 18.5, 13, 0.5, fill=True)
    elif name == "network":
        circle(12, 12, 10)
        line((2, 12), (22, 12))
        d.ellipse([*P(7.5, 2), *P(16.5, 22)], outline=255, width=lw)
    elif name == "spark":
        pts = []
        for i in range(8):
            a = math.pi / 4 * i - math.pi / 2
            r = 10.5 if i % 2 == 0 else 3.2
            pts.append(P(12 + r * math.cos(a), 12 + r * math.sin(a)))
        d.polygon(pts, fill=255)
    elif name == "shield":
        d.line([P(12, 2), P(20, 5), P(20, 12), P(12, 22), P(4, 12), P(4, 5), P(12, 2)], fill=255, width=lw, joint="curve")
        line((8.5, 12), (11, 14.5), (15.5, 9.5))
    elif name == "keyboard":
        rect(2, 6, 22, 18, 2)
        for y in (10, 14):
            for x in (6, 10, 14, 18):
                circle(x, y, 0.6, fill=True)
        line((8, 14.5), (16, 14.5))
    elif name == "chat":
        d.line([P(4, 4), P(20, 4), P(20, 16), P(10, 16), P(5, 20), P(5, 16), P(4, 16), P(4, 4)], fill=255, width=lw, joint="curve")
    elif name == "gear":
        circle(12, 12, 3.5)
        for i in range(8):
            a = math.pi / 4 * i
            line((12 + 6.5 * math.cos(a), 12 + 6.5 * math.sin(a)), (12 + 10 * math.cos(a), 12 + 10 * math.sin(a)))
        circle(12, 12, 7)
    elif name == "cube":
        d.line([P(12, 2), P(21, 7), P(21, 17), P(12, 22), P(3, 17), P(3, 7), P(12, 2)], fill=255, width=lw, joint="curve")
        line((3, 7), (12, 12), (21, 7)); line((12, 12), (12, 22))
    elif name == "clock":
        circle(12, 12, 10)
        line((12, 6), (12, 12), (16, 14))
    elif name == "check":
        circle(12, 12, 10)
        line((7.5, 12), (10.5, 15), (16.5, 9))
    elif name == "cross":
        circle(12, 12, 10)
        line((8.5, 8.5), (15.5, 15.5)); line((15.5, 8.5), (8.5, 15.5))
    elif name == "alert":
        d.line([P(12, 3), P(22, 20), P(2, 20), P(12, 3)], fill=255, width=lw, joint="curve")
        line((12, 9), (12, 14)); circle(12, 17, 0.7, fill=True)
    elif name == "terminal":
        rect(2, 4, 22, 20, 2)
        line((6, 9), (9.5, 12), (6, 15)); line((12, 15), (17, 15))
    elif name == "bolt":
        d.polygon([P(13.5, 2), P(4, 14), P(11, 14), P(10, 22), P(20, 10), P(13, 10)], fill=255)
    elif name == "user":
        circle(12, 8, 4)
        arc(12, 22, 8, 180, 360)
    elif name == "disk":
        d.ellipse([*P(4, 3), *P(20, 8)], outline=255, width=lw)
        line((4, 5.5), (4, 18.5)); line((20, 5.5), (20, 18.5))
        arc(12, 18.5, 8, 0, 180)
        (x0, y0), (x1, y1) = P(4, 9.5), P(20, 14.5)
        d.arc([x0, y0, x1, y1], 0, 180, fill=255, width=lw)
    elif name == "brain":
        arc(8.5, 8, 4.5, 150, 330); arc(15.5, 8, 4.5, 210, 30)
        arc(7, 14.5, 4.5, 90, 270); arc(17, 14.5, 4.5, 270, 90)
        line((8.5, 19), (15.5, 19)); line((12, 4), (12, 19))
    img = big.resize((2 * w, h), Image.LANCZOS)
    return img.crop((0, 0, w, h)), img.crop((w, 0, 2 * w, h))


# ------------------------------------------------------------------ text glyphs

def build(px):
    reg = ImageFont.truetype(DEJAVU + "DejaVuSansMono.ttf", px)
    bold = ImageFont.truetype(DEJAVU + "DejaVuSansMono-Bold.ttf", px)
    asc, desc = reg.getmetrics()
    w, h = round(reg.getlength("M")), asc + desc
    def text_glyph(font, ch):
        img = Image.new("L", (w, h), 0)
        ImageDraw.Draw(img).text((0, asc), ch, font=font, fill=255, anchor="ls")
        return img

    notdef = text_glyph(reg, chr(0x10FFFD)).tobytes()

    glyphs = {}
    for a, b in RANGES:
        for cp in range(a, b + 1):
            ch = chr(cp)
            if unicodedata.category(ch) in ("Cc", "Cs", "Co", "Cn"):
                continue
            g = text_glyph(reg, ch)
            if g.tobytes() == notdef:
                continue
            glyphs[cp] = (g, text_glyph(bold, ch))
    for cp in range(0x2500, 0x2580):
        g = draw_box(cp, w, h)
        glyphs[cp] = (g, g)
    for cp in range(0x2580, 0x25A0):
        g = draw_block(cp, w, h)
        glyphs[cp] = (g, g)
    for cp in range(0x2800, 0x2900):
        g = draw_braille(cp, w, h)
        glyphs[cp] = (g, g)
    for cp in (0xE0B0, 0xE0B2, 0xE0B4, 0xE0B6):
        g = draw_separator(cp, w, h)
        glyphs[cp] = (g, g)
    for cp in (0xE0C0, 0xE0C2, 0xE0C3):
        g = draw_meter(cp, w, h)
        glyphs[cp] = (g, g)
    for i, name in enumerate(ICONS):
        left, right = draw_icon(name, w, h)
        glyphs[0xE100 + 2 * i] = (left, left)
        glyphs[0xE101 + 2 * i] = (right, right)
    return w, h, glyphs


def pack(img):
    px = img.tobytes()
    if len(px) % 2:
        px += b"\0"
    return bytes((px[i] >> 4) << 4 | px[i + 1] >> 4 for i in range(0, len(px), 2))


def main():
    out = Path(sys.argv[1])
    out.mkdir(parents=True, exist_ok=True)
    for px in SIZES:
        w, h, glyphs = build(px)
        cps = sorted(glyphs)
        blob = b"AFN2" + struct.pack("<HHI", w, h, len(cps)) + struct.pack(f"<{len(cps)}I", *cps)
        blob += b"".join(pack(glyphs[c][0]) for c in cps) + b"".join(pack(glyphs[c][1]) for c in cps)
        (out / f"font-{h}.fnt").write_bytes(blob)
        print(f"font-{h}.fnt: {w}x{h} cells, {len(cps)} glyphs, {len(blob) >> 10} KB")


if __name__ == "__main__":
    main()

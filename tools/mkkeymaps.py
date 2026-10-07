#!/usr/bin/env python3
"""Console keyboard layouts for Daimon, built from the host's XKB data with ckbcomp (console-setup).

  python3 tools/mkkeymaps.py <out_dir>

Writes <out_dir>/<name>.akm (loaded by daimon with KDSKBENT/KDSKBDIACRUC, see daimon/src/keyboard.rs) and
<out_dir>/index.txt ("name<TAB>description" per line). Format of .akm, little endian:
  b"AKM1", u16 n, n x (u8 table, u8 keycode, u16 value), u16 d, d x (u32 diacr, u32 base, u32 result)
`value` is what KDSKBENT takes: K(type, val), or U+xxxx ^ 0xF000 for Unicode symbols.
"""
import re, struct, subprocess, sys, unicodedata
from pathlib import Path

# name -> (xkb layout, xkb variant). Names are what the owner/agent types: /set keymap it
LAYOUTS = {
    "us": ("us", ""), "us-intl": ("us", "intl"), "us-dvorak": ("us", "dvorak"), "us-colemak": ("us", "colemak"),
    "us-mac": ("us", "mac"), "gb": ("gb", ""), "gb-extd": ("gb", "extd"), "ie": ("ie", ""),
    "it": ("it", ""), "it-mac": ("it", "mac"), "it-nodeadkeys": ("it", "nodeadkeys"),
    "de": ("de", ""), "de-nodeadkeys": ("de", "nodeadkeys"), "de-mac": ("de", "mac"), "at": ("at", ""),
    "ch": ("ch", ""), "ch-fr": ("ch", "fr"),
    "fr": ("fr", ""), "fr-nodeadkeys": ("fr", "nodeadkeys"), "fr-bepo": ("fr", "bepo"), "fr-mac": ("fr", "mac"),
    "be": ("be", ""), "nl": ("nl", ""), "ca": ("ca", ""), "ca-multix": ("ca", "multix"),
    "es": ("es", ""), "es-cat": ("es", "cat"), "latam": ("latam", ""), "pt": ("pt", ""), "br": ("br", ""),
    "dk": ("dk", ""), "no": ("no", ""), "se": ("se", ""), "fi": ("fi", ""), "is": ("is", ""),
    "pl": ("pl", ""), "cz": ("cz", ""), "cz-qwerty": ("cz", "qwerty"), "sk": ("sk", ""), "hu": ("hu", ""),
    "ro": ("ro", ""), "ro-std": ("ro", "std"), "si": ("si", ""), "hr": ("hr", ""), "rs-latin": ("rs", "latin"),
    "ee": ("ee", ""), "lv": ("lv", ""), "lt": ("lt", ""), "tr": ("tr", ""), "tr-f": ("tr", "f"),
    "gr": ("gr", ""), "ru": ("ru", ""), "ua": ("ua", ""), "by": ("by", ""), "bg": ("bg", ""), "mk": ("mk", ""),
    "rs": ("rs", ""), "kz": ("kz", ""), "jp": ("jp", ""),
}


def K(t, v):
    return (t << 8) | v


LATIN, FN, SPEC, PAD, DEAD, CONS, CUR, SHIFT, META, ASCII, LOCK, LETTER = range(12)
ASCII_NAMES = {
    "space": 32, "exclam": 33, "quotedbl": 34, "numbersign": 35, "dollar": 36, "percent": 37, "ampersand": 38,
    "apostrophe": 39, "parenleft": 40, "parenright": 41, "asterisk": 42, "plus": 43, "comma": 44, "minus": 45,
    "period": 46, "slash": 47, "zero": 48, "one": 49, "two": 50, "three": 51, "four": 52, "five": 53, "six": 54,
    "seven": 55, "eight": 56, "nine": 57, "colon": 58, "semicolon": 59, "less": 60, "equal": 61, "greater": 62,
    "question": 63, "at": 64, "bracketleft": 91, "backslash": 92, "bracketright": 93, "asciicircum": 94,
    "underscore": 95, "grave": 96, "braceleft": 123, "bar": 124, "braceright": 125, "asciitilde": 126,
    "nul": 0, "Tab": 9, "Linefeed": 10, "Escape": 27, "Delete": 127, "BackSpace": 8,
}
ASCII_NAMES.update({c: ord(c) for c in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ"})
CONTROL = {**{f"Control_{c}": ord(c) - 96 for c in "abcdefghijklmnopqrstuvwxyz"},
           "Control_bracketleft": 27, "Control_backslash": 28, "Control_bracketright": 29,
           "Control_asciicircum": 30, "Control_underscore": 31, "nul": 0}
FN_NAMES = {**{f"F{i}": i - 1 for i in range(1, 21)}, **{f"F{i}": 30 + i - 21 for i in range(21, 247)},
            "Find": 20, "Home": 20, "Insert": 21, "Remove": 22, "Select": 23, "End": 23, "Prior": 24, "PageUp": 24,
            "Next": 25, "PageDown": 25, "Macro": 26, "Help": 27, "Do": 28, "Pause": 29}
SPEC_NAMES = {"VoidSymbol": 0, "Return": 1, "Show_Registers": 2, "Show_Memory": 3, "Show_State": 4, "Break": 5,
              "Last_Console": 6, "Caps_Lock": 7, "Num_Lock": 8, "Scroll_Lock": 9, "Scroll_Forward": 10,
              "Scroll_Backward": 11, "Boot": 12, "Caps_On": 13, "Compose": 14, "SAK": 15, "Decr_Console": 16,
              "Incr_Console": 17, "KeyboardSignal": 18, "Bare_Num_Lock": 19}
PAD_NAMES = {**{f"KP_{i}": i for i in range(10)}, "KP_Add": 10, "KP_Subtract": 11, "KP_Multiply": 12,
             "KP_Divide": 13, "KP_Enter": 14, "KP_Comma": 15, "KP_Period": 16, "KP_MinPlus": 17}
SHIFT_NAMES = {"Shift": 0, "AltGr": 1, "Control": 2, "Alt": 3, "ShiftL": 4, "ShiftR": 5, "CtrlL": 6, "CtrlR": 7,
               "CapsShift": 8}
CUR_NAMES = {"Down": 0, "Left": 1, "Right": 2, "Up": 3}
ASCII_PAD = {**{f"Ascii_{i}": i for i in range(10)}, **{f"Hex_{c}": 10 + i for i, c in enumerate("0123456789ABCDEF")}}
# kernel KT_DEAD index -> (ckbcomp names, the char the kernel reports as `diacr`, combining mark for the table)
DEADS = [
    (("dead_grave",), "`", "\u0300"), (("dead_acute",), "'", "\u0301"), (("dead_circumflex",), "^", "\u0302"),
    (("dead_tilde",), "~", "\u0303"), (("dead_diaeresis",), '"', "\u0308"), (("dead_cedilla",), ",", "\u0327"),
    (("dead_macron",), "_", "\u0304"), (("dead_breve", "dead_kbreve"), "U", "\u0306"),
    (("dead_abovedot",), ".", "\u0307"), (("dead_abovering",), "*", "\u030a"),
    (("dead_doubleacute", "dead_kdoubleacute"), "=", "\u030b"), (("dead_caron", "dead_kcaron"), "c", "\u030c"),
    (("dead_ogonek", "dead_kogonek"), "k", "\u0328"), (("dead_iota",), "i", "\u0345"),
    ((), "#", None), ((), "o", None), (("dead_belowdot",), "!", "\u0323"), (("dead_hook",), "?", "\u0309"),
    (("dead_horn",), "+", "\u031b"), (("dead_stroke",), "-", None), (("dead_abovecomma",), ")", "\u0313"),
    ((), "(", None), (("dead_doublegrave",), ":", "\u030f"), (("dead_invertedbreve",), "n", "\u0311"),
    (("dead_belowcomma",), ";", "\u0326"),
    (("dead_currency",), "$", None), (("dead_greek",), "@", None),
]
DEAD_NAMES = {n: i for i, (names, _, _) in enumerate(DEADS) for n in names}


def value(sym):
    """keysym name from ckbcomp -> KDSKBENT value, or None if unknown"""
    letter = sym.startswith("+")
    sym = sym.lstrip("+")
    if m := re.fullmatch(r"U\+([0-9a-fA-F]{4})", sym):
        c = int(m[1], 16)
        if c < 0x100:
            return K(LETTER if letter else LATIN, c)
        return c ^ 0xF000
    if sym in ASCII_NAMES:
        c = ASCII_NAMES[sym]
        return K(LETTER if letter or chr(c).isalpha() else LATIN, c)
    if sym in CONTROL:
        return K(LATIN, CONTROL[sym])
    if sym.startswith("Meta_"):
        rest = sym[5:]
        if rest.startswith("Control_") and "Control_" + rest[8:] in CONTROL:
            return K(META, CONTROL["Control_" + rest[8:]])
        if rest in ASCII_NAMES:
            return K(META, ASCII_NAMES[rest])
    if m := re.fullmatch(r"Console_(\d+)", sym):
        return K(CONS, int(m[1]) - 1)
    for table, t in ((FN_NAMES, FN), (SPEC_NAMES, SPEC), (PAD_NAMES, PAD), (SHIFT_NAMES, SHIFT),
                     (CUR_NAMES, CUR), (ASCII_PAD, ASCII), (DEAD_NAMES, DEAD)):
        if sym in table:
            return K(t, table[sym])
    return None


def diacritics(dead_used):
    """accent table: (diacr char, base letter) -> composed letter, for the dead keys this layout has (max 256)"""
    out = []
    for i in sorted(dead_used):
        _, diacr, mark = DEADS[i]
        if not mark:
            continue
        for base in "aeiouyAEIOUYcCnNsSzZgGrRlLtTdDkKwWhHjJbBmMpPvVxXfFqQ":
            comp = unicodedata.normalize("NFC", base + mark)
            if len(comp) == 1:
                out.append((ord(diacr), ord(base), ord(comp)))
    return out[:256]  # ponytail: MAX_DIACR; layouts with many dead keys lose their rarest combinations


def build(name, layout, variant):
    args = ["ckbcomp", "-compact", layout] + ([variant] if variant else [])
    src = subprocess.run(args, capture_output=True, text=True, check=True).stdout
    tables = []
    entries, unknown, deads = [], set(), set()
    for line in src.splitlines():
        if line.startswith("keymaps "):
            for part in line.split()[1].split(","):
                a, _, b = part.partition("-")
                tables += range(int(a), int(b or a) + 1)
        m = re.match(r"keycode\s+(\d+)\s*=\s*(.*)", line)
        if not m:
            continue
        code = int(m[1])
        for t, sym in zip(tables, m[2].split()):
            v = value(sym)
            if v is None:
                unknown.add(sym)
                v = K(SPEC, 0)
            if v >> 8 == DEAD:
                deads.add(v & 0xFF)
            entries.append((t, code, v))
    if not entries:
        raise SystemExit(f"{name}: ckbcomp produced no keys")
    d = diacritics(deads)
    blob = b"AKM1" + struct.pack("<H", len(entries)) + b"".join(struct.pack("<BBH", *e) for e in entries)
    blob += struct.pack("<H", len(d)) + b"".join(struct.pack("<III", *e) for e in d)
    return blob, unknown


def descriptions():
    lst = Path("/usr/share/X11/xkb/rules/base.lst").read_text().split("\n! ")
    desc = {}
    for sec in lst:
        head, *lines = sec.splitlines()
        for l in lines:
            p = l.split(None, 1)
            if len(p) < 2:
                continue
            if head.endswith("layout"):
                desc[(p[0], "")] = p[1].strip()
            elif head.endswith("variant"):
                lay, _, d = p[1].partition(":")
                desc[(lay.strip(), p[0])] = d.strip()
    return desc


def main():
    out = Path(sys.argv[1])
    out.mkdir(parents=True, exist_ok=True)
    desc = descriptions()
    index = []
    for name, (layout, variant) in LAYOUTS.items():
        blob, unknown = build(name, layout, variant)
        (out / f"{name}.akm").write_bytes(blob)
        index.append(f"{name}\t{desc.get((layout, variant), name)}")
        if unknown:
            print(f"{name}: unknown keysyms left empty: {' '.join(sorted(unknown))}", file=sys.stderr)
    (out / "index.txt").write_text("\n".join(index) + "\n")
    print(f"{len(index)} keymaps in {out}")


if __name__ == "__main__":
    main()

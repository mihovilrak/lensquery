#!/usr/bin/env python3
"""Regenerate the OCR test fixtures in tests/fixtures/.

The PNGs are committed; this script exists so they are reproducible and so a
new fixture is a diff to source rather than a binary someone dropped in. It is
the only Python left in the project and is not part of any build — run it by
hand (`just fixtures`) when you add or change a fixture.

The fixtures are synthetic on purpose. Real screenshots are the interesting
input, but they carry whatever was on the author's screen, and a public
repository is the wrong place for that. Everything here is rendered from the
strings in FIXTURES below, so what you see in this file is exactly what is in
the images.

Each fixture declares the substrings OCR must recover from it. Those live in
`tests/fixtures/README.md` and in `examples/recall.rs`, which is what actually
asserts them. Substrings, not whole-output equality: a rotated page or a
low-contrast scan will drop a comma or split a word depending on the Tesseract
build, and a gate that fails on that is a gate people learn to ignore.

Requires Pillow and a few common TrueType fonts. Deterministic: no timestamps,
seeded noise, fixed font sizes.
"""

import os
import pathlib
import random
import sys

from PIL import Image, ImageDraw, ImageFilter, ImageFont

OUT_DIR = pathlib.Path(__file__).resolve().parent.parent / "tests" / "fixtures"

# Font files by role. Each entry is tried in order; the first that exists wins.
# Different boxes have different fonts, and the rendered glyphs differ slightly
# between them, which is fine — the fixtures are committed, so only the person
# regenerating them ever runs this, and the recall gate asserts substrings.
FONT_CANDIDATES = {
    "sans": [
        "C:/Windows/Fonts/arial.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/System/Library/Fonts/Supplemental/Arial.ttf",
    ],
    "sans_bold": [
        "C:/Windows/Fonts/arialbd.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
        "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
    ],
    "serif": [
        "C:/Windows/Fonts/times.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf",
        "/System/Library/Fonts/Supplemental/Times New Roman.ttf",
    ],
    "mono": [
        "C:/Windows/Fonts/consola.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "/System/Library/Fonts/Menlo.ttc",
    ],
}


def font(role: str, size: int) -> ImageFont.FreeTypeFont:
    """Load the first available font for `role` at `size`, or die loudly.

    Dying is deliberate. A silent fallback to Pillow's bitmap default renders
    text Tesseract reads badly, and the failure would show up as a mysterious
    recall-gate regression rather than "install a font."
    """
    for path in FONT_CANDIDATES[role]:
        if os.path.exists(path):
            return ImageFont.truetype(path, size)
    sys.exit(f"no font found for role {role!r}; tried {FONT_CANDIDATES[role]}")


def hello_world(path: pathlib.Path) -> None:
    """The sanity fixture: one line, high contrast, nothing clever."""
    img = Image.new("RGB", (1200, 300), "white")
    ImageDraw.Draw(img).text((40, 100), "Hello World", fill="black", font=font("sans", 64))
    img.save(path)


def croatian_text(path: pathlib.Path) -> None:
    """Diacritics end-to-end: rendering, OCR, FTS5 tokenizing, terminal output.

    Real words, not a `čćžšđ` glyph cluster. Tesseract scores against a language
    model, so a nonsense run of accented letters comes back below the default
    confidence floor and gets dropped — the fixture would then fail for a reason
    that has nothing to do with diacritic handling. Each word below carries one
    of the five, and `đ` is the one that historically broke: it is the only one
    that is not a Latin letter plus a combining mark in most fonts.
    """
    img = Image.new("RGB", (1200, 300), "white")
    d = ImageDraw.Draw(img)
    body = font("sans", 56)
    d.text((40, 60), "Dobar dan, evo izvještaja.", fill="black", font=body)
    d.text((40, 150), "Čaša, žuto, ćevapi, đak, šuma.", fill="black", font=body)
    img.save(path)


def invoice(path: pathlib.Path) -> None:
    """A document with structure: headings, a right-aligned numeric column, a
    total. Exercises the case where line order matters and where digits and
    letters share a line."""
    img = Image.new("RGB", (1000, 1300), "white")
    d = ImageDraw.Draw(img)
    head, bold, body = font("serif", 52), font("sans_bold", 30), font("sans", 30)

    d.text((60, 60), "INVOICE", fill="black", font=head)
    d.text((60, 140), "Invoice number: 2026-0417", fill="black", font=body)
    d.text((60, 185), "Date of issue: 17 August 2026", fill="black", font=body)
    d.text((60, 230), "Billed to: Northwind Analytics", fill="black", font=body)

    d.line((60, 300, 940, 300), fill="black", width=2)
    d.text((60, 320), "Description", fill="black", font=bold)
    d.text((700, 320), "Amount", fill="black", font=bold)
    d.line((60, 365, 940, 365), fill="black", width=1)

    rows = [
        ("Optical character recognition", "1200.00"),
        ("Index maintenance, August", "340.50"),
        ("Storage and backup", "89.00"),
    ]
    y = 395
    for desc, amount in rows:
        d.text((60, y), desc, fill="black", font=body)
        d.text((700, y), amount, fill="black", font=body)
        y += 55

    d.line((60, y + 15, 940, y + 15), fill="black", width=2)
    d.text((60, y + 40), "Total due", fill="black", font=bold)
    d.text((700, y + 40), "1629.50", fill="black", font=bold)
    d.text((60, y + 140), "Payment terms: net 30 days", fill="black", font=body)
    img.save(path)


def ui_panel(path: pathlib.Path) -> None:
    """Light text on a dark chrome, the way an application screenshot looks.

    This is the shape of the real corpus this tool was built for, and it is the
    one that most often OCRs badly: thin antialiased glyphs on a mid-dark
    background, at UI sizes rather than document sizes.
    """
    img = Image.new("RGB", (1100, 700), (30, 32, 38))
    d = ImageDraw.Draw(img)
    ui, ui_bold, mono = font("sans", 26), font("sans_bold", 30), font("mono", 24)

    d.rectangle((0, 0, 1100, 64), fill=(22, 24, 28))
    d.text((28, 18), "Deployment Settings", fill=(235, 237, 240), font=ui_bold)

    d.rectangle((0, 64, 300, 700), fill=(40, 43, 50))
    for i, item in enumerate(["Overview", "Environment", "Secrets", "Logs", "Danger zone"]):
        d.text((32, 100 + i * 48), item, fill=(200, 204, 212), font=ui)

    d.text((340, 110), "Region", fill=(160, 165, 175), font=ui)
    d.text((340, 148), "europe-west3", fill=(235, 237, 240), font=ui)
    d.text((340, 210), "Build command", fill=(160, 165, 175), font=ui)
    d.rectangle((340, 246, 1040, 296), fill=(20, 22, 26), outline=(70, 75, 85))
    d.text((356, 258), "cargo build --release", fill=(200, 235, 205), font=mono)
    d.text((340, 330), "Health check path", fill=(160, 165, 175), font=ui)
    d.rectangle((340, 366, 1040, 416), fill=(20, 22, 26), outline=(70, 75, 85))
    d.text((356, 378), "/healthz", fill=(200, 235, 205), font=mono)

    d.rectangle((340, 480, 520, 534), fill=(60, 110, 200))
    d.text((372, 494), "Save changes", fill=(255, 255, 255), font=ui)
    d.text((340, 570), "Last deployed 6 minutes ago", fill=(150, 155, 165), font=ui)
    img.save(path)


def low_contrast_scan(path: pathlib.Path) -> None:
    """Grey on off-white with paper grain and a slight blur — a photographed or
    badly-scanned page. Exercises the confidence floor: words here come back
    with genuinely lower confidence than the clean fixtures, so this is the
    fixture that notices if the `--min-conf` default starts eating real text."""
    rng = random.Random(20260817)
    # Greyscale: a scan has no colour information, and the per-pixel grain below
    # triples the PNG size in RGB for nothing. This is the largest fixture even
    # so — keep an eye on it if you raise the noise amplitude.
    img = Image.new("L", (1000, 620), 236)
    d = ImageDraw.Draw(img)
    body = font("serif", 34)

    lines = [
        "The archive was stored in a damp room for",
        "eleven years before anyone thought to scan it.",
        "Most pages survived. The ink faded unevenly,",
        "which is why the contrast is poor throughout.",
    ]
    for i, line in enumerate(lines):
        d.text((60, 80 + i * 70), line, fill=104, font=body)
    d.text((60, 420), "Reference number 44-119-B", fill=113, font=body)

    # Paper grain, then a light blur so the noise is not pixel-sharp. Seeded, so
    # regenerating produces the same image.
    px = img.load()
    for y in range(img.height):
        for x in range(img.width):
            px[x, y] = max(0, min(255, px[x, y] + rng.randint(-9, 9)))
    img.filter(ImageFilter.GaussianBlur(0.6)).save(path)


def rotated_page(path: pathlib.Path) -> None:
    """A page put on the scanner crooked. Four degrees is enough to break naive
    line-finding and well within what Tesseract's deskew handles, which is the
    point: it should still read."""
    page = Image.new("RGB", (900, 620), "white")
    d = ImageDraw.Draw(page)
    head, body = font("sans_bold", 40), font("sans", 32)

    d.text((60, 60), "Meeting notes", fill="black", font=head)
    for i, line in enumerate(
        [
            "The migration is scheduled for Thursday.",
            "Rollback takes about twenty minutes.",
            "Nobody deploys on Friday afternoon.",
        ]
    ):
        d.text((60, 150 + i * 60), line, fill="black", font=body)
    d.text((60, 400), "Owner: platform team", fill="black", font=body)

    page.rotate(-4, resample=Image.BICUBIC, expand=True, fillcolor="white").save(path)


def two_column(path: pathlib.Path) -> None:
    """Two columns with a rule between them. Reading order across columns is
    where page-segmentation choices show up: sparse mode returns the words but
    not necessarily in newspaper order, and the gate asserts substrings rather
    than a transcript precisely so that stays a non-issue."""
    img = Image.new("RGB", (1100, 720), "white")
    d = ImageDraw.Draw(img)
    head, body = font("sans_bold", 38), font("serif", 28)

    d.text((60, 50), "Quarterly Field Report", fill="black", font=head)
    d.line((550, 120, 550, 660), fill=(120, 120, 120), width=2)

    left = [
        "Rainfall in the northern",
        "districts was well above",
        "the seasonal average, and",
        "the reservoirs recovered",
        "faster than forecast.",
        "",
        "Three monitoring stations",
        "were offline in July.",
    ]
    right = [
        "Southern districts saw the",
        "opposite pattern. Irrigation",
        "demand rose sharply and",
        "two boreholes were drilled",
        "ahead of schedule.",
        "",
        "Replacement sensors ship",
        "in early September.",
    ]
    for i, line in enumerate(left):
        d.text((60, 160 + i * 48), line, fill="black", font=body)
    for i, line in enumerate(right):
        d.text((600, 160 + i * 48), line, fill="black", font=body)
    img.save(path)


FIXTURES = {
    "hello_world.png": hello_world,
    "croatian_text.png": croatian_text,
    "invoice.png": invoice,
    "ui_panel.png": ui_panel,
    "low_contrast_scan.png": low_contrast_scan,
    "rotated_page.png": rotated_page,
    "two_column.png": two_column,
}


def main() -> None:
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    only = sys.argv[1:]
    for name, build in FIXTURES.items():
        if only and name not in only and name.removesuffix(".png") not in only:
            continue
        path = OUT_DIR / name
        build(path)
        print(f"wrote {path} ({path.stat().st_size} bytes)")
    print("\nNow run `just recall` and update tests/fixtures/README.md if the")
    print("recovered text changed.")


if __name__ == "__main__":
    main()

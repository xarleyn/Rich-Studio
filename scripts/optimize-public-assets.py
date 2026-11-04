"""Rebuild public derivatives; no image or font tooling is needed on the server.

Install development tools in an isolated environment:
  python -m venv work/seo-tools
  work/seo-tools/Scripts/python -m pip install pillow 'fonttools[woff]' brotli
  work/seo-tools/Scripts/python scripts/optimize-public-assets.py
"""

import hashlib
import json
from pathlib import Path

import brotli
from fontTools import subset
from fontTools.ttLib import TTFont
from PIL import Image, ImageDraw, ImageFont, ImageOps

ROOT = Path(__file__).resolve().parent.parent
STATIC = ROOT / "app" / "static"


def sha(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main() -> None:
    font_source = STATIC / "fonts" / "manrope.ttf"
    font = TTFont(font_source, recalcTimestamp=False)
    options = subset.Options()
    options.recalc_timestamp = False
    subsetter = subset.Subsetter(options=options)
    unicode_ranges = ((0x20, 0x250), (0x400, 0x530), (0x1E00, 0x1F00), (0x2000, 0x2300))
    subsetter.populate(unicodes={point for start, end in unicode_ranges for point in range(start, end)})
    subsetter.subset(font)
    font.flavor = "woff2"
    font.save(STATIC / "fonts" / "manrope.woff2")
    font.close()

    derived = {}
    for name in ("landing-editor", "landing-preview", "landing-schedule"):
        source = STATIC / "images" / f"{name}.jpg"
        output = source.with_suffix(".webp")
        with Image.open(source) as image:
            image.save(output, "WEBP", quality=86, method=6)
        derived[output.relative_to(STATIC).as_posix()] = {
            "source": source.relative_to(STATIC).as_posix(),
            "source_sha256": sha(source),
            "sha256": sha(output),
        }

    with Image.open(STATIC / "icons" / "icon-512.png") as icon:
        icon.resize((120, 120), Image.Resampling.LANCZOS).save(STATIC / "icons" / "icon-120.png", optimize=True)
        icon.save(STATIC / "icons" / "favicon.ico", sizes=[(16, 16), (32, 32), (48, 48), (64, 64)])

    # A sharing image uses the real product screenshot, never fictional interface text.
    social = Image.new("RGB", (1200, 630), "#f6f7fb")
    draw = ImageDraw.Draw(social)
    brand_font = ImageFont.truetype(str(font_source), 32)
    draw.text((40, 16), "RichPost", font=brand_font, fill="#182336")
    with Image.open(STATIC / "images" / "landing-editor.jpg") as screenshot:
        preview = ImageOps.contain(screenshot, (1120, 530), Image.Resampling.LANCZOS)
        social.paste(preview, ((1200 - preview.width) // 2, 82))
    social.save(STATIC / "images" / "social-preview.png", optimize=True)

    compressed = {}
    for name in ("landing.css", "landing.js"):
        source = STATIC / name
        output = source.with_name(source.name + ".br")
        output.write_bytes(brotli.compress(source.read_bytes(), quality=11))
        compressed[name] = {"source_sha256": sha(source), "brotli_sha256": sha(output)}

    manifest = {
        "font": {"source_sha256": sha(font_source), "sha256": sha(STATIC / "fonts" / "manrope.woff2")},
        "derived": derived,
        "compressed": compressed,
    }
    (ROOT / "app" / "bundled" / "public-assets.json").write_text(json.dumps(manifest, indent=2) + "\n", "utf-8")
    for path in [
        STATIC / "fonts" / "manrope.woff2",
        *STATIC.glob("images/*.webp"),
        STATIC / "images" / "social-preview.png",
        *STATIC.glob("*.br"),
    ]:
        print(f"{path.relative_to(STATIC).as_posix()}: {path.stat().st_size} bytes")


if __name__ == "__main__":
    main()

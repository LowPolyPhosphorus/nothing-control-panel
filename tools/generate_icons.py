"""Generate the original tray/app icons (no third-party artwork)."""
from pathlib import Path
from PIL import Image, ImageDraw

OUT = Path(__file__).resolve().parents[1] / "src-tauri" / "icons"
OUT.mkdir(parents=True, exist_ok=True)

S = 512
img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
d = ImageDraw.Draw(img)
d.rounded_rectangle((48, 48, 464, 464), radius=110, fill=(28, 32, 36, 255))


def bud(cx: int, tilt: int) -> Image.Image:
    layer = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    ld = ImageDraw.Draw(layer)
    ld.ellipse((cx - 46, 150, cx + 46, 262), fill=(242, 244, 246, 255))
    ld.rounded_rectangle((cx - 23, 230, cx + 23, 350), radius=22, fill=(242, 244, 246, 255))
    ld.ellipse((cx - 12, 176, cx + 12, 200), fill=(120, 128, 136, 255))
    return layer.rotate(tilt, resample=Image.Resampling.BICUBIC, center=(cx, 250))


img = Image.alpha_composite(img, bud(190, -12))
img = Image.alpha_composite(img, bud(322, 12))

img.save(OUT / "icon-512.png")
for size in (32, 64, 128, 256):
    img.resize((size, size), Image.Resampling.LANCZOS).save(OUT / f"icon-{size}.png")

img.resize((32, 32), Image.Resampling.LANCZOS).save(OUT / "icon.png")
img.resize((256, 256), Image.Resampling.LANCZOS).save(
    OUT / "icon.ico",
    sizes=[(16, 16), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
)
print("Generated icons in", OUT)

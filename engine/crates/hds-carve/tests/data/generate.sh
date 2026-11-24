#!/usr/bin/env bash
# Regenerates the sample files in this directory.
#
# Every sample is written by a real encoder (the tools listed below), not by
# hand, so that the tests exercise what real software puts on disk. The files
# are committed; this script documents where they came from and lets them be
# rebuilt. Encoders embed version strings and time stamps, so a rebuild is not
# byte-identical - the tests only rely on each file being a valid file whose
# true length is its size on disk.
#
# Tools (Ubuntu 24.04 package names):
#   apt:  ffmpeg imagemagick zip 7zip rar mkvtoolnix qpdf ghostscript
#         libimage-exiftool-perl lame webp gifsicle libtiff-tools optipng
#         libjpeg-turbo-progs libheif-examples libheif-plugin-x265
#         libheif-plugin-aomenc
#   pip:  pillow numpy tifffile imagecodecs reportlab pypdf python-docx
#         openpyxl python-pptx odfdo ebooklib
#   optional:
#     PIDNG_SRC  path to an unpacked `pidng` source tree with its extension
#                built in place (pip download pidng; python3 setup.py
#                build_ext --inplace) - writes the DNG sample.
#     RAR4, RAR6 paths to older rar binaries. RAR 7 can no longer create
#                RAR 4.x archives; RAR 4.20 and 6.23 are still in the Ubuntu
#                pool (archive.ubuntu.com/ubuntu/pool/multiverse/r/rar/,
#                rar_4.2.0-1_amd64.deb and rar_6.23-1~22.04.1_amd64.deb;
#                unpack with dpkg-deb -x).
#
# Usage: tests/data/generate.sh            (writes next to itself)
set -euo pipefail

OUT="$(cd "$(dirname "$0")" && pwd)"
W="$(mktemp -d)"
trap 'rm -rf "$W"' EXIT
cd "$W"
FF="ffmpeg -hide_banner -loglevel error -y"
export SOURCE_DATE_EPOCH=1700000000

say() { printf '== %s\n' "$*"; }

# ---------------------------------------------------------------- sources --
say "source material"
python3 - <<'EOF'
import math, random
from PIL import Image
rnd = random.Random(20240501)

def photo(w, h, mode="RGB", noise=10):
    img = Image.new("RGB", (w, h))
    px = img.load()
    for y in range(h):
        for x in range(w):
            r = 128 + 100 * math.sin(x / 9.0) * math.cos(y / 13.0)
            g = 255.0 * x / w
            b = 255.0 * y / h
            n = rnd.randint(-noise, noise)
            px[x, y] = tuple(max(0, min(255, int(v + n))) for v in (r, g, b))
    return img.convert(mode)

photo(160, 120).save("base.png")
photo(160, 120).save("base.ppm")
photo(48, 36).save("small.png")
photo(96, 72, noise=0).save("smooth.png")
photo(64, 48).save("thumb_src.png")
a = photo(64, 48).convert("RGBA")
ap = a.load()
for y in range(48):
    for x in range(64):
        r, g, b, _ = ap[x, y]
        ap[x, y] = (r, g, b, int(255 * x / 63))
a.save("alpha.png")
frames = []
for i in range(4):
    f = photo(48, 36)
    for x in range(48):
        f.putpixel((x, (i * 9) % 36), (255, 255, 255))
    frames.append(f)
for i, f in enumerate(frames):
    f.save(f"frame{i}.png")
EOF
cat > text1.txt <<'EOF'
HDSavior sample text file. The quick brown fox jumps over the lazy dog.
Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod
tempor incididunt ut labore et dolore magna aliqua.
EOF
for i in $(seq 1 40); do echo "line $i of a slightly longer text file used as archive content"; done > text2.txt
head -c 6000 /dev/urandom > noise.bin
$FF -f lavfi -i "testsrc2=size=64x48:rate=10" -f lavfi -i "sine=frequency=440:sample_rate=22050" \
    -t 1 -c:v rawvideo -pix_fmt yuv420p -c:a pcm_s16le src.nut
$FF -f lavfi -i "sine=frequency=330:sample_rate=44100" -t 1.2 -ac 2 tone.wav

# ------------------------------------------------------------------ JPEG --
say "jpeg"
python3 - <<'EOF'
from PIL import Image, ImageCms
b = Image.open("base.png")
b.save("jpeg_baseline.jpg", quality=80, subsampling=2)
b.save("jpeg_progressive.jpg", quality=75, progressive=True, optimize=True)
b.convert("L").save("jpeg_gray.jpg", quality=70)
b.convert("CMYK").save("jpeg_cmyk_adobe.jpg", quality=60)
b.save("jpeg_icc.jpg", quality=70, subsampling=0,
       icc_profile=ImageCms.ImageCmsProfile(ImageCms.createProfile("sRGB")).tobytes())
Image.open("thumb_src.png").save("thumb.jpg", quality=60)
b.save("exif_src.jpg", quality=78)
b.save("special_jpeg_mpo.jpg", format="MPO", save_all=True,
       append_images=[Image.open("small.png")], quality=70)
EOF
cjpeg -quality 70 -restart 2B -outfile jpeg_restart.jpg base.ppm
cjpeg -quality 70 -arithmetic -outfile jpeg_arithmetic.jpg base.ppm
cjpeg -quality 70 -progressive -restart 2 -outfile jpeg_progressive_restart.jpg base.ppm
convert base.png -sampling-factor 1x1 -quality 72 -set comment "written by ImageMagick" jpeg_imagemagick_comment.jpg
$FF -i base.png -frames:v 1 -q:v 5 -f image2 jpeg_ffmpeg.jpg
# camera-like: EXIF first (no JFIF segment), with a thumbnail JPEG inside APP1
cp exif_src.jpg jpeg_exif_thumbnail.jpg
exiftool -q -overwrite_original -Make="HDSavior" -Model="Test Camera 1" \
    -DateTimeOriginal="2024:05:01 12:00:00" -ExposureTime=1/125 -FNumber=5.6 \
    -GPSLatitude=48.2 -GPSLatitudeRef=N -GPSLongitude=16.37 -GPSLongitudeRef=E \
    "-ThumbnailImage<=thumb.jpg" -JFIF:all= jpeg_exif_thumbnail.jpg
cp exif_src.jpg jpeg_jfif_exif_xmp.jpg
exiftool -q -overwrite_original -Artist="Somebody" -XMP-dc:Title="A title" \
    -IPTC:Keywords="rescue" jpeg_jfif_exif_xmp.jpg

# ------------------------------------------------------------------- PNG --
say "png"
python3 - <<'EOF'
from PIL import Image
b = Image.open("base.png")
b.save("png_rgb.png")
Image.open("alpha.png").save("png_rgba.png", optimize=True)
fr = [Image.open(f"frame{i}.png") for i in range(4)]
fr[0].save("png_apng.png", save_all=True, append_images=fr[1:], duration=100, loop=0)
EOF
convert base.png -colors 32 -interlace PNG -set comment "interlaced palette" png_palette_interlaced.png
convert small.png -depth 16 -define png:color-type=2 png_rgb16.png
cp png_rgb.png png_optipng.png && optipng -quiet -o2 png_optipng.png
$FF -i small.png -frames:v 1 -f image2 png_ffmpeg.png
convert base.png -colorspace Gray -define png:compression-level=0 -resize 64x48 png_gray_stored.png

# ------------------------------------------------------------------- GIF --
say "gif"
python3 - <<'EOF'
from PIL import Image
b = Image.open("base.png")
b.convert("P", palette=Image.ADAPTIVE, colors=64).save("gif_static.gif")
fr = [Image.open(f"frame{i}.png").convert("P", palette=Image.ADAPTIVE, colors=32) for i in range(4)]
fr[0].save("gif_animated.gif", save_all=True, append_images=fr[1:], duration=120, loop=0)
EOF
$FF -i src.nut -frames:v 4 -an gif_ffmpeg.gif
gifsicle -O3 --comment "optimised by gifsicle" gif_animated.gif -o gif_gifsicle_comment.gif
convert base.png -colors 16 -interlace GIF gif_imagemagick_interlaced.gif

# ------------------------------------------------------------------- BMP --
say "bmp"
python3 - <<'EOF'
from PIL import Image
s = Image.open("small.png")
s.save("bmp_rgb24.bmp")
s.convert("P", palette=Image.ADAPTIVE, colors=200).save("bmp_palette8.bmp")
Image.open("base.png").convert("1").save("bmp_mono1.bmp")
EOF
convert alpha.png bmp_imagemagick_v5_rgba.bmp
convert small.png BMP3:bmp_imagemagick_v3.bmp
convert small.png BMP2:bmp_imagemagick_v2_core.bmp
convert base.png -colors 8 -type Palette -compress RLE BMP3:bmp_rle8.bmp
$FF -i small.png -frames:v 1 -f image2 bmp_ffmpeg.bmp
convert small.png -resize 47x35! bmp_odd_width.bmp

# ------------------------------------------------------------------ TIFF --
say "tiff"
python3 - <<'EOF'
import os, sys
import numpy as np
from PIL import Image
import tifffile
b = Image.open("base.png")
s = Image.open("small.png")
s.save("tiff_pillow_uncompressed.tif")
fr = [Image.open(f"frame{i}.png") for i in range(4)]
# Pillow pads every appended frame to a multiple of 16 bytes: the file ends
# with a few zero bytes that nothing in it refers to
fr[0].save("special_tiff_pillow_multipage_padded.tif", save_all=True, append_images=fr[1:], compression="tiff_lzw")
m = Image.open("smooth.png")
m.save("src_for_tiffcp.tif")
a = np.asarray(m)
# pyramid with SubIFDs, as written by a scientific/GIS style encoder
with tifffile.TiffWriter("tiff_tifffile_subifds.tif") as t:
    t.write(a, subifds=2, compression="zlib", tile=(32, 32), photometric="rgb")
    t.write(a[::2, ::2], subfiletype=1, compression="zlib", photometric="rgb")
    t.write(a[::4, ::4], subfiletype=1, compression="zlib", photometric="rgb")
# RAW-like layout: small RGB preview in IFD0, colour-filter-array data in a
# SubIFD, camera Make - the structure a TIFF-based camera RAW file has.
cfa = (np.asarray(b.convert("L")).astype(np.uint16) << 4)
with tifffile.TiffWriter("tiff_rawlike_nikon.tif") as t:
    t.write(a[::4, ::4], photometric="rgb", subfiletype=1, subifds=1,
            extratags=[(271, "s", 0, "NIKON CORPORATION", True), (272, "s", 0, "NIKON TEST", True)])
    t.write(cfa, photometric=32803, subfiletype=0, compression=None,
            extratags=[(33421, "H", 2, (2, 2), True), (33422, "B", 4, (0, 1, 1, 2), True)])
src = os.environ.get("PIDNG_SRC")
if src:
    sys.path.insert(0, os.path.join(src, "src"))
    from pidng.core import RAW2DNG, DNGTags, Tag
    from pidng.defs import CFAPattern, CalibrationIlluminant, DNGVersion, PhotometricInterpretation, PreviewColorSpace
    h, w = cfa.shape
    t = DNGTags()
    t.set(Tag.ImageWidth, w); t.set(Tag.ImageLength, h)
    t.set(Tag.TileWidth, w); t.set(Tag.TileLength, h)
    t.set(Tag.Orientation, 1)
    t.set(Tag.PhotometricInterpretation, PhotometricInterpretation.Color_Filter_Array)
    t.set(Tag.SamplesPerPixel, 1); t.set(Tag.BitsPerSample, 12)
    t.set(Tag.CFARepeatPatternDim, [2, 2]); t.set(Tag.CFAPattern, CFAPattern.GBRG)
    t.set(Tag.BlackLevel, 256); t.set(Tag.WhiteLevel, 4095)
    t.set(Tag.ColorMatrix1, [[9, 10], [0, 1], [0, 1], [0, 1], [9, 10], [0, 1], [0, 1], [0, 1], [9, 10]])
    t.set(Tag.CalibrationIlluminant1, CalibrationIlluminant.D65)
    t.set(Tag.AsShotNeutral, [[1, 1], [1, 1], [1, 1]])
    t.set(Tag.BaselineExposure, [[0, 100]])
    t.set(Tag.Make, "HDSavior"); t.set(Tag.Model, "Test Camera 1"); t.set(Tag.UniqueCameraModel, "HDSavior Test Camera 1")
    t.set(Tag.DNGVersion, DNGVersion.V1_4); t.set(Tag.DNGBackwardVersion, DNGVersion.V1_2)
    t.set(Tag.PreviewColorSpace, PreviewColorSpace.sRGB)
    r = RAW2DNG(); r.options(t, path="", compress=False); r.convert(cfa, filename="tiff_pidng")
    r = RAW2DNG(); r.options(t, path="", compress=True); r.convert(cfa, filename="tiff_pidng_ljpeg")
else:
    print("PIDNG_SRC not set: DNG samples not regenerated", file=sys.stderr)
EOF
tiffcp -B -t -w 32 -l 32 -c lzw src_for_tiffcp.tif tiff_libtiff_tiled_bigendian.tif
tiffcp -c jpeg:r:75 -r 16 src_for_tiffcp.tif tiff_libtiff_jpeg.tif
tiffcp -c none -r 1 tiff_pillow_uncompressed.tif tiff_libtiff_one_row_strips.tif
convert small.png -compress Zip tiff_imagemagick_exif_gps.tif
exiftool -q -overwrite_original -Make="HDSavior" -Model="Scanner 2" -DateTimeOriginal="2024:05:01 12:00:00" \
    -GPSLatitude=48.2 -GPSLatitudeRef=N -XMP-dc:Title="A title" tiff_imagemagick_exif_gps.tif
convert base.png -colorspace Gray -compress Group4 -monochrome tiff_imagemagick_g4.tif
[ -f tiff_pidng.dng ] && mv tiff_pidng.dng tiff_pidng_dng.dng
[ -f tiff_pidng_ljpeg.dng ] && mv tiff_pidng_ljpeg.dng tiff_pidng_ljpeg_dng.dng

# ------------------------------------------------------------------- PDF --
say "pdf"
python3 - <<'EOF'
from reportlab.lib.pagesizes import A5
from reportlab.pdfgen import canvas
c = canvas.Canvas("pdf_reportlab.pdf", pagesize=A5)
for p in range(2):
    c.setFont("Helvetica", 14)
    c.drawString(60, 500, f"HDSavior sample PDF, page {p + 1}")
    for i in range(12):
        c.drawString(60, 470 - 18 * i, f"line {i}: the quick brown fox jumps over the lazy dog")
    c.showPage()
c.save()
c = canvas.Canvas("pdf_reportlab_image.pdf", pagesize=A5, pageCompression=0)
c.drawImage("small.png", 60, 300, width=240, height=180)
c.drawString(60, 280, "uncompressed page stream, embedded image")
c.showPage(); c.save()
from pypdf import PdfWriter, PdfReader
w = PdfWriter()
w.append(PdfReader("pdf_reportlab.pdf"))
w.add_metadata({"/Title": "written by pypdf"})
w.write("pdf_pypdf.pdf")
EOF
cat > page.ps <<'EOF'
%!PS
/Helvetica findfont 14 scalefont setfont
72 700 moveto (Written by Ghostscript ps2pdf) show
72 680 moveto (second line) show
newpath 72 600 moveto 200 0 rlineto 0 50 rlineto -200 0 rlineto closepath stroke
showpage
EOF
gs -q -dNOPAUSE -dBATCH -sDEVICE=pdfwrite -sOutputFile=pdf_ghostscript.pdf page.ps
qpdf --object-streams=generate pdf_reportlab.pdf pdf_qpdf_object_streams.pdf
qpdf --linearize pdf_reportlab.pdf pdf_qpdf_linearized.pdf
qpdf --linearize --object-streams=generate pdf_ghostscript.pdf pdf_qpdf_linearized_objstm.pdf
convert small.png pdf_imagemagick_image.pdf
# incremental updates: pypdf, then exiftool, each append a new section
# (objects, cross-reference table, trailer, %%EOF) instead of rewriting
python3 - <<'EOF'
from pypdf import PdfReader, PdfWriter
w = PdfWriter(PdfReader("pdf_reportlab.pdf"), incremental=True)
w.add_metadata({"/Subject": "appended by pypdf"})
w.write("pdf_incremental_update.pdf")
EOF
exiftool -q -overwrite_original -Title="updated again" pdf_incremental_update.pdf
cp pdf_qpdf_linearized.pdf pdf_linearized_then_updated.pdf
exiftool -q -overwrite_original -Title="linearized, then updated" pdf_linearized_then_updated.pdf
# a PDF that carries another complete PDF as an uncompressed attachment:
# the inner %%EOF and startxref must not end the outer file
qpdf pdf_ghostscript.pdf --add-attachment pdf_reportlab_image.pdf -- attach_tmp.pdf
qpdf --stream-data=uncompress --object-streams=disable attach_tmp.pdf pdf_embedded_pdf_attachment.pdf

# ------------------------------------------------------------------- ZIP --
say "zip"
mkdir -p ztree/sub/empty && cp text1.txt ztree/ && cp text2.txt ztree/sub/ && cp small.png ztree/sub/
(cd ztree && zip -q -9 -r ../zip_infozip_deflate.zip .)
(cd ztree && echo "archive comment written by zip -z" | zip -q -0 -r -z ../zip_infozip_stored_comment.zip .)
(cd ztree && zip -q -r -P secret ../zip_infozip_encrypted.zip text1.txt sub/text2.txt)
(cd ztree && zip -q -fz -r ../zip_infozip_forced_zip64.zip text1.txt sub/text2.txt)
(cd ztree && cat text1.txt | zip -q ../zip_infozip_stdin_zip64.zip -)
(cd ztree && zip -q -fd -r ../zip_infozip_forced_descriptors.zip text1.txt sub)
(cd ztree && 7z a -bso0 -bsp0 -tzip ../zip_7zip.zip text1.txt sub)
python3 - <<'EOF'
import io, zipfile

class Unseekable(io.RawIOBase):
    """A pipe-like sink: zipfile then has to use data descriptors."""
    def __init__(self, path): self.f = open(path, "wb")
    def writable(self): return True
    def write(self, b): return self.f.write(b)
    def close(self): self.f.close(); super().close()

t1 = open("text1.txt", "rb").read(); t2 = open("text2.txt", "rb").read()
png = open("small.png", "rb").read()
with zipfile.ZipFile(Unseekable("zip_python_stream_descriptors.zip"), "w", zipfile.ZIP_DEFLATED) as z:
    z.writestr("text1.txt", t1)
    z.writestr("dir/text2.txt", t2)
    z.writestr(zipfile.ZipInfo("stored.png"), png, zipfile.ZIP_STORED)
with zipfile.ZipFile(Unseekable("zip_python_stream_zip64.zip"), "w", zipfile.ZIP_DEFLATED) as z:
    with z.open("big-declared.txt", "w", force_zip64=True) as f: f.write(t2)
    with z.open("second.bin", "w", force_zip64=True) as f: f.write(png)
# a stored ZIP inside a streamed ZIP: the inner signatures and descriptors
# sit in the outer entry's data
inner = io.BytesIO()
with zipfile.ZipFile(Unseekable("inner_tmp.zip"), "w", zipfile.ZIP_DEFLATED) as z:
    z.writestr("a.txt", t1); z.writestr("b.txt", t2)
with zipfile.ZipFile(Unseekable("zip_python_nested_stored.zip"), "w", zipfile.ZIP_STORED) as z:
    z.writestr("inner.zip", open("inner_tmp.zip", "rb").read())
    z.writestr("after.txt", t1)
with zipfile.ZipFile("zip_jar_layout.jar", "w", zipfile.ZIP_DEFLATED) as z:
    z.writestr("META-INF/MANIFEST.MF", "Manifest-Version: 1.0\r\nCreated-By: HDSavior tests\r\n\r\n")
    z.writestr("org/example/Main.class", png)

import docx
d = docx.Document(); d.add_heading("HDSavior sample", 0); d.add_paragraph("Written by python-docx."); d.save("zip_ooxml_word.docx")
import openpyxl
wb = openpyxl.Workbook(); ws = wb.active
for r in range(1, 20): ws.append([r, r * r, f"row {r}"])
wb.save("zip_ooxml_excel.xlsx")
import pptx
p = pptx.Presentation(); s = p.slides.add_slide(p.slide_layouts[0]); s.shapes.title.text = "HDSavior sample"; p.save("zip_ooxml_powerpoint.pptx")
from odfdo import Document, Paragraph
o = Document("text"); o.body.clear(); o.body.append(Paragraph("Written by odfdo.")); o.save("zip_odf_text.odt")
o = Document("spreadsheet"); o.save("zip_odf_spreadsheet.ods")
from ebooklib import epub
b = epub.EpubBook(); b.set_identifier("hdsavior-sample"); b.set_title("HDSavior sample"); b.set_language("en")
c = epub.EpubHtml(title="One", file_name="one.xhtml", lang="en"); c.content = "<h1>One</h1><p>Written by ebooklib.</p>"
b.add_item(c); b.toc = (c,); b.add_item(epub.EpubNcx()); b.add_item(epub.EpubNav()); b.spine = ["nav", c]
epub.write_epub("zip_epub.epub", b)
EOF

# ------------------------------------------------------- ISO-BMFF family --
say "mp4 / mov / heic"
X264="-c:v libx264 -preset veryfast -pix_fmt yuv420p -g 5"
$FF -i src.nut $X264 -c:a aac -b:a 24k mp4_ffmpeg_moov_last.mp4
$FF -i src.nut $X264 -c:a aac -b:a 24k -movflags +faststart mp4_ffmpeg_faststart.mp4
$FF -i src.nut $X264 -c:a aac -b:a 24k -movflags frag_keyframe+empty_moov+default_base_moof mp4_ffmpeg_fragmented.mp4
$FF -i src.nut $X264 -c:a aac -b:a 24k -f mov mp4_ffmpeg_quicktime.mov
$FF -i src.nut -t 0.5 -c:v mjpeg -q:v 20 -c:a pcm_s16le -ar 8000 -ac 1 -f mov mp4_ffmpeg_quicktime_mjpeg_pcm.mov
$FF -i src.nut -vn -c:a aac -b:a 32k -f ipod mp4_ffmpeg_audio.m4a
$FF -i src.nut -s 128x96 -c:v h263 -c:a aac -b:a 24k -ar 16000 -f 3gp mp4_ffmpeg_3gp.3gp
$FF -i src.nut -c:v libx265 -preset ultrafast -x265-params log-level=error -tag:v hvc1 -an mp4_ffmpeg_hevc.mp4
heif-enc -q 50 -o heic_libheif.heic small.png >/dev/null
heif-enc -q 50 -A -o heic_libheif_avif.avif small.png >/dev/null
heif-enc -q 40 -t 32 -o heic_libheif_thumbnail.heic base.png >/dev/null
python3 - <<'EOF'
from PIL import Image
Image.open("small.png").save("heic_pillow_avif.avif", quality=50)
EOF

# ------------------------------------------------------------ RIFF family --
say "avi / wav / webp"
$FF -i tone.wav -ac 1 -ar 8000 -c:a pcm_s16le wav_pcm16.wav
$FF -i tone.wav -ac 1 -c:a pcm_u8 -af aresample=8000,atrim=end_sample=4001 wav_pcm8_odd_length.wav
$FF -i tone.wav -t 0.5 -ac 2 -ar 8000 -c:a pcm_s16le -rf64 always wav_rf64.wav
$FF -i tone.wav -ac 1 -ar 8000 -c:a pcm_s16le -write_bext 1 -metadata title="bext and LIST" wav_bext_list.wav
$FF -i tone.wav -ac 1 -ar 8000 -c:a adpcm_ms wav_adpcm_fact.wav
# Python's wave module, then a LIST INFO chunk appended behind the audio data
# the way tagging tools do it: the one WAV whose end is verified by structure
python3 - <<'PYEOF'
import math, struct, wave
with wave.open("w.wav", "wb") as w:
    w.setnchannels(1); w.setsampwidth(2); w.setframerate(8000)
    w.writeframes(b"".join(struct.pack("<h", int(8000 * math.sin(i / 9))) for i in range(3001)))
d = bytearray(open("w.wav", "rb").read())
info = b"INFO" + b"INAM" + struct.pack("<I", 12) + b"HDS sample\0\0"
if len(d) % 2:
    d += b"\0"
d += b"LIST" + struct.pack("<I", len(info)) + info
d[4:8] = struct.pack("<I", len(d) - 8)
open("wav_python_list_after_data.wav", "wb").write(d)
PYEOF
$FF -i src.nut -c:v mpeg4 -q:v 6 -c:a libmp3lame -b:a 32k avi_ffmpeg_mpeg4_mp3.avi
$FF -i src.nut -c:v mjpeg -q:v 10 -c:a pcm_s16le -ar 8000 -ac 1 avi_ffmpeg_mjpeg_pcm.avi
$FF -i src.nut -an -c:v mpeg4 -q:v 6 -reserve_index_space 4096 avi_ffmpeg_video_only.avi
cwebp -quiet -q 60 base.png -o webp_lossy.webp
cwebp -quiet -lossless small.png -o webp_lossless.webp
cwebp -quiet -q 60 -metadata all alpha.png -o webp_alpha.webp
cwebp -quiet -q 60 -metadata all jpeg_exif_thumbnail.jpg -o webp_exif.webp 2>/dev/null || cwebp -quiet -q 60 base.png -o webp_exif.webp
python3 - <<'EOF'
from PIL import Image
fr = [Image.open(f"frame{i}.png") for i in range(4)]
fr[0].save("webp_animated.webp", save_all=True, append_images=fr[1:], duration=100, loop=0, quality=50)
EOF

# -------------------------------------------------------- Matroska / WebM --
say "mkv / webm"
$FF -i src.nut $X264 -c:a libvorbis -b:a 32k mkv_ffmpeg.mkv
$FF -i src.nut -c:v libvpx -b:v 60k -c:a libopus -b:a 16k -ar 48000 mkv_ffmpeg_vp8_opus.webm
$FF -i src.nut -c:v libvpx-vp9 -b:v 60k -an -cluster_time_limit 200 mkv_ffmpeg_vp9_many_clusters.webm
printf 'CHAPTER01=00:00:00.000\nCHAPTER01NAME=Start\nCHAPTER02=00:00:00.500\nCHAPTER02NAME=Middle\n' > chapters.txt
mkvmerge -q -o mkv_mkvmerge.mkv --chapters chapters.txt --attach-file text1.txt --title "HDSavior sample" mp4_ffmpeg_moov_last.mp4
mkvmerge -q -o mkv_mkvmerge_audio.mka -D mp4_ffmpeg_moov_last.mp4
# no Cues and no Tags: the file ends in its last Cluster (end not verified by
# structure)
mkvmerge -q --no-cues --disable-track-statistics-tags -o mkv_mkvmerge_no_cues.mkv mp4_ffmpeg_moov_last.mp4
mkvpropedit -q mkv_mkvmerge_no_cues.mkv --tags all:
# written to a pipe: the muxer cannot seek back, the segment size stays unknown
$FF -i src.nut $X264 -c:a libvorbis -b:a 32k -cluster_time_limit 300 -f matroska - > special_mkv_unknown_size.mkv
$FF -i src.nut -c:v libvpx -b:v 60k -an -f webm -live 1 - > special_mkv_live_stream.webm

# ------------------------------------------------------------------- MP3 --
say "mp3"
$FF -i tone.wav -c:a libmp3lame -b:a 64k mp3_ffmpeg_cbr_id3v24.mp3
$FF -i tone.wav -c:a libmp3lame -q:a 6 -id3v2_version 3 -metadata title="vbr" mp3_ffmpeg_vbr_id3v23.mp3
$FF -i tone.wav -c:a libmp3lame -b:a 64k -write_xing 0 mp3_ffmpeg_no_xing.mp3
$FF -i tone.wav -c:a libmp3lame -b:a 64k -write_id3v1 1 -metadata title="both tags" mp3_ffmpeg_id3v1.mp3
$FF -i tone.wav -c:a libshine -b:a 64k mp3_ffmpeg_shine.mp3
lame --quiet -V 5 tone.wav mp3_lame_vbr_xing_first.mp3
lame --quiet -b 96 tone.wav mp3_lame_cbr_info_first.mp3
lame --quiet -b 64 --add-id3v2 --tt "title" --ta "artist" tone.wav mp3_lame_id3v2_and_id3v1.mp3
lame --quiet -m m --resample 22.05 -b 32 --id3v2-only --tt "mpeg2" tone.wav mp3_lame_mpeg2_mono.mp3
lame --quiet -m m --resample 11.025 -b 16 --id3v2-only --tt "mpeg2.5" tone.wav mp3_lame_mpeg25_mono.mp3
# no tag at all (lame -t): nothing but bare frames, which sniff() does not accept
lame --quiet -t -b 64 tone.wav special_mp3_bare_frames.mp3

# -------------------------------------------------------------------- 7z --
say "7z"
(cd ztree && 7z a -bso0 -bsp0 ../7z_lzma2.7z .)
(cd ztree && 7z a -bso0 -bsp0 -mhc=off ../7z_plain_header.7z text1.txt sub/text2.txt)
(cd ztree && 7z a -bso0 -bsp0 -mx0 ../7z_stored.7z text1.txt sub/small.png)
(cd ztree && 7z a -bso0 -bsp0 -mhe=on -psecret ../7z_encrypted_header.7z text1.txt sub/text2.txt)
(cd ztree && 7z a -bso0 -bsp0 -m0=ppmd -ms=on ../7z_ppmd_solid.7z .)

# ------------------------------------------------------------------- RAR --
say "rar"
cp noise.bin ztree/noise.bin
(cd ztree && rar a -idq -r ../rar_v5.rar .)
(cd ztree && echo "archive comment" > ../c.txt && rar a -idq -rr5 -z../c.txt ../rar_v5_recovery_comment.rar text1.txt sub/text2.txt)
(cd ztree && rar a -idq -m0 ../rar_v5_stored.rar text1.txt sub/small.png)
(cd ztree && rar a -idq -s -ma5 ../rar_v5_solid.rar text1.txt sub/text2.txt noise.bin)
# RAR 5 fills a volume up to its nominal size with zeros after the end block
(cd ztree && rar a -idq -v4k -m0 ../rar_v5_volume.rar noise.bin && mv ../rar_v5_volume.part1.rar ../special_rar_v5_volume_padded.rar && rm -f ../rar_v5_volume.part*.rar)
(cd ztree && rar a -idq -hpsecret ../special_rar_v5_encrypted_headers.rar text1.txt)
if [ -n "${RAR4:-}" ]; then
  (cd ztree && "$RAR4" a -idq -r ../rar_v4_rar420.rar .)
  (cd ztree && "$RAR4" a -idq -m0 -rr ../rar_v4_rar420_stored_recovery.rar text1.txt noise.bin)
  (cd ztree && "$RAR4" a -idq -hpsecret ../special_rar_v4_encrypted_headers.rar text1.txt)
  (cd ztree && "$RAR4" a -idq -v4k -m0 ../rar_v4_volume.rar noise.bin && mv ../rar_v4_volume.part1.rar ../rar_v4_first_volume.rar && rm -f ../rar_v4_volume.part*.rar)
else
  echo "RAR4 not set: RAR 4.20 samples not regenerated" >&2
fi
if [ -n "${RAR6:-}" ]; then
  (cd ztree && echo "archive comment" > ../c.txt && "$RAR6" a -idq -ma4 -z../c.txt ../rar_v4_rar623_comment.rar text1.txt sub/text2.txt)
  (cd ztree && "$RAR6" a -idq -ma4 -s ../rar_v4_rar623_solid.rar text1.txt sub/text2.txt noise.bin)
else
  echo "RAR6 not set: RAR 6.23 (-ma4) samples not regenerated" >&2
fi

# ------------------------------------------------------------------ copy --
say "copy"
shopt -s nullglob
n=0
PREFIXES="jpeg png gif bmp tiff pdf zip mp4 heic wav avi webp mkv mp3 7z rar special"
for p in $PREFIXES; do
  for f in "${p}"_*; do
    cp "$f" "$OUT/$f"; n=$((n + 1))
  done
done
# Samples that this run did not produce (optional tools missing, or a sample
# was renamed) are left alone; say which, so that stale ones get noticed.
for p in $PREFIXES; do
  for f in "$OUT/${p}"_*; do
    [ -e "$W/$(basename "$f")" ] || echo "kept, not regenerated: $(basename "$f")" >&2
  done
done
echo "$n samples written to $OUT"
du -cb "$OUT"/*.* | tail -1

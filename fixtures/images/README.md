# Small codec fixtures

Run `scripts/gen-fixtures.sh` from the checkout. It needs libvips 8.15+ with
lcms2, libheif (AV1 and HEVC encoders), libjxl, and Python 3. It does not fetch
images, profiles, or programs. Images are generated through the libvips CLI;
EXIF/XMP setters use that same installed library through Python ctypes.
The malicious samples are corruptions of those generated files.

`chart-p3-nclx.heic` and `chart-p3-nclx.avif` contain Display P3 pixels and
only a `colr/nclx` profile (primaries 12, transfer 13, identity matrix 0, full
range). The script losslessly encodes with libvips and replaces its generated
ICC property with NCLX, preserving property indices and all file offsets with
a trailing `free` property. They pass the same mean <1.0/max <2.5 delta E 2000
gates as the ICC chart inputs through all three derivative encoders.

`alpha-{10,12}.{avif,heic}` are 96x32 libvips-generated lossless RGBA images
with transparent, half and opaque vertical stripes. Committing these small
files lets Linux tests exercise high-bit-depth decode and JXL round trips with
decoder-only libheif installations, without CLI encoder/version dependencies.

`float.tif` is the chart converted to linear floating scRGB for decode benchmarks.
`1bit.png`, `2bit.png` and `4bit.png` are 32x32 packed grayscale patterns
written through the generator's libvips bindings. Pipeline tests identify their
original sample depths and ingest each through the complete default plan.
`gain-map.avif` is a 32x32 base plus a 16x16 gain image encoded locally with
libavif 1.4.1. It contains a valid `tmap` derived image and `dimg` item references,
with no gain-map text signatures. The inline C generator in
`scripts/gen-fixtures.sh` uses libavif >=1.2 development headers, pkg-config and
a C compiler, and independently decodes both images with libavif before saving.
These are generator-only requirements, not Rust build/test dependencies.

The 24 chart patches are original synthetic test values. `chart.json` records
RGB and their lcms2 D50 Lab values, with patch geometry. RGB, Display P3,
Adobe RGB 1998 and CMYK profiles here are the offline macOS ColorSync profiles
explicitly permitted by task 006. The alpha chart has a translucent first patch.
`landscape.jpg` is an original procedural textured landscape, not a photograph.
It exercises resampling without a third-party image or download.
`photo.jpg` is one of the owner's real photographs, supplied by the maintainer:
1600x1236, sRGB, stripped metadata, below 200 KB. It is the mandatory photographic
SSIM fixture (threshold 0.98 against linear `vips thumbnail` at 800px). The
generator preserves this photograph; it cannot regenerate an owner's original.

`photo-large.jpg` is the same maintainer-supplied photograph at 3400x2627,
sRGB with stripped metadata (about 1 MB). The default-plan pipeline fidelity
test uses it for long edges above 1600px, so medium2x, large and xlarge are
measured at their requested sizes. Smaller variants still use `photo.jpg`.

Two complete runs on libvips 8.18.7 produced identical SHA-256 manifests.
Bitwise equivalence across *different* libvips/lcms/encoder versions is not
claimed. Regeneration prints a manifest for comparison. Each generated image
is below 200 KB; the complete directory is below 3 MB.

## Large fixtures (not committed)

Sync these from tower for the additional large photographic test and RAW performance:

- `fixtures/large/24mp.jpg`: a 6000x4000 camera JPEG with an embedded profile.
- `fixtures/large/raw/sony.arw`
- `fixtures/large/raw/panasonic.rw2`
- `fixtures/large/raw/canon.cr2`
- `fixtures/large/raw/canon.cr3`
- `fixtures/large/raw/iphone.dng`
- `fixtures/large/raw/android.dng`

The 24 MP benchmark and RAW tests explicitly report a skip when absent.
RAW tests require `--features raw`; small HEIC tests require `--features heif`.

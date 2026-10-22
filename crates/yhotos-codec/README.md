# Codec boundary

Requires installed libvips >=8.15, lcms2 and libjxl development headers.
On macOS these are supplied by Homebrew libvips. The build script probes and
links installed libraries and generates bindings; it never downloads them.
`libvips` 2.3.0's Rust operation wrappers omit JPEG XL save/load, so this task
uses a minimal bindgen boundary rather than extending or bypassing those
wrappers. No native code is placed outside this crate.

`identify` probes headers only. JPEG and HEIF gain map detection reads metadata
segments/boxes and seeks past pixel payloads. JXL's basic-info and color headers
provide decoder agreement and identity directly through libjxl. Input is fed in 4 KiB
chunks, preserving unconsumed tails, and stops at `JXL_DEC_COLOR_ENCODING` without
requesting pixels. A cumulative `min(max_bytes, 16 MiB)` scan cap includes skipped
container boxes; a large pixel payload after the color header does not consume
that cap. Identify does not enter libvips' JXL header loader, which scans through
the container for frames and trailing metadata. Animated JXL returns `Unsupported`
because counting its pages would require that scan. Decode still uses libvips.
Generic loader probing is avoided because
short JXL streams can enter unrelated fallback decoders and hang. Limits are checked again before decode, and
supplied Identity values must agree with a fresh header probe.

Apple JPEG gain maps are found through the APP2 MPF index, then the indexed
auxiliary JPEGs' APP1 XMP. The index is capped at 64 images; its TIFF offsets
stay inside APP2, image ranges stay inside the file and cannot overlap, and
all JPEG headers share a 16 MiB scan budget. Primary-header signature text
alone is insufficient for Apple gain map detection.

ISO HEIF/AVIF gain maps are found through `meta/pitm`, `iinf/infe` item types
and `iref/dimg`: a `tmap` item must reference the primary image first and a
declared gain image second. Both 16-bit and 32-bit item ids are supported,
independently of box ordering. `iprp/ipco/auxC` retains Apple auxiliary type
detection; arbitrary pixel/unknown property bytes do not count as signatures.
HEIF and CR3 share checked finite box extents, eight nesting levels and 4096
total boxes. HEIF metadata shares a 16 MiB budget, and references are capped at
4096 per meta. These checks run before the native HEIF header loader.

HEIF/AVIF primary-image `colr/nclx` profiles are resolved through bounded
`iprp/ipco` and `ipma` associations (16/32-bit item ids, 7/15-bit property
indices), independent of property order. Thumbnail profiles do not affect the
primary image. Embedded ICC takes precedence; otherwise an lcms-generated RGB
ICC is attached before import and retained for export. `has_icc` still reports
whether the source contained an ICC; `icc_desc` describes the effective profile.
Supported CICP primaries/transfer pairs are BT.709/sRGB (1/13), BT.709 (1/1),
Display P3 (12/13), BT.2020 PQ (9/16), and BT.2020 HLG (9/18). Matrix codes
0, 1, 5, 6 and 9 and either range are handled by libheif's RGB decode; the ICC
applies only primaries and transfer. Other combinations return `Unsupported`.
PQ is normalized to its 10000-nit peak; HLG uses scene-linear inverse OETF
without a display OOTF or tone map. Transfers 16/18 themselves flag HDR inputs;
reserved HDR output requests still return `Unsupported`.

The materialized working image is unsigned 16-bit linear libvips D65 XYZ, scaled by 256
(about 0.004 XYZ units per step). Alpha is unassociated unsigned 16-bit. Each
resize starts from that original working image, center crops before a square
resize, and uses premultiplied alpha and Lanczos3. Export color-manages XYZ back
to the source RGB profile, or explicitly to sRGB. CMYK and gray profiles are
used at import and converted to an embedded sRGB profile for RGB-only output codecs. Only ICC is retained by encoders;
metadata-bearing derivative requests and reserved HDR requests are rejected.
Native handles own a reference, cannot cross threads, and never outlive global
initialization. Native errors are mapped to fixed public errors.

TIFF and BigTIFF dimensions are checked by our bounded directory reader before
libvips is called, including chained IFDs and nested SubIFDs. Directory cycles,
excessive counts, and invalid offsets are rejected. These checks do not depend
on which error a particular libtiff/libvips version emits.
Every entry, including unknown tags, has its type, u32 count-times-size and
in-file payload extent validated before libvips or rawler sees it. The reader
caps entries at 4096 in total (and therefore per IFD) and sums all declared
payload lengths, including overlapping/inline values, against 256 MiB across
all directories. EXIF, GPS, interoperability and typed IFD links are included.
Overflow and budget failures return `Limits`; invalid types/extents return
`Decode` through identify, decode and metadata alike.

Alpha import uses the loader's effective opacity range: 8-bit 0..255, ordinary
16-bit 0..65535, and floating TIFF/scRGB 0..1. HEIF's 10/12-bit samples are
left-shifted into ushort, so their full opacity is
`((1 << bits) - 1) << (16 - bits)`. Alpha is scaled and rounded before casting to the
16-bit working alpha band; signed/complex alpha formats return `Unsupported`.
Committed 10/12-bit AVIF and HEIC fixtures, regenerated by the fixture script,
cover transparent, partial and opaque alpha through import and a lossless JXL
round trip, without requiring an encoder at test time.
Floating scRGB color bypasses `icc_import`'s device-RGB uchar cast and uses
`vips_scRGB2XYZ` directly, retaining floating precision until the working-image
quantization. Other floating interpretations return `Unsupported` rather than
silently quantizing normalized values as 8-bit device samples. The generated
float TIFF regression checks (0.25, 0.5, 0.75) against the mean <1.0/max <2.5
delta E 2000 gates in the working image and a lossless JXL round trip. It also
checks transparent, half and opaque alpha, including extended scRGB luminance.
JPEG fallback explicitly flattens alpha against white in the exported 16-bit
image before scaling and casting to uchar. This retains ICC and avoids the
JPEG saver's implicit flatten interpreting 8-bit alpha as RGB16 opacity.

RAW identification never calls rawler's `decode_dummy`, `full_image`, or
`preview_image`: those APIs can allocate and decode before returning dimensions.
TIFF-based RAW uses metadata-only dimension probes. Every embedded JPEG
candidate is checked against its own SOF dimensions, and the largest is decoded
from its encoded bytes by the color-managed libvips path, retaining its ICC.
Each candidate retains only its file offset, length and SOF dimensions; the
marker reader is bounded to the first 64 KiB. ICC/orientation/metadata use only
the JPEG headers through SOS under that same cap. Only decode buffers the
selected JPEG, after enforcing `min(max_bytes, 256 MiB)` (CR3 retains its tighter
64 MiB record cap). A sparse TIFF with 1x1 dimensions and a declared 512 MiB
preview identifies without buffering that payload, then rejects decode before
allocation. Selection-order tests prove only the winning JPEG is buffered.
TIFF JPEGInterchangeFormat previews and JPEG strips (including DNG SubIFDs),
and RW2 JPEG blobs are enumerated. Without a JPEG, rawler
performs the camera-default 16-bit demosaic after the sensor headers pass limits.
Other proprietary containers without a metadata-only probe return Unsupported;
they never fall back to an unbounded dummy decode. RAW fixtures are intentionally
optional and must be synced from tower. No RAW performance or fidelity claim is made without them.

Sensor lossless JPEG strips/tiles are checked against their own SOF dimensions
and component-expanded allocation width before rawler can decode, independently
of TIFF dimensions or the presence of a preview. TIFF tile geometry, Panasonic
sensor tags and DNG JPEG XL headers are checked
as well. Sensor marker scans share a 16 MiB budget and at most 4096 segments.
MakerNote IFDs use the same entry and payload validation and aggregate budgets
as ordinary TIFF directories, before every rawler entry point. Supported layouts:
Canon and Sony plain IFDs; Nikon after its
10-byte prefix and embedded TIFF header (byte order and offsets from that TIFF
header); Olympus OLYMPUS/OLYMP prefixes (including the equipment, image-processing
and 0x3000 directories rawler follows); Fujifilm's 12-byte prefix (little endian,
note-relative offsets). Unsupported consumed make/header layouts return `Unsupported`.
DNG dispatch (including Apple) ignores MakerNotes, so only their blob extent and
allocation budget are validated. The same applies to native TIFF decoders that
do not parse EXIF MakerNotes (Panasonic/Leica, Hasselblad, Phase One/Leaf, Mamiya
and Kodak DCS/DCR). Tests cover Apple identification, preview pixels, metadata,
invalid blob extents and oversized declared allocations. Directory
tables must fit inside the declared note; referenced entry payloads must fit in
the file using the vendor's offset base. Sony and Nikon count, oversized payload,
aggregate allocation and truncated-note regressions exercise identify, decode
and metadata. Before using rawler's generic Fujifilm helper, its inherited-endian,
absolute-offset view is checked too; DNG dispatch does not use that helper.
Canon Cr2OldOffset (0x0081) and slice
offset/length tables (0x00c5/0x00c6) have their referenced JPEG SOFs checked,
including component-expanded widths and in-file extents, before any rawler
entry point. `src/raw.rs` lists the validated decoder inputs by camera make.
Panasonic CameraIFD (0x0120) is an embedded TIFF, validated with the same reader
before any rawler entry point (also for rawler's Leica dispatch). A seekable
blob view enforces its own byte order, relative offsets and blob extent without
copying it; nested IFDs share the outer entry and allocation budgets. Nested
CameraIFD regressions cover invalid types, multiplication overflow, out-of-blob
payloads, truncated tables and aggregate budgets through identify/decode/metadata.
Beyond these container checks, rawler's internal allocation behavior is contained
by the Linux host's resource limits and seccomp sandbox. RAW is accepted only via
admin upload over Tailscale. This is the bounded residual risk accepted in review;
no additional rawler parser preflights are added here.
CR3 never enters rawler, including for metadata. Our BMFF walker locates JPEGs
in Canon's PRVW preview UUID (after its eight-byte prefix) and THMB records under
moov/the descriptor UUID, validates each record's extent and JPEG SOF against
limits, and passes the largest preview to the normal libvips JPEG import path.
Dimensions, orientation, ICC and available metadata come from that JPEG;
sensor-only CMT metadata is not extracted. A CR3 without a JPEG preview returns
`CodecError::Unsupported`, with no demosaic fallback. Successful identities keep
`Kind::Raw(RawMake("Canon"))`.

The walker retains finite positive extents, forward progress, eight nesting
levels, 4096 total boxes, the 16 MiB moov cap and CMP1 geometry checks. Preview
headers share a 16 MiB scan budget; at most 64 JPEG records are examined and only
the selected encoded JPEG is buffered (at most 64 MiB and max_bytes). Sample
counts and movie headers stay opaque; unknown UUID payloads are skipped by their
checked extent. The stsz count, unknown UUID/nested free, mvhd version 2 and
zero-sized child regressions exercise identify, decode and metadata rejections.
The RAW-enabled identify fuzz target exercises arbitrary files, nested CR3 boxes
and JPEG payloads inside a valid preview UUID.

The mandatory photographic resize test uses the owner's stripped sRGB
`fixtures/images/photo.jpg`, compares full-image local SSIM against linear
`vips thumbnail` at half size, and enforces 0.98. The tower 24 MP photographic
test remains an additional optional check.

Run normal codec benchmarks with `cargo bench -p yhotos-bench --bench codec`;
add `--features raw` to include CR3 preview identification and medium encodes.
`codec/identify_large_jxl` generates a reproducible 25.2 MP lossless grayscale JXL
larger than 16 MiB outside the timed loop; no large binary fixture is committed.

The ThumbHash DCT and packing follow the MIT-licensed reference by Evan Wallace:
https://github.com/evanw/thumbhash . Reference vectors were generated with the
unmodified JavaScript implementation, including alpha and non-square cases.

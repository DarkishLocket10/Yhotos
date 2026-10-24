#!/usr/bin/env python3
"""Generate synthetic SDR patches and an independent BT.709 RGB reference.

Only the local, digest-pinned ffmpeg is used. Reference pixels use BT.601/709
YCbCr equations, inverse OETFs and D65 RGB/XYZ matrices, not an ffmpeg filter.
"""
import json
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent
FFMPEG = ROOT.parent.parent / "target/video-tools/ffmpeg"
PATCHES = [(128, 96, 160), (64, 160, 112), (180, 150, 104)]
TO_XYZ = {
    "bt709": ((.41239080, .35758434, .18048079), (.21263901, .71516868, .07219232), (.01933082, .11919478, .95053215)),
    "smpte170m": ((.39352090, .36525808, .19167695), (.21237636, .70105986, .08656378), (.01873909, .11193393, .95838473)),
    "bt470bg": ((.4306190, .3415419, .1783091), (.2220379, .7066384, .0713236), (.0201853, .1295504, .9390944)),
    "smpte432": ((.48657095, .26566769, .19821729), (.22897456, .69173852, .07928691), (0, .04511338, 1.04394437)),
    "bt2020": ((.63695805, .14461690, .16888098), (.26270021, .67799807, .05930172), (0, .02807269, 1.06098506)),
}
FROM_XYZ_709 = ((3.24096994, -1.53738318, -.49861076), (-.96924364, 1.87596750, .04155506), (.05563008, -.20397696, 1.05697151))


def multiply(matrix, vector):
    return [sum(a * b for a, b in zip(row, vector)) for row in matrix]


def reference(primaries, transfer, matrix, full):
    kr, kb = {"smpte170m": (.299, .114), "bt470bg": (.299, .114), "bt709": (.2126, .0722), "bt2020nc": (.2627, .0593)}[matrix]
    def linear(v):
        if transfer == "iec61966-2-1":
            return v / 12.92 if v <= .04045 else ((v + .055) / 1.055) ** 2.4
        return v / 4.5 if v < .081 else ((v + .099) / 1.099) ** (1 / .45)
    def encode(v):
        return 4.5 * v if v < .018 else 1.099 * v ** .45 - .099
    result = []
    for y, cb, cr in PATCHES:
        y = y / 255 if full else (y - 16) / 219
        cb, cr = ((v - 128) / (255 if full else 224) for v in (cb, cr))
        r, b = y + 2 * (1 - kr) * cr, y + 2 * (1 - kb) * cb
        g = (y - kr * r - kb * b) / (1 - kr - kb)
        xyz = multiply(TO_XYZ[primaries], [linear(v) for v in (r, g, b)])
        rgb = multiply(FROM_XYZ_709, xyz)
        result.append([round(255 * max(0, min(1, encode(v)))) for v in rgb])
    return result


def main():
    cases = [
        ("bt601", "smpte170m", "smpte170m", "smpte170m", False, True),
        ("bt709", "bt709", "bt709", "bt709", False, True),
        ("p3", "smpte432", "iec61966-2-1", "bt709", False, True),
        ("srgb", "bt709", "iec61966-2-1", "bt709", False, True),
        ("bt2020", "bt2020", "bt2020-10", "bt2020nc", False, True),
        ("bt709-full", "bt709", "bt709", "bt709", True, True),
        ("untagged-sd", "smpte170m", "bt709", "smpte170m", False, False),
        ("bt601-pal", "bt470bg", "bt709", "bt470bg", False, True),
    ]
    expected = []
    with tempfile.TemporaryDirectory() as temp:
        raw = Path(temp) / "patches.yuv"
        raw.write_bytes(b"".join(bytes(v for patch in PATCHES for v in [patch[plane]] * (width // 3)) * height for plane, width, height in [(0, 144, 96), (1, 72, 48), (2, 72, 48)]))
        for name, primaries, transfer, matrix, full, tagged in cases:
            filename = f"color-{name}.mp4"
            cmd = [str(FFMPEG), "-v", "error", "-y", "-f", "rawvideo", "-pixel_format", "yuv420p", "-video_size", "144x96", "-framerate", "5", "-i", str(raw), "-vf", "loop=loop=4:size=1", "-frames:v", "5", "-c:v", "libx264", "-crf", "1", "-profile:v", "high", "-pix_fmt", "yuv420p", "-map_metadata", "-1"]
            if tagged:
                # Set frame properties too: the loop filter otherwise preserves
                # the raw input's unspecified primaries/transfer on FFmpeg 9.
                cmd[cmd.index("loop=loop=4:size=1")] += f",setparams=color_primaries={primaries}:color_trc={transfer}:colorspace={matrix}:range={'full' if full else 'limited'}"
                cmd += ["-color_primaries", primaries, "-color_trc", transfer, "-colorspace", matrix, "-color_range", "pc" if full else "tv"]
            subprocess.run(cmd + ["-movflags", "+faststart", str(ROOT / filename)], check=True)
            expected.append({"file": filename, "primaries": primaries, "transfer": transfer, "matrix": matrix, "full": full, "tagged": tagged, "rgb": reference(primaries, transfer, matrix, full)})
    (ROOT / "color-reference.json").write_text(json.dumps(expected, indent=2) + "\n")


if __name__ == "__main__":
    main()

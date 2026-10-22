#!/usr/bin/env bash
# Deterministic synthetic fixtures. Requires Python 3, libvips 8.15+, lcms2.
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
export VIPS_CONCURRENCY=2 LC_ALL=C SOURCE_DATE_EPOCH=0
python3 - <<'PY'
import ctypes as C, ctypes.util, hashlib, json, pathlib, shlex, struct, subprocess, tempfile, zlib
root=pathlib.Path.cwd(); dest=root/'fixtures/images'; dest.mkdir(parents=True,exist_ok=True)
lib=C.CDLL(ctypes.util.find_library('vips')); lib.vips_init.argtypes=[C.c_char_p]; lib.vips_init(b'fixture-generator')
lib.vips_image_new_from_file.restype=C.c_void_p
lib.vips_image_set_string.argtypes=[C.c_void_p,C.c_char_p,C.c_char_p]
lib.vips_image_set_int.argtypes=[C.c_void_p,C.c_char_p,C.c_int]
lib.vips_image_set_blob_copy.argtypes=[C.c_void_p,C.c_char_p,C.c_void_p,C.c_size_t]
lib.vips_image_write_to_file.argtypes=[C.c_void_p,C.c_char_p];lib.vips_image_write_to_file.restype=C.c_int
lib.g_object_unref.argtypes=[C.c_void_p]
def v(*args): subprocess.run(['vips',*map(str,args)],check=True,stdout=subprocess.DEVNULL)
def annotated(src,out,strings={},ints={},xmp=None):
    image=lib.vips_image_new_from_file(C.c_char_p(str(src).encode()),C.c_void_p())
    if not image: raise RuntimeError('load failed')
    for k,val in strings.items(): lib.vips_image_set_string(image,k.encode(),str(val).encode())
    for k,val in ints.items(): lib.vips_image_set_int(image,k.encode(),val)
    if xmp: lib.vips_image_set_blob_copy(image,b'xmp-data',xmp,len(xmp))
    if lib.vips_image_write_to_file(image,str(out).encode(),C.c_void_p()): raise RuntimeError('save failed')
    lib.g_object_unref(image)
with tempfile.TemporaryDirectory() as tmp:
    t=pathlib.Path(tmp)
    # Original synthetic patches, kept away from clipping in all three gamuts.
    colors=[(115,82,68),(194,150,130),(98,122,157),(87,108,67),(133,128,177),(103,189,170),(214,126,44),(80,91,166),(193,90,99),(94,60,108),(157,188,64),(224,163,46),(56,61,150),(70,148,73),(175,54,60),(220,192,55),(187,86,149),(45,133,151),(240,240,240),(200,200,200),(160,160,160),(120,120,120),(80,80,80),(40,40,40)]
    svg='<svg xmlns="http://www.w3.org/2000/svg" width="384" height="256">'+''.join(f'<rect x="{i%6*64}" y="{i//6*64}" width="64" height="64" fill="rgb{c}"/>' for i,c in enumerate(colors))+'</svg>'
    (t/'chart.svg').write_text(svg)
    v('flatten',t/'chart.svg',t/'chart.v','--background=255')
    v('copy',t/'chart.v',str(dest/'chart-srgb.png')+'[profile='+str(dest/'icc/srgb.icc')+']')
    v('icc_transform',dest/'chart-srgb.png',dest/'chart-p3.png',dest/'icc/p3.icc', '--embedded')
    v('icc_transform',dest/'chart-srgb.png',str(dest/'chart-adobergb.jpg')+'[Q=100,subsample-mode=off]',dest/'icc/adobergb.icc','--embedded')
    v('icc_import',dest/'chart-srgb.png',t/'lab.v','--embedded','--pcs=lab')
    v('rawsave',t/'lab.v',t/'lab.raw')
    values=struct.unpack('<'+'f'*(384*256*3),(t/'lab.raw').read_bytes())
    labs=[list(values[((i//6*64+32)*384+i%6*64+32)*3:((i//6*64+32)*384+i%6*64+32)*3+3]) for i in range(24)]
    (dest/'chart.json').write_text(json.dumps({'width':384,'height':256,'patch_size':64,'rgb':colors,'lab_d50':labs},indent=2)+'\n')
    # Pixel-aligned monochrome arrow gives identical JPEG blocks under rotation.
    (t/'arrow.svg').write_text('<svg xmlns="http://www.w3.org/2000/svg" width="128" height="96"><rect width="128" height="96" fill="white"/><path d="M16 40H72V16H88V32H104V40H120V56H104V64H88V80H72V56H16Z" fill="black"/></svg>')
    v('flatten',t/'arrow.svg',t/'arrow.v','--background=255')
    v('colourspace',t/'arrow.v',t/'gray-arrow.v','b-w')
    transforms={1:[],2:[('flip','horizontal')],3:[('rot','d180')],4:[('flip','vertical')],5:[('rot','d90'),('flip','horizontal')],6:[('rot','d270')],7:[('rot','d90'),('flip','vertical')],8:[('rot','d90')]}
    for orient,ops in transforms.items():
        src=t/'gray-arrow.v'
        for j,(op,arg) in enumerate(ops):
            out=t/f'o-{orient}-{j}.v';v(op,src,out,arg);src=out
        annotated(src,str(dest/f'orientation-{orient}.jpg')+'[Q=100]',ints={'orientation':orient})
    # Exercise packed PNG sample depths through our existing libvips bindings.
    # Exact gray levels avoid palette quantization or dithering differences.
    for bits in [1,2,4]:
        levels=(1<<bits)-1
        source=t/f'{bits}bit.pgm'
        source.write_bytes(b'P5\n32 32\n255\n'+bytes(
            ((x//2+y//2)%(levels+1))*255//levels for y in range(32) for x in range(32)))
        annotated(source,str(dest/f'{bits}bit.png')+f'[bitdepth={bits}]')
    (t/'alpha.svg').write_text(svg.replace('fill="rgb(115, 82, 68)"','fill-opacity="0.25" fill="rgb(115, 82, 68)"'))
    v('copy',t/'alpha.svg',dest/'alpha.png')
    v('linear',dest/'chart-srgb.png',t/'16.v','257','0')
    v('cast',t/'16.v',dest/'16bit.png','ushort')
    v('cast',t/'16.v',str(dest/'16bit.tif')+'[compression=deflate]','ushort')
    v('colourspace',dest/'chart-srgb.png',dest/'gray.jpg','b-w')
    v('icc_transform',dest/'chart-srgb.png',dest/'cmyk.jpg',dest/'icc/cmyk.icc','--embedded')
    v('jpegsave',dest/'chart-srgb.png',dest/'progressive.jpg','--interlace','--Q=95')
    v('black',dest/'tiny-1x1.jpg','1','1','--bands=3')
    v('black',dest/'wide-8000x100.jpg','8000','100','--bands=3')
    # Smooth textured, synthetic landscape; no third-party photograph in the repo.
    (t/'scene.svg').write_text('<svg xmlns="http://www.w3.org/2000/svg" width="768" height="512"><defs><linearGradient id="sky" x2="0" y2="1"><stop stop-color="#6689bb"/><stop offset="1" stop-color="#eddbb3"/></linearGradient><filter id="grain"><feTurbulence baseFrequency=".012" numOctaves="3" seed="6"/><feColorMatrix type="saturate" values="0"/><feBlend in="SourceGraphic" mode="soft-light"/></filter></defs><rect width="768" height="512" fill="url(#sky)"/><path d="M0 370L190 120L410 390L560 170L768 380V512H0Z" fill="#59684e" filter="url(#grain)"/></svg>')
    v('jpegsave',t/'scene.svg',dest/'landscape.jpg','--Q=95')
    strings={'exif-ifd0-Make':'Yhotos','exif-ifd0-Model':'Fixture Camera','exif-ifd2-LensModel':'Synthetic 50mm','exif-ifd2-FocalLength':'50/1','exif-ifd2-FNumber':'28/10','exif-ifd2-ExposureTime':'1/125','exif-ifd2-ISOSpeedRatings':'200','exif-ifd2-DateTimeOriginal':'2026:01:02 03:04:05','exif-ifd3-GPSLatitudeRef':'N','exif-ifd3-GPSLatitude':'43/1 39/1 0/1','exif-ifd3-GPSLongitudeRef':'W','exif-ifd3-GPSLongitude':'79/1 23/1 0/1'}
    xmp=b'<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmp:Rating="4" dc:title="'+b'Long title '*40+b'"/></rdf:RDF></x:xmpmeta>'
    annotated(dest/'chart-srgb.png',str(dest/'exif-heavy.jpg')+'[Q=100]',strings=strings,xmp=xmp)
    for suffix in ['avif','jxl','heic']:
        v('copy',dest/'chart-srgb.png',str(dest/f'chart.{suffix}')+('[Q=95,effort=3]' if suffix!='jxl' else '[distance=0,effort=3]'))
    # NCLX-only Display P3: libvips' HEIF saver writes ICC but has no NCLX
    # option. Replace the generated ICC property, padding at the end of ipco
    # with a free box so property indices and absolute iloc offsets stay fixed.
    def nclx_only(path):
        data=bytearray(path.read_bytes())
        colr=data.index(b'colr')-4; ipco=data.index(b'ipco')-4
        size=struct.unpack_from('>I',data,colr)[0]
        end=ipco+struct.unpack_from('>I',data,ipco)[0]
        assert data[colr+8:colr+12]==b'rICC' and colr+size<=end
        profile=struct.pack('>I4s4sHHHB',19,b'colr',b'nclx',12,13,0,128)
        padding=struct.pack('>I4s',size-19,b'free')+bytes(size-27)
        data[colr:end]=profile+data[colr+size:end]+padding
        path.write_bytes(data)
    for compression,suffix in [('av1','avif'),('hevc','heic')]:
        out=dest/f'chart-p3-nclx.{suffix}'
        v('heifsave',dest/'chart-p3.png',out,'--lossless','--effort=0',
          '--subsample-mode=off',f'--compression={compression}')
        nclx_only(out)
    # Committed decoder inputs keep CI independent of HEVC encoders (Ubuntu
    # installs decoders only) and CLI bitdepth options across libvips versions.
    rgba=b''.join(struct.pack('=4H',16384,32768,49152,alpha)
                  for y in range(32) for alpha in [0,32768,65535] for x in range(32))
    (t/'rgba.raw').write_bytes(rgba)
    v('rawload',t/'rgba.raw',t/'rgba.v',96,32,4,'--format=ushort','--interpretation=rgb16')
    for bits in [10,12]:
        for compression,suffix in [('av1','avif'),('hevc','heic')]:
            v('heifsave',t/'rgba.v',dest/f'alpha-{bits}.{suffix}','--lossless',
              '--effort=0',f'--compression={compression}',f'--bitdepth={bits}')
    v('colourspace',dest/'chart-srgb.png',t/'float.v','scrgb')
    v('tiffsave',t/'float.v',dest/'float.tif','--compression=deflate')
    # libavif >=1.2 emits a real tmap derived image, two dimg references,
    # gain-map metadata and an AV1 gain image. No XMP/auxiliary text signatures.
    # Kept inline so the fixture and its complete offline generator travel together.
    (t/'gain.c').write_text(r'''
#include <avif/avif.h>
#include <stdio.h>
#include <string.h>
int main(int argc, char **argv) {
    if (argc != 2) return 1;
    avifImage *base = avifImageCreate(32, 32, 8, AVIF_PIXEL_FORMAT_YUV444);
    if (!base) return 1;
    base->colorPrimaries = AVIF_COLOR_PRIMARIES_BT709;
    base->transferCharacteristics = AVIF_TRANSFER_CHARACTERISTICS_SRGB;
    base->matrixCoefficients = AVIF_MATRIX_COEFFICIENTS_BT601;
    base->gainMap = avifGainMapCreate();
    if (!base->gainMap) return 1;
    avifGainMap *gain = base->gainMap;
    gain->image = avifImageCreate(16, 16, 8, AVIF_PIXEL_FORMAT_YUV400);
    if (!gain->image) return 1;
    gain->image->colorPrimaries = AVIF_COLOR_PRIMARIES_UNSPECIFIED;
    gain->image->transferCharacteristics = AVIF_TRANSFER_CHARACTERISTICS_UNSPECIFIED;
    gain->image->matrixCoefficients = AVIF_MATRIX_COEFFICIENTS_BT601;
    gain->baseHdrHeadroom = (avifUnsignedFraction){0, 1};
    gain->alternateHdrHeadroom = (avifUnsignedFraction){2, 1};
    gain->useBaseColorSpace = AVIF_TRUE;
    gain->altColorPrimaries = AVIF_COLOR_PRIMARIES_BT709;
    gain->altTransferCharacteristics = AVIF_TRANSFER_CHARACTERISTICS_LINEAR;
    gain->altMatrixCoefficients = AVIF_MATRIX_COEFFICIENTS_BT601;
    gain->altYUVRange = AVIF_RANGE_FULL;
    gain->altDepth = 10;
    gain->altPlaneCount = 3;
    for (int c = 0; c < 3; ++c) {
        gain->gainMapMin[c] = (avifSignedFraction){0, 1};
        gain->gainMapMax[c] = (avifSignedFraction){2, 1};
        gain->gainMapGamma[c] = (avifUnsignedFraction){1, 1};
        gain->baseOffset[c] = gain->alternateOffset[c] = (avifSignedFraction){0, 1};
    }
    if (avifImageAllocatePlanes(base, AVIF_PLANES_YUV) != AVIF_RESULT_OK ||
        avifImageAllocatePlanes(gain->image, AVIF_PLANES_YUV) != AVIF_RESULT_OK) return 1;
    for (int c = 0; c < 3; ++c)
        memset(base->yuvPlanes[c], c ? 128 : 160, base->yuvRowBytes[c] * base->height);
    memset(gain->image->yuvPlanes[0], 128, gain->image->yuvRowBytes[0] * gain->image->height);
    avifEncoder *encoder = avifEncoderCreate();
    if (!encoder) return 1;
    encoder->maxThreads = 1;
    encoder->speed = 8;
    avifRWData encoded = AVIF_DATA_EMPTY;
    if (avifEncoderWrite(encoder, base, &encoded) != AVIF_RESULT_OK) return 1;
    // Independently validate that the encoded gain image and metadata decode.
    avifDecoder *decoder = avifDecoderCreate();
    if (!decoder) return 1;
    decoder->imageContentToDecode = AVIF_IMAGE_CONTENT_ALL;
    if (avifDecoderReadMemory(decoder, base, encoded.data, encoded.size) != AVIF_RESULT_OK ||
        !base->gainMap || !base->gainMap->image) return 1;
    FILE *out = fopen(argv[1], "wb");
    if (!out || fwrite(encoded.data, 1, encoded.size, out) != encoded.size || fclose(out)) return 1;
    avifDecoderDestroy(decoder);
    avifRWDataFree(&encoded);
    avifEncoderDestroy(encoder);
    avifImageDestroy(base);
    return 0;
}
''')
    flags=shlex.split(subprocess.check_output(['pkg-config','--cflags','--libs','libavif'],text=True))
    subprocess.run(['cc',str(t/'gain.c'),'-o',str(t/'gain'),*flags],check=True)
    subprocess.run([str(t/'gain'),str(dest/'gain-map.avif')],check=True)
    assert not any(s in (dest/'gain-map.avif').read_bytes() for s in
                   [b'urn:iso:std:iso:ts:21496',b'hdrgm:',b'hdr-gain-map',b'HDRGainMap',b'hdrgainmap'])
    malicious=dest/'malicious';malicious.mkdir(exist_ok=True)
    jpeg=(dest/'progressive.jpg').read_bytes()
    (malicious/'truncated.jpg').write_bytes(jpeg[:len(jpeg)//2])
    (malicious/'polyglot-jpg-html.jpg').write_bytes(jpeg+b'<html><script>alert(1)</script></html>')
    (malicious/'zero.jpg').write_bytes(b'')
    (malicious/'wrong-ext.png').write_bytes(jpeg)
    png=bytearray((dest/'chart-srgb.png').read_bytes());png[16:24]=struct.pack('>II',60000,60000);png[29:33]=struct.pack('>I',zlib.crc32(png[12:29]));(malicious/'bomb-png.png').write_bytes(png[:1024])
    # Patch width and height in a CLI-generated TIFF, then discard its pixel payload.
    v('tiffsave',dest/'tiny-1x1.jpg',t/'tiny.tif','--compression=none')
    data=bytearray((t/'tiny.tif').read_bytes());endian='<' if data[:2]==b'II' else '>';offset=struct.unpack_from(endian+'I',data,4)[0];count=struct.unpack_from(endian+'H',data,offset)[0]
    for i in range(count):
        at=offset+2+i*12;tag=struct.unpack_from(endian+'H',data,at)[0]
        if tag in (256,257):
            struct.pack_into(endian+'H',data,at+2,4);struct.pack_into(endian+'I',data,at+8,60000)
    (malicious/'huge-dims-tiff.tif').write_bytes(data)
    for p in dest.rglob('*'):
        if p.is_file() and p.stat().st_size>200_000:raise RuntimeError(f'oversize fixture {p.name}')
    manifest={str(p.relative_to(dest)):hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(dest.rglob('*')) if p.is_file() and p.suffix!='.md'}
    print(json.dumps(manifest,indent=2))
PY

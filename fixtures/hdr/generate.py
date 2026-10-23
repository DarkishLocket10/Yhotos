#!/usr/bin/env python3
"""HDR inputs from committed fixtures only. No network or private metadata."""
import ctypes, ctypes.util, pathlib, shlex, struct, subprocess, tempfile, sys
ROOT=pathlib.Path(__file__).resolve().parents[2]
DEST=ROOT/'fixtures/hdr'
def run(*args): subprocess.run(list(map(str,args)),check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
def box(kind,data): return struct.pack('>I4s',len(data)+8,kind)+data
def u16(n): return struct.pack('>H',n)
def u32(n): return struct.pack('>I',n)
def boxes(data):
    result=[];at=0
    while at<len(data):
        n,kind=struct.unpack_from('>I4s',data,at);assert n>=8 and at+n<=len(data)
        result.append((kind,data[at+8:at+n]));at+=n
    return result
def one(data,kind): return next(payload for k,payload in boxes(data) if k==kind)
def compressed_jxl(data):
    # brob stores the original box type followed by its Brotli-compressed contents.
    # Only the offline generator encodes Brotli; the codec never decompresses it.
    lib=ctypes.CDLL(ctypes.util.find_library('brotlienc'))
    lib.BrotliEncoderMaxCompressedSize.argtypes=[ctypes.c_size_t]
    lib.BrotliEncoderMaxCompressedSize.restype=ctypes.c_size_t
    lib.BrotliEncoderCompress.argtypes=[ctypes.c_int,ctypes.c_int,ctypes.c_int,ctypes.c_size_t,ctypes.c_void_p,ctypes.POINTER(ctypes.c_size_t),ctypes.c_void_p]
    lib.BrotliEncoderCompress.restype=ctypes.c_int
    result=bytearray()
    for kind,payload in boxes(data):
        if kind==b'jhgm':
            size=ctypes.c_size_t(lib.BrotliEncoderMaxCompressedSize(len(payload)))
            encoded=ctypes.create_string_buffer(size.value)
            success=lib.BrotliEncoderCompress(6,22,0,len(payload),payload,ctypes.byref(size),encoded)
            assert success
            kind,payload=b'brob',b'jhgm'+encoded.raw[:size.value]
        result.extend(box(kind,payload))
    return result
def heic_image(path):
    data=path.read_bytes();meta=one(data,b'meta')[4:]
    # vips writes one hvc1 image and one EXIF item, with v0 iloc and one extent per item.
    loc=one(meta,b'iloc');assert loc[:4]==bytes(4)
    os,ls,bs=loc[4]>>4,loc[4]&15,loc[5]>>4
    count=int.from_bytes(loc[6:8],'big');at=8;locations={}
    for _ in range(count):
        item=int.from_bytes(loc[at:at+2],'big');at+=4
        base=int.from_bytes(loc[at:at+bs],'big');at+=bs
        n=int.from_bytes(loc[at:at+2],'big');at+=2;assert n==1
        off=int.from_bytes(loc[at:at+os],'big');at+=os
        size=int.from_bytes(loc[at:at+ls],'big');at+=ls
        locations[item]=data[base+off:base+off+size]
    primary=int.from_bytes(one(meta,b'pitm')[4:],'big')
    props=boxes(one(one(meta,b'iprp'),b'ipco'))
    return locations[primary],[(k,v) for k,v in props if k in [b'hvcC',b'av1C',b'ispe',b'pixi',b'colr',b'clap']]
def apple(base,gain,xmp):
    base,baseprops=heic_image(base);gain,gainprops=heic_image(gain)
    # Header + two documented float32 MakerNote values. Headroom is 3 stops.
    note=b'Apple iOS\0'+b'\0\1MM'+u16(2)
    note+=u16(33)+u16(11)+u32(1)+struct.pack('>f',1.0)
    note+=u16(48)+u16(11)+u32(1)+struct.pack('>f',0.0)+u32(0)
    # Standard EXIF TIFF: IFD0 -> ExifIFD -> bounded Apple MakerNote.
    tiff=b'MM\0*'+u32(8)+u16(1)+u16(34665)+u16(4)+u32(1)+u32(26)+u32(0)
    tiff+=u16(1)+u16(37500)+u16(7)+u32(len(note))+u32(44)+u32(0)+note
    exif=u32(0)+tiff
    ftyp=box(b'ftyp',b'heic'+u32(0)+b'heicmif1')
    properties=baseprops+gainprops+[(b'auxC',bytes(4)+b'urn:com:apple:photo:2020:aux:hdrgainmap\0')]
    ipco=box(b'ipco',b''.join(box(k,v) for k,v in properties))
    ipma=bytes(4)+u32(2)
    ipma+=u16(1)+bytes([len(baseprops)])+bytes(range(1,len(baseprops)+1))
    ipma+=u16(2)+bytes([len(gainprops)+1])+bytes(range(len(baseprops)+1,len(properties)+1))
    iprp=box(b'iprp',ipco+box(b'ipma',ipma))
    kinds=[(1,b'hvc1',b'base\0'),(2,b'hvc1',b'gain\0'),(3,b'Exif',b'exif\0'),(4,b'mime',b'xmp\0application/rdf+xml\0')]
    iinf=box(b'iinf',bytes(4)+u16(4)+b''.join(box(b'infe',b'\x02\0\0\0'+u16(i)+u16(0)+k+n) for i,k,n in kinds))
    iref=box(b'iref',bytes(4)+box(b'auxl',u16(2)+u16(1)+u16(1))+box(b'cdsc',u16(3)+u16(1)+u16(1))+box(b'cdsc',u16(4)+u16(1)+u16(2)))
    payloads=[base,gain,exif,xmp]
    def metadata(offset):
        iloc=bytes(4)+b'\x44\0'+u16(4)
        for i,payload in enumerate(payloads,1):
            iloc+=u16(i)+u16(0)+u16(1)+u32(offset)+u32(len(payload));offset+=len(payload)
        return box(b'meta',bytes(4)+box(b'hdlr',bytes(8)+b'pict'+bytes(12)+b'\0')+box(b'pitm',bytes(4)+u16(1))+iinf+box(b'iloc',iloc)+iref+iprp)
    meta=metadata(0);meta=metadata(len(ftyp)+len(meta)+8)
    return ftyp+meta+box(b'mdat',b''.join(payloads))
def adobe_only(data):
    # Remove ISO APP2s, preserving scan data. Rebuild the MPF offsets and lengths.
    starts=[];at=0
    while True:
        start=data.find(b'\xff\xd8',at)
        if start<0:break
        end=data.find(b'\xff\xd9',start)+2;assert end>1
        starts.append(data[start:end]);at=end
    assert len(starts)==2
    def strip(jpeg):
        out=bytearray(jpeg[:2]);at=2
        while at<len(jpeg):
            marker=jpeg[at+1]
            if marker==0xda: out.extend(jpeg[at:]);break
            n=int.from_bytes(jpeg[at+2:at+4],'big')+2
            segment=jpeg[at:at+n]
            if not ((marker==0xe2 and segment[4:].startswith(b'urn:iso:std:iso:ts:21496')) or (marker==0xe1 and segment[4:].startswith(b'http://ns.adobe.com/xap/1.0/'))):out.extend(segment)
            at+=n
        return out
    base,gain=map(strip,starts)
    def segment(xml):
        payload=b'http://ns.adobe.com/xap/1.0/\0'+xml.encode()
        return b'\xff\xe1'+u16(len(payload)+2)+payload
    def xml(body):
        return '<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:hdrgm="http://ns.adobe.com/hdr-gain-map/1.0/" hdrgm:Version="1.0" '+body+'</rdf:Description></rdf:RDF></x:xmpmeta>'
    secondary=segment(xml('hdrgm:GainMapMin="0" hdrgm:GainMapMax="2" hdrgm:Gamma="1" hdrgm:OffsetSDR="0" hdrgm:OffsetHDR="0" hdrgm:HDRCapacityMin="0" hdrgm:HDRCapacityMax="2" hdrgm:BaseRenditionIsHDR="False">'))
    prefix=4+int.from_bytes(gain[4:6],'big') if gain[2:4]==b'\xff\xe0' else 2
    gain[prefix:prefix]=secondary
    primary=segment(xml('xmlns:Container="http://ns.google.com/photos/1.0/container/" xmlns:Item="http://ns.google.com/photos/1.0/container/item/"><Container:Directory><rdf:Seq><rdf:li rdf:parseType="Resource"><Container:Item Item:Semantic="Primary" Item:Mime="image/jpeg"/></rdf:li><rdf:li rdf:parseType="Resource"><Container:Item Item:Semantic="GainMap" Item:Mime="image/jpeg" Item:Length="'+str(len(gain))+'"/></rdf:li></rdf:Seq></Container:Directory>'))
    prefix=4+int.from_bytes(base[4:6],'big') if base[2:4]==b'\xff\xe0' else 2
    base[prefix:prefix]=primary
    p=base.index(b'MPF\0')+4
    endian='<' if base[p:p+2]==b'II' else '>'
    directory=struct.unpack_from(endian+'I',base,p+4)[0]+p
    count=struct.unpack_from(endian+'H',base,directory)[0]
    for i in range(count):
        at=directory+2+12*i
        if struct.unpack_from(endian+'H',base,at)[0]==0xb002:entries=p+struct.unpack_from(endian+'I',base,at+8)[0]
    struct.pack_into(endian+'I',base,entries+4,len(base))
    struct.pack_into(endian+'I',base,entries+16+4,len(gain))
    struct.pack_into(endian+'I',base,entries+16+8,len(base)-p)
    return bytes(base+gain)
with tempfile.TemporaryDirectory() as tmp:
    tmp=pathlib.Path(tmp)
    flags=shlex.split(subprocess.check_output(['pkg-config','--cflags','--libs','libuhdr'],text=True))
    run('cc',DEST/'mux.c','-o',tmp/'mux',*flags)
    if sys.argv[1:] == ['--orientations']:
        import orientations
        orientations.generate(sys.modules[__name__], tmp)
        sys.exit(0)
    if len(sys.argv)==3 and sys.argv[1]=='--benchmark':
        # 24 MP working set from the owner's stripped, committed photographic fixture.
        out=pathlib.Path(sys.argv[2]).resolve()
        run('vips','resize',ROOT/'fixtures/images/photo.jpg',tmp/'up.v',3.75)
        run('vips','crop',tmp/'up.v',str(tmp/'base.jpg')+'[Q=95,subsample-mode=off,profile='+str(ROOT/'fixtures/images/icc/srgb.icc')+']',0,317,6000,4000)
        run('vips','black',tmp/'gain.v',1500,1000)
        run('vips','linear',tmp/'gain.v',str(tmp/'gain.jpg')+'[Q=100]',1,128)
        run(tmp/'mux',tmp/'base.jpg',tmp/'gain.jpg',out)
        sys.exit(0)
    run('vips','resize',ROOT/'fixtures/images/chart-srgb.png',tmp/'base.jpg',0.25)
    run('vips','black',tmp/'gray.v',48,32)
    run('vips','linear',tmp/'gray.v',tmp/'gain.jpg',1,128)
    run(tmp/'mux',tmp/'base.jpg',tmp/'gain.jpg',DEST/'iso.jpg')
    source=(DEST/'iso.jpg').read_bytes()
    assert b'urn:iso:std:iso:ts:21496' in source
    (DEST/'adobe.jpg').write_bytes(adobe_only(source))
    run('vips','resize',ROOT/'fixtures/images/chart-p3.png',tmp/'p3.jpg',0.25)
    run(tmp/'mux',tmp/'p3.jpg',tmp/'gain.jpg',DEST/'iso-p3.jpg')
    for name in ['base','gain']:
        run('vips','heifsave',tmp/f'{name}.jpg',tmp/f'{name}.heic','--compression=hevc','--lossless')
    for name,form in [('apple','attribute'),('apple-element','element')]:
        xmp=(DEST/f'apple-{form}.xmp').read_bytes()
        (DEST/f'{name}.heic').write_bytes(apple(tmp/'base.heic',tmp/'gain.heic',xmp))
    for name in ['base','gain']:
        run('vips','jxlsave',tmp/f'{name}.jpg',tmp/f'{name}.jxl','--distance=0','--effort=3','--keep=icc' if name=='base' else '--keep=none')
    base=(tmp/'base.jxl').read_bytes();gain=(tmp/'gain.jxl').read_bytes()
    if not gain.startswith(b'\xff\x0a'): gain=one(gain,b'jxlc')
    metadata=struct.pack('>HHBI',0,0,0x48,1)+struct.pack('>IIiiIii',0,2,0,2,1,0,0)
    bundle=bytes([0])+u16(len(metadata))+metadata+bytes(5)+gain
    if base.startswith(b'\xff\x0a'): base=box(b'JXL ',b'\r\n\x87\n')+box(b'ftyp',b'jxl '+u32(0)+b'jxl ')+box(b'jxlc',base)
    (DEST/'gain-map.jxl').write_bytes(base+box(b'jhgm',bundle))
    (DEST/'gain-map-brob.jxl').write_bytes(compressed_jxl(base+box(b'jhgm',bundle)))
    for name in ['iso.jpg','iso-p3.jpg','adobe.jpg','apple.heic','apple-element.heic','gain-map.jxl','gain-map-brob.jxl']:assert (DEST/name).stat().st_size<300000

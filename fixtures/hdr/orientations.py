"""Offline asymmetric HDR fixtures; invoked by generate.py --orientations."""
import shlex
import struct
import subprocess


def generate(hdr, tmp):
    dest = hdr.DEST / 'orientation'
    dest.mkdir(exist_ok=True)
    flags = shlex.split(subprocess.check_output(['pkg-config', '--cflags', '--libs', 'libjxl'], text=True))
    hdr.run('cc', hdr.DEST / 'orientation-jxl.c', '-o', tmp / 'jxl', *flags)
    # Four distinct quadrants detect every reflection, including the diagonals.
    for name, w, h in [('base', 96, 64), ('gain', 48, 32)]:
        pixels = bytes(v for y in range(h) for x in range(w)
                       for v in [[224, 48, 96, 160][(y >= h // 2) * 2 + (x >= w // 2)]] * 3)
        (tmp / f'{name}.raw').write_bytes(pixels)
        hdr.run('vips', 'rawload', tmp / f'{name}.raw', tmp / f'{name}.v', w, h, 3,
                '--format=uchar', '--interpretation=srgb')
        hdr.run('vips', 'jpegsave', tmp / f'{name}.v', tmp / f'{name}.jpg', '--Q=100',
                '--subsample-mode=off', '--keep=none',
                '--profile=' + str(hdr.ROOT / 'fixtures/images/icc/srgb.icc'))
        for compression, suffix in [('hevc', 'heic'), ('av1', 'avif')]:
            hdr.run('vips', 'heifsave', tmp / f'{name}.v', tmp / f'{name}.{suffix}',
                    f'--compression={compression}', '--lossless', '--subsample-mode=off', '--effort=0')

    def exif(orientation, apple=False):
        # IFD0 orientation and, for Apple, an ExifIFD with only documented MakerNotes.
        tiff = b'MM\0*' + hdr.u32(8) + hdr.u16(2 if apple else 1)
        tiff += hdr.u16(274) + hdr.u16(3) + hdr.u32(1) + hdr.u16(orientation) + bytes(2)
        if apple:
            note = b'Apple iOS\0' + b'\0\1MM' + hdr.u16(2)
            for key, value in [(33, 1.0), (48, 0.0)]:
                note += hdr.u16(key) + hdr.u16(11) + hdr.u32(1) + struct.pack('>f', value)
            note += hdr.u32(0)
            tiff += hdr.u16(34665) + hdr.u16(4) + hdr.u32(1) + hdr.u32(38) + hdr.u32(0)
            tiff += hdr.u16(1) + hdr.u16(37500) + hdr.u16(7) + hdr.u32(len(note)) + hdr.u32(56)
            tiff += hdr.u32(0) + note
        else:
            tiff += hdr.u32(0)
        return tiff

    def jpeg(name, orientation):
        payload = b'Exif\0\0' + exif(orientation)
        data = (tmp / f'{name}.jpg').read_bytes()
        return data[:2] + b'\xff\xe1' + hdr.u16(len(payload) + 2) + payload + data[2:]

    # HEIF transformation properties in application order. imir 0 flips vertically.
    transforms = [[], [(b'imir', b'\1')], [(b'irot', b'\2')], [(b'imir', b'\0')],
                  [(b'irot', b'\3'), (b'imir', b'\1')], [(b'irot', b'\3')],
                  [(b'irot', b'\3'), (b'imir', b'\0')], [(b'irot', b'\1')]]
    iso = struct.pack('>HHBI', 0, 0, 0x48, 1) + struct.pack('>IIiiIii', 0, 2, 0, 2, 1, 0, 0)

    def heif(suffix, orientation, aux_orientation, container):
        base, baseprops = hdr.heic_image(tmp / f'base.{suffix}')
        gain, gainprops = hdr.heic_image(tmp / f'gain.{suffix}')
        baseprops += transforms[orientation - 1] if container else []
        gainprops += transforms[aux_orientation - 1]  # Must be ignored by HDR extraction.
        apple = suffix == 'heic'
        extra = [(b'auxC', bytes(4) + b'urn:com:apple:photo:2020:aux:hdrgainmap\0')] if apple else []
        properties = baseprops + gainprops + extra
        ipco = hdr.box(b'ipco', b''.join(hdr.box(k, v) for k, v in properties))
        ipma = bytes(4) + hdr.u32(2)
        ipma += hdr.u16(1) + bytes([len(baseprops)]) + bytes(range(1, len(baseprops) + 1))
        ipma += hdr.u16(2) + bytes([len(gainprops) + len(extra)]) + bytes(range(len(baseprops) + 1, len(properties) + 1))
        iprp = hdr.box(b'iprp', ipco + hdr.box(b'ipma', ipma))
        kinds = [(1, b'hvc1' if apple else b'av01', b'base\0'),
                 (2, b'hvc1' if apple else b'av01', b'gain\0'), (3, b'Exif', b'exif\0'),
                 (4, b'mime' if apple else b'tmap', b'xmp\0application/rdf+xml\0' if apple else b'tmap\0')]
        iinf = hdr.box(b'iinf', bytes(4) + hdr.u16(4) + b''.join(
            hdr.box(b'infe', b'\2\0\0\0' + hdr.u16(i) + hdr.u16(0) + k + n) for i, k, n in kinds))
        refs = hdr.box(b'cdsc', hdr.u16(3) + hdr.u16(1) + hdr.u16(1))
        if apple:
            refs += hdr.box(b'auxl', hdr.u16(2) + hdr.u16(1) + hdr.u16(1))
            refs += hdr.box(b'cdsc', hdr.u16(4) + hdr.u16(1) + hdr.u16(2))
        else:
            refs += hdr.box(b'dimg', hdr.u16(4) + hdr.u16(2) + hdr.u16(1) + hdr.u16(2))
        payloads = [base, gain, hdr.u32(0) + exif(1 if container else orientation, apple),
                    (hdr.DEST / 'apple-attribute.xmp').read_bytes() if apple else b'\0' + iso]
        ftyp = hdr.box(b'ftyp', (b'heic' if apple else b'avif') + hdr.u32(0) + (b'heic' if apple else b'avif') + b'mif1')

        def metadata(offset):
            iloc = bytes(4) + b'\x44\0' + hdr.u16(4)
            for i, payload in enumerate(payloads, 1):
                iloc += hdr.u16(i) + hdr.u16(0) + hdr.u16(1) + hdr.u32(offset) + hdr.u32(len(payload))
                offset += len(payload)
            return hdr.box(b'meta', bytes(4) + hdr.box(b'hdlr', bytes(8) + b'pict' + bytes(12) + b'\0') +
                           hdr.box(b'pitm', bytes(4) + hdr.u16(1)) + iinf + hdr.box(b'iloc', iloc) +
                           hdr.box(b'iref', bytes(4) + refs) + iprp)
        meta = metadata(0)
        meta = metadata(len(ftyp) + len(meta) + 8)
        return ftyp + meta + hdr.box(b'mdat', b''.join(payloads))

    for orientation in range(1, 9):
        auxiliary = 9 - orientation
        for name, value in [('base', orientation), ('gain', auxiliary)]:
            (tmp / f'{name}-exif.jpg').write_bytes(jpeg(name, value))
            hdr.run(tmp / 'jxl', tmp / f'{name}.raw', tmp / f'{name}.jxl',
                    96 if name == 'base' else 48, 64 if name == 'base' else 32, value)
        hdr.run(tmp / 'mux', tmp / 'base-exif.jpg', tmp / 'gain-exif.jpg', tmp / 'iso.jpg')
        data = (tmp / 'iso.jpg').read_bytes()
        (dest / f'iso-{orientation}.jpg').write_bytes(data)
        (dest / f'adobe-{orientation}.jpg').write_bytes(hdr.adobe_only(data))
        bundle = b'\0' + hdr.u16(len(iso)) + iso + bytes(5) + (tmp / 'gain.jxl').read_bytes()
        base = (tmp / 'base.jxl').read_bytes()
        (dest / f'jxl-{orientation}.jxl').write_bytes(
            hdr.box(b'JXL ', b'\r\n\x87\n') + hdr.box(b'ftyp', b'jxl ' + hdr.u32(0) + b'jxl ') +
            hdr.box(b'jxlc', base) + hdr.box(b'jhgm', bundle))
        for suffix in ['heic', 'avif']:
            for container in [False, True]:
                name = 'container' if container else 'exif'
                (dest / f'{suffix}-{name}-{orientation}.{suffix}').write_bytes(
                    heif(suffix, orientation, auxiliary, container))
    assert all(p.stat().st_size < 300000 for p in dest.iterdir())

import sys, zlib, struct
data = open(sys.argv[1],'rb').read()
parts = data.split(b'\n', 3)
w,h = map(int, parts[1].split())
raw = parts[3]
def chunk(t, d):
    c = t + d
    return struct.pack('>I', len(d)) + c + struct.pack('>I', zlib.crc32(c) & 0xffffffff)
scan = b''
for y in range(h):
    scan += b'\x00' + raw[y*w*3:(y+1)*w*3]
png = b'\x89PNG\r\n\x1a\n'
png += chunk(b'IHDR', struct.pack('>IIBBBBB', w, h, 8, 2, 0, 0, 0))
png += chunk(b'IDAT', zlib.compress(bytes(scan)))
png += chunk(b'IEND', b'')
open(sys.argv[2],'wb').write(png)

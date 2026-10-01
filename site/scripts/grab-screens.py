"""Two Xvfb screens recorded at the same instants: raw BGRX video on
stdout, each frame the first screen stacked above the second.

    grab-screens.py FB_A FB_B SECONDS | ffmpeg -f rawvideo -pix_fmt bgr0 -s WxH*2 -framerate 60 -i - ...

FB_A and FB_B are the framebuffers Xvfb keeps in files (Xvfb -fbdir),
mapped here and read back to back, a millisecond or so apart, half a frame
after each beat of the grid clock-scene.py draws on (frame k at k/60 s of
the monotonic clock): the host's frame is then always whole. Grabbing the
two screens through X instead (two x11grab inputs) stamps each grab before
its server answers: a busy server answers late, and the client's half can
look newer than its host's.

When the encoder holds the pipe for longer than a frame, the picture just
read stands in for the frames missed, so the video keeps the wall clock's
pace; how many did is said on stderr."""

import math
import mmap
import struct
import sys
import time

FPS = 60


def framebuffer(path):
    """The pixels of an XWD file Xvfb writes: a big-endian header, the
    colour map, then rows in the server's byte order."""
    f = open(path, "rb")
    m = mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ)
    h = struct.unpack(">25I", m[:100])
    header, width, height, bpp, stride, ncolors = h[0], h[4], h[5], h[11], h[12], h[19]
    if bpp != 32 or stride != width * 4:
        sys.exit(f"{path}: {bpp} bits per pixel, {stride} bytes per row; expected 32 and {width * 4}")
    start = header + ncolors * 12
    return memoryview(m)[start : start + stride * height], width, height


a, w, h = framebuffer(sys.argv[1])
b, w2, h2 = framebuffer(sys.argv[2])
if (w, h) != (w2, h2):
    sys.exit(f"the screens differ: {w}x{h} and {w2}x{h2}")
total = round(float(sys.argv[3]) * FPS)
out = sys.stdout.buffer

beat = math.ceil(time.monotonic() * FPS)
n = stood_in = 0
while n < total:
    delay = (beat + 0.5) / FPS - time.monotonic()
    if delay > 0:
        time.sleep(delay)
    frame = bytes(a) + bytes(b)
    out.write(frame)
    n += 1
    beat += 1
    behind = min(math.floor(time.monotonic() * FPS - 0.5) - beat + 1, total - n)
    for _ in range(max(0, behind)):
        out.write(frame)
        n += 1
        beat += 1
        stood_in += 1
out.flush()
print(f"grab-screens: {n} frames, {stood_in} stood in for frames missed", file=sys.stderr)

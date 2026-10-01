"""The host's screen for the page's stream video: a running clock, a frame
counter, a strip that lights one cell per frame and a marker sweeping at
constant speed, on a plain dark screen. Recorded on both screens at once,
it lets the two halves be compared frame by frame, and shows a dropped or
repeated frame as a skipped or doubled cell. 60 fps, full screen on
whatever X display DISPLAY names.

Everything repeats every PERIOD seconds, the length of the loop
record-stream.sh cuts, so the loop's seam falls on matching frames.

Frames are drawn on a grid shared with grab-screens.py: frame k at k/60 s
of the monotonic clock, which grab-screens.py reads half a frame later. A
scene timed on its own drifts past the reader's beat, and where the two
meet a frame is read twice and the next one never."""

import math
import os
import sys
import time

os.environ.setdefault("SDL_VIDEO_X11_NET_WM_BYPASS_COMPOSITOR", "0")
import pygame

W, H = 1280, 720
FPS = 60
SECS = float(sys.argv[1]) if len(sys.argv) > 1 else 60.0
PERIOD = 23.0
# Ten sweeps per period: one every 2.3 s.
SWEEP = PERIOD / 10

BG = (10, 13, 20)
INK = (240, 243, 250)
INK_2 = (176, 188, 214)
ACCENT = (110, 155, 255)

FONTS = "/usr/share/fonts/truetype/dejavu"
pygame.init()
screen = pygame.display.set_mode((W, H), pygame.NOFRAME)
pygame.mouse.set_visible(False)
big = pygame.font.Font(f"{FONTS}/DejaVuSans-Bold.ttf", 140)
small = pygame.font.Font(f"{FONTS}/DejaVuSansMono.ttf", 26)


CELLS, CELL_W, GAP = FPS, 14, 4
strip_w = CELLS * CELL_W + (CELLS - 1) * GAP
strip_x = (W - strip_w) // 2
# The strip and the sweep, drawn translucent over the screen.
LAYER_Y, LAYER_H = 420, 160
layer = pygame.Surface((W, LAYER_H), pygame.SRCALPHA)

first = k = math.ceil(time.monotonic() * FPS)
while k - first < SECS * FPS:
    for e in pygame.event.get():
        if e.type == pygame.QUIT:
            sys.exit(0)
    delay = k / FPS - time.monotonic()
    if delay > 0:
        time.sleep(delay)
    frame = (k - first) % round(PERIOD * FPS)
    t = frame / FPS

    screen.fill(BG)
    stamp = big.render(f"{int(t):02d}.{int(t * 1000) % 1000:03d}", True, INK)
    screen.blit(stamp, ((W - stamp.get_width()) // 2, 170))
    label = small.render(f"frame {frame:04d}  ·  {FPS} fps", True, INK_2)
    screen.blit(label, ((W - label.get_width()) // 2, 340))

    # One cell per frame; the last few stay lit, fading, so a skipped
    # frame shows as a gap in the trail.
    layer.fill((0, 0, 0, 0))
    lit = frame % CELLS
    for i in range(CELLS):
        age = (lit - i) % CELLS
        c = (*ACCENT, int(255 * (1 - age / 6))) if age < 6 else (255, 255, 255, 30)
        pygame.draw.rect(layer, c, (strip_x + i * (CELL_W + GAP), 10, CELL_W, 30), border_radius=3)

    pygame.draw.rect(layer, (255, 255, 255, 50), (strip_x, 120, strip_w, 2))
    x = strip_x + (t % SWEEP) / SWEEP * (strip_w - 6)
    pygame.draw.rect(layer, (*INK, 255), (int(x), 96, 6, 50), border_radius=3)
    screen.blit(layer, (0, LAYER_Y))
    pygame.display.flip()
    # After a stall, on to the frame now due rather than every one missed.
    k = max(k + 1, math.ceil(time.monotonic() * FPS))

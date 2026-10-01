// Every figure the page quotes, with where it comes from. Change them here
// when the documents they come from change.

/**
 * The recording on the page (public/media/stream-split.mp4), made by
 * scripts/record-stream.sh: a Linux host and client in one container
 * (tools/linux, release builds), the host's X screen showing a running
 * clock (scripts/clock-scene.py), 1280x720 at 60 fps, software H.264, over
 * loopback. Averages of Ping's per-second statistics over 32 s of that same
 * run, the best of four (all four passed; this one had the lowest and
 * steadiest delay); network is 0.0 ms in every second (the statistics give
 * a tenth).
 * Read half a frame after the host draws each one, the client's half shows
 * the frame before in every frame of the clip.
 */
export const RECORDING = {
  mode: '1280×720 at 60 fps, H.264',
  framesLost: 0,
  hostMs: 2.63,
  networkMs: 0,
  decodeMs: 0.46,
  renderMs: 1.84,
  captureToScreenMs: 5.09,
};

/**
 * docs/benchmarks.md, "Against Moonlight + Apollo": 3024x1890 at 120 fps,
 * 100 Mbit/s, HEVC, 7.1 audio; a 14" MacBook Pro (M4 Pro) on Wi-Fi, a
 * Windows 11 host with an RTX 3070 Ti and SudoVDA.
 */
export const VERSUS = [
  { label: 'Frames shown', unit: 'fps', ours: '115.6', theirs: '110.0', better: 'higher' },
  { label: 'Host processing', unit: 'ms', ours: '5.0', theirs: '5.6', better: 'lower' },
  { label: 'Decode', unit: 'ms', ours: '1.6–2.0', theirs: '2.32', better: 'lower' },
] as const;

/** docs/benchmarks.md, pq-boringtun on an Apple M4 Pro (Criterion). */
export const TUNNEL = {
  encrypt1200: '0.63 µs',
};

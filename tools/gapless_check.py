"""Gapless check for `music-cli decode` output.

usage: python -I tools/gapless_check.py <decode-dir> <videoId>...

Needs ff_<id>.f32 next to each track: ffmpeg's decode of the same file, e.g.
    ffmpeg -c:a libopus -i <id>.webm -f f32le -ac 2 ff_<id>.f32

Pass criteria (what the pipeline controls):
  1. Every track's frame count equals ffmpeg's: pre-skip and end padding trimmed exactly.
  2. gapless.f32 (tracks played back to back) matches ffmpeg's decodes concatenated, so
     nothing is added or lost at the joins.
Also reported: near-silence at each track's edges. That is in YouTube's files themselves
(some album tracks end in ~100 ms of silence) and no player can remove it without guessing.
"""
import array, math, os, sys

SILENCE = 10 ** (-50 / 20)  # -50 dBFS


def load(p):
    a = array.array('f')
    with open(p, 'rb') as f:
        a.frombytes(f.read())
    return a


def silent_ms(a, from_end):
    """Leading (or trailing) run of 1 ms slices below -50 dBFS RMS, in ms."""
    n = len(a) // 2
    ms = 0
    while ms < 2000:
        s = (n - (ms + 1) * 48) * 2 if from_end else ms * 48 * 2
        if s < 0 or s + 96 > len(a):
            break
        rms = math.sqrt(sum(v * v for v in a[s:s + 96]) / 96)
        if rms > SILENCE:
            break
        ms += 1
    return ms


d, ids = sys.argv[1], sys.argv[2:]
ok = True
ref = array.array('f')
for i in ids:
    ours = os.path.getsize(f"{d}/{i}.f32") // 8
    r = load(f"{d}/ff_{i}.f32")
    good = ours == len(r) // 2
    ok &= good
    print(f"{i}: {ours} frames, ffmpeg {len(r) // 2} -> {'OK' if good else 'MISMATCH'}; "
          f"source silence: start {silent_ms(r, False)} ms, end {silent_ms(r, True)} ms")
    ref.extend(r)

g = load(f"{d}/gapless.f32")
if len(g) != len(ref):
    ok = False
    print(f"gapless length {len(g) // 2} != {len(ref) // 2}")
else:
    sig = noise = worst = 0.0
    for k in range(0, len(g), 3):
        e = g[k] - ref[k]
        sig += ref[k] * ref[k]
        noise += e * e
        worst = max(worst, abs(e))
    snr = 10 * math.log10(sig / max(noise, 1e-30))
    good = snr >= 60
    ok &= good
    print(f"back-to-back vs ffmpeg: SNR {snr:.1f} dB, max diff {worst:.2e} -> {'OK' if good else 'FAIL'}")
print("PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)

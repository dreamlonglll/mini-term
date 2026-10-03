#!/usr/bin/env python3
"""宣传片配乐:纯程序合成的氛围铺底(无采样、无版权素材)。

用法:music.py <out.wav> [时长秒]
- 和弦铺底:Am9 → Fmaj7 → Cmaj9 → G6,每个和弦两小节(84 BPM),三把轻微失谐的锯齿波
  过一阶低通,慢起慢收;
- 拨弦琶音:三角波八分音符,第 3 小节进场、片尾前淡出;
- 两处「叮」:片头 logo 与片尾 logo 出现时的一声钟琴;
- 混响:指数衰减噪声做卷积脉冲;最后整体压到约 -20 dBFS RMS、峰值 -1 dBFS,首尾淡入淡出。
"""
import sys
import wave

import numpy as np

OUT = sys.argv[1]
DUR = float(sys.argv[2]) if len(sys.argv) > 2 else 74.4
SR = 44100
BPM = 84.0
BEAT = 60.0 / BPM
BAR = 4 * BEAT
N = int(DUR * SR)
t_all = np.arange(N) / SR
rng = np.random.default_rng(7)


def midi(m):
    return 440.0 * 2 ** ((m - 69) / 12)


# 和弦(MIDI 音高),每个两小节循环
CHORDS = [
    [45, 57, 60, 64, 67, 71],   # Am9:A2 A3 C4 E4 G4 B4
    [41, 57, 60, 64, 65, 69],   # Fmaj7:F2 A3 C4 E4 F4 A4
    [48, 55, 59, 62, 64, 67],   # Cmaj9:C3 G3 B3 D4 E4 G4
    [43, 55, 59, 62, 64, 71],   # G6:G2 G3 B3 D4 E4 B4
]


def one_pole_lowpass(x, cutoff):
    # cutoff 可以是逐采样数组(慢 LFO)
    cutoff = np.broadcast_to(cutoff, x.shape)
    a = np.exp(-2 * np.pi * cutoff / SR)
    y = np.empty_like(x)
    acc = 0.0
    for i in range(len(x)):
        acc = (1 - a[i]) * x[i] + a[i] * acc
        y[i] = acc
    return y


def saw(freq, t, phase=0.0):
    return 2.0 * ((freq * t + phase) % 1.0) - 1.0


def envelope(n, attack, release):
    env = np.ones(n)
    a = min(n, int(attack * SR))
    r = min(n, int(release * SR))
    env[:a] = np.linspace(0, 1, a) ** 2
    env[n - r:] *= np.linspace(1, 0, r) ** 2
    return env


def pad():
    out = np.zeros((N, 2))
    seg = 2 * BAR
    k = 0
    start = 0.0
    while start < DUR:
        notes = CHORDS[k % len(CHORDS)]
        s0 = int(start * SR)
        length = int((seg + 1.6) * SR)          # 尾巴叠进下一个和弦
        s1 = min(N, s0 + length)
        n = s1 - s0
        tt = np.arange(n) / SR
        env = envelope(n, 0.9, 1.8)
        for ch, detune in enumerate((-1, 1)):
            voice = np.zeros(n)
            for m in notes:
                f = midi(m)
                amp = 0.55 if m < 50 else 0.32
                for d in (-7, 0, 7):
                    ff = f * 2 ** ((d + detune * 3) / 1200)
                    voice += amp * saw(ff, tt, rng.random()) / 3
            # 根音补一个正弦低音
            voice += 0.35 * np.sin(2 * np.pi * midi(notes[0]) * tt)
            out[s0:s1, ch] += voice * env
        start += seg
        k += 1
    # 慢 LFO 扫低通,铺底有呼吸感
    lfo = 1300 + 500 * np.sin(2 * np.pi * t_all / 9.0)
    for ch in range(2):
        out[:, ch] = one_pole_lowpass(one_pole_lowpass(out[:, ch], lfo), 2600)
    return out * 0.20


def pluck(f, n):
    tt = np.arange(n) / SR
    tri = 2 * np.abs(2 * ((f * tt) % 1.0) - 1) - 1
    return tri * np.exp(-tt * 5.5) * (1 - np.exp(-tt * 400))


def arp():
    out = np.zeros((N, 2))
    step = BEAT / 2
    i = 0
    tpos = 2 * BAR
    while tpos < DUR - 3.0:
        chord = CHORDS[int(tpos // (2 * BAR)) % len(CHORDS)]
        tones = sorted(set(m + 12 for m in chord[1:]))
        pattern = [0, 2, 1, 3, 2, 4, 3, 1]
        m = tones[pattern[i % len(pattern)] % len(tones)]
        n = int(0.9 * SR)
        s0 = int(tpos * SR)
        s1 = min(N, s0 + n)
        p = pluck(midi(m), s1 - s0)
        vel = 0.55 + 0.25 * (i % 2 == 0)
        pan = 0.5 + 0.35 * np.sin(i * 0.9)
        out[s0:s1, 0] += p * vel * (1 - pan)
        out[s0:s1, 1] += p * vel * pan
        tpos += step
        i += 1
    # 进场与收尾
    gain = np.clip((t_all - 2 * BAR) / (2 * BAR), 0, 1) * np.clip((DUR - 3.0 - t_all) / 4.0, 0, 1)
    return out * gain[:, None] * 0.11


def bell(at, notes):
    out = np.zeros((N, 2))
    s0 = int(at * SR)
    n = min(N - s0, int(4.0 * SR))
    tt = np.arange(n) / SR
    for j, m in enumerate(notes):
        f = midi(m)
        tone = (np.sin(2 * np.pi * f * tt) + 0.4 * np.sin(2 * np.pi * f * 2.76 * tt) * np.exp(-tt * 3)) * np.exp(-tt * 1.6)
        tone *= 1 - np.exp(-tt * 300)
        out[s0:s0 + n, 0] += tone * (0.6 - 0.1 * j)
        out[s0:s0 + n, 1] += tone * (0.5 + 0.1 * j)
    return out * 0.10


def reverb(x, seconds=2.6, wet=0.32):
    n = int(seconds * SR)
    tt = np.arange(n) / SR
    out = np.zeros_like(x)
    for ch in range(2):
        ir = rng.standard_normal(n) * np.exp(-tt * 3.2)
        ir = one_pole_lowpass(ir, 4000)
        ir /= np.sqrt(np.sum(ir ** 2))
        size = 1 << int(np.ceil(np.log2(len(x) + n)))
        y = np.fft.irfft(np.fft.rfft(x[:, ch], size) * np.fft.rfft(ir, size), size)[:len(x)]
        out[:, ch] = (1 - wet) * x[:, ch] + wet * y
    return out


def main():
    mix = pad() + arp() + bell(0.25, [81, 88, 93]) + bell(69.55, [76, 81, 88])
    mix = reverb(mix)
    # 首尾淡入淡出
    fade = np.clip(t_all / 1.6, 0, 1) * np.clip((DUR - t_all) / 3.2, 0, 1)
    mix *= fade[:, None]
    # 响度:RMS 压到约 -20 dBFS,再保峰值 ≤ -1 dBFS
    rms = np.sqrt(np.mean(mix ** 2))
    mix *= 10 ** (-20 / 20) / max(rms, 1e-9)
    peak = np.max(np.abs(mix))
    if peak > 10 ** (-1 / 20):
        mix *= 10 ** (-1 / 20) / peak
    pcm = (np.clip(mix, -1, 1) * 32767).astype('<i2')
    with wave.open(OUT, 'wb') as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(SR)
        w.writeframes(pcm.tobytes())
    print(f'wrote {OUT}: {DUR:.1f}s, rms={20*np.log10(np.sqrt(np.mean(mix**2))):.1f} dBFS, peak={20*np.log10(np.max(np.abs(mix))):.1f} dBFS')


if __name__ == '__main__':
    main()

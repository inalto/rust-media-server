# OBS setup

rms accepts RTMP from OBS Studio, ffmpeg or any RTMP encoder. Every live stream has its own **secret stream
key**; anyone who knows it can broadcast on that live stream (regenerate it from the dashboard if it leaks).

![OBS settings panel of a live stream](../screenshots/broadcast-obs.png)

## Stream settings

*Settings → Stream*:

| Field | Value |
|---|---|
| Service | **Custom…** |
| Server | `rtmp://<host>:1935/live` (copy it from the *OBS settings* panel; behind a proxy use the public host name) |
| Stream key | the live stream's key (eye button to reveal it, copy button) |

The application name (`live`) is only cosmetic: the server accepts any name as long as the key is correct.

## Output settings (low latency)

*Settings → Output → Output mode: **Advanced***, tab *Streaming*. The simple mode does not expose the
keyframe interval.

| Setting | x264 (CPU) | NVENC (NVIDIA) / QSV |
|---|---|---|
| Rate control | CBR | CBR |
| Bitrate | 2500–6000 kbps, with headroom on your upload | same |
| **Keyframe interval** | **2 s** (1 s for minimum latency) | **2 s** |
| Preset | *veryfast* or faster | P4 or similar |
| Profile | main or high | main or high |
| Tune / Tuning | **zerolatency** | **Low latency** or **Ultra low latency** |
| B-frames | `bframes=0` (implied by zerolatency) | **Max B-frames = 0** |
| Other | — | Multipass **off**, look-ahead off |

Audio: AAC 128–160 kbps, 48 kHz. Video: constant frame rate (30 or 60 fps), 720p or 1080p.
*Settings → Advanced → Stream Delay*: disabled.

Why these values:

- **Keyframe interval.** OBS's default is `0` (automatic), which lets x264 use GOPs of up to 250 frames.
  rms cuts LL-HLS/LL-DASH segments at keyframes and new viewers can only start at a keyframe, so long GOPs
  mean high latency and slow starts. The *OBS settings* panel of a live stream warns when the measured
  keyframe interval of the incoming stream is longer than 4 s.
- **No B-frames.** Required for WebRTC: H.264 is passed through without transcoding and browsers' WebRTC
  decoders do not reorder B-frames. x264 *zerolatency* already disables them; set *B-frames = 0* for NVENC/QSV.
- **H.264 + AAC.** LL-HLS and LL-DASH package H.264 + AAC only. HEVC/AV1 over Enhanced RTMP are accepted but
  play only over HTTP-FLV/WS-FLV (and only in browsers that decode them).

## Check the keyframe interval

```bash
ffprobe -v error -select_streams v:0 -read_intervals "%+10" -show_entries packet=pts_time,flags \
        -of csv=p=0 https://stream.example.com/live/<name>.flv | grep K
```

The keyframe times must be 2.0 s apart. The measured value is also in the API:
`GET /api/v1/broadcasts/<name>` → `live.packager.measured_gop_secs`.

## Publishing with ffmpeg

`scripts/test-publish.sh <stream_key> [host[:port]] [app]` publishes a 720p30 test pattern with the
recommended settings. An existing file, looped:

```bash
ffmpeg -re -stream_loop -1 -i input.mp4 \
  -c:v libx264 -preset veryfast -tune zerolatency -g 60 -keyint_min 60 -sc_threshold 0 \
  -b:v 2500k -maxrate 2500k -bufsize 5000k -pix_fmt yuv420p \
  -c:a aac -b:a 128k -ar 48000 -f flv rtmp://<host>:1935/live/<stream_key>
```

## Going live

1. Start streaming in OBS: the live stream turns **Live** in the dashboard with codec, resolution, frame rate,
   incoming bitrate and viewers.
2. Viewers watching the embed switch from the waiting video to the live stream by themselves.
3. *Pause* keeps OBS connected but shows the waiting video to viewers; *Disconnect encoder* kicks the
   publisher; *Record now* / automatic recording: [Recording and waiting video](Recording-and-Waiting-Video.md).

If the same key publishes again (e.g. OBS reconnecting) while the previous connection is idle for more than
5 s, the new connection replaces the old one. See [Troubleshooting](Troubleshooting.md#obs) for connection
problems. Background on protocols and latency: [docs/PROTOCOLS.md](../PROTOCOLS.md) (Italian).

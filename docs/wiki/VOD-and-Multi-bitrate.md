# VOD and multi-bitrate

Besides live streams, rms publishes **video on demand** with adaptive bitrate (ABR): every video is encoded
by ffmpeg **in a single pass** into several qualities (a *ladder*) and packaged as CMAF (fMP4) segments that
are served both as **HLS** and **DASH**. The player picks the quality from the bandwidth and the player size;
viewers can pin one from the quality menu. Requires `ffmpeg` and `ffprobe` on the server.

![VOD library](../screenshots/vod-library.png)

## Adding videos

Three ways, all ending in the same encoding queue:

1. **Dashboard** — *Video on demand → Upload videos*: drop one or more files (MP4, MOV, M4V, MKV, WebM, AVI,
   MTS/M2TS, TS, MXF, FLV, MPG/MPEG, WMV; up to `server.max_upload_mb` each), choose the ladder and follow
   upload and encoding progress live.
2. **Inbox folder** — copy files into `data/vod/inbox/` (or `vod.inbox_dir`).
3. **Storage API** — users upload into their buckets and videos are encoded automatically
   ([Users, API keys and buckets](Users-API-Keys-and-Buckets.md)).

![Upload and encoding progress](../screenshots/vod-upload.png)

Admin API:

```bash
H="Authorization: Bearer $TOKEN"; B=http://127.0.0.1:8080/api/v1
curl -s -H "$H" -F ladder=standard -F "title=Keynote 2026" -F "description=Full keynote" \
     -F file=@keynote.mp4 $B/vod                                        # 201, status "queued"
curl -s -H "$H" $B/vod                                                  # library with status and progress
curl -s -H "$H" -X POST -H 'Content-Type: application/json' -d '{"ladder":"mobile"}' \
     $B/vod/<id>/retranscode                                            # re-encode (also retries a failed item)
curl -s -H "$H" -o original.mp4 $B/vod/<id>/source                      # the original
curl -s -H "$H" -X DELETE "$B/vod/<id>?force=1"                         # delete (force cancels a running job)
curl -s -H "$H" $B/vod/ladders                                          # configured ladders and limits
```

Send `ladder` before `file` in the multipart form: an unknown ladder is then rejected before the upload.

### Inbox folder

- A file is imported once the copy is finished: its size and modification time must stay the same for two
  scans (`vod.scan_interval_secs`, 10 s). Names starting with `.` and partial downloads (`.part`, `.tmp`,
  `.crdownload`, `.filepart`) are ignored.
- The title comes from the file name (`My_video.mp4` → "My video").
- **Choose the ladder with a sub-folder**: `inbox/mobile/clip.mp4` uses the `mobile` ladder; sub-folders for
  every configured ladder are created at startup. Files in the root use `vod.default_ladder`.
- Files that cannot be imported are moved to `inbox/_failed/` with a `<name>.error.txt` explaining why.
- Use rsync/scp/SMB into the folder; with several nodes only one scans a given folder at a time.

## Ladders

A ladder is a list of renditions, largest first. Defaults:

| Ladder | Renditions |
|---|---|
| `standard` (default) | 1080p 5 Mbit/s · 720p 2.8 Mbit/s · 480p 1.4 Mbit/s · 360p 800 kbit/s (AAC 160/128/96/96 kbit/s) |
| `mobile` | 720p 2.2 Mbit/s · 360p 700 kbit/s (AAC 128/64 kbit/s) |
| `single` | one rendition at the source resolution, constant quality (CRF 23), AAC 128 kbit/s |

Defining `[[vod.ladders]]` in `config.toml` replaces the built-in list:

```toml
[vod]
default_ladder = "web"

[[vod.ladders]]
name = "web"
description = "1080p, 720p and 360p"
renditions = [
  { name = "1080p", height = 1080, video_kbps = 4500, audio_kbps = 128 },
  { name = "720p",  height = 720,  video_kbps = 2500, audio_kbps = 128 },
  { name = "360p",  height = 360,  video_kbps = 700,  audio_kbps = 96 },
]
```

Rules:

- `height` is the **short side** of the output: a portrait 1080×1920 video becomes "1080p" at 1080×1920.
  `height = 0` keeps the source size; `video_kbps = 0` means constant quality (CRF 23 or the encoder's
  equivalent).
- Renditions larger than the source are skipped (never upscale); at least one is always produced.
- Keyframes every `vod.segment_duration` seconds (4 s), so every segment starts with a keyframe.
- H.264 High profile from 720p up, Main below; AAC 48 kHz stereo (mono stays mono), one audio track per
  distinct audio bitrate.
- The ladders are shown read-only in *Settings → VOD ladders*.

Measured on 8 cores with libx264 *veryfast*: a 30 s 1080p30 video → `standard` ladder in 16.8 s (≈ 1.8× real
time).

## Hardware encoders

Set `vod.encoder` to an ffmpeg H.264 hardware encoder (the ffmpeg build must support it):

| GPU | `encoder` | `preset` | `hwaccel_args` (optional) | Notes |
|---|---|---|---|---|
| NVIDIA | `h264_nvenc` | `p4` | `-hwaccel cuda` | constant quality uses `-cq` |
| Intel Quick Sync | `h264_qsv` | `veryfast` | | `nv12` pixel format |
| Intel/AMD on Linux | `h264_vaapi` | `""` | `-vaapi_device /dev/dri/renderD128` | frames uploaded with `format=nv12,hwupload` |

`encoder_args` adds output-side options (e.g. `-rc vbr -multipass qres` for NVENC, `-tune film` for x264).
`max_parallel_jobs` controls how many videos are encoded at once (each job encodes all its renditions).
Hardware encoders are wired but were not tested on real GPUs by the project: check the first encodings.

## Playing and sharing

| What | URL |
|---|---|
| HLS master | `http://<host>:8080/vod/<id>/hls/master.m3u8` |
| DASH MPD | `http://<host>:8080/vod/<id>/dash/manifest.mpd` |
| Poster | `http://<host>:8080/vod/<id>/poster.jpg` (frame at 10 % of the duration) |
| Embed player | `http://<host>:8080/embed/vod/<id>` |
| Watch page | `http://<host>:8080/watch/<id>` (title, description, link to the catalogue) |
| Public catalogue | `http://<host>:8080/vod` (every ready public video) |

![VOD player with the quality menu](../screenshots/player-vod-quality.png)

- *Links and embed* on a video gives the `<iframe>` with options (autoplay, sound, controls, language, theme,
  start time) and examples for hls.js and dash.js. Player parameters: [Playback and embedding](Playback-and-Embedding.md#vod-player-parameters).
- Quality menu: *Auto* plus every rendition with its bitrate; native HLS (Safari/iOS) has no quality API and
  hides the menu.
- Segments are immutable and cached for a year; playlists, MPD and poster for 60 s: put a CDN in front if
  needed.
- *plays* counts started viewing sessions, *viewers now* the active ones.

![Public catalogue](../screenshots/catalog.png)

```html
<iframe src="https://stream.example.com/embed/vod/4fb48570-aaa4-45ed-8bd6-46f2e62f794d?t=30"
        width="960" height="540" frameborder="0"
        allow="autoplay; fullscreen; picture-in-picture" allowfullscreen></iframe>
```

## Limits

H.264 output only; no thumbnail track on the seek bar, no subtitles, no multi-language audio; the admin
preview counts as a play. Admin-library videos are always public; private videos with signed URLs live in
users' buckets. Details: [docs/API.md](../API.md) §4.7, §6.6, §7 and [docs/ARCHITECTURE.md](../ARCHITECTURE.md) §19.

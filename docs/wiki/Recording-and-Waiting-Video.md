# Recording and waiting video

## Recording

A recording is the live stream written to disk by the server, exactly as received (no transcoding).

![Recordings of a live stream](../screenshots/broadcast-recordings.png)

- **Automatic**: tick *Record automatically* when creating or editing a live stream (`"record": true`); every
  live period is recorded from the moment the encoder connects. A manual stop is not undone until the next
  live period.
- **Manual**: *Record now* / *Stop recording* in the *Status* panel while the stream is live.
- Files: `data/recordings/<name>/<YYYYMMDD-HHMMSS>.flv` (UTC). The FLV is written in pure Rust while live; when
  the recording stops and ffmpeg is available it is remuxed to MP4 (`-c copy -movflags +faststart`, no quality
  loss) and the FLV is deleted (`recording.keep_flv = true` keeps it). Without ffmpeg the FLV stays.
- The *Recordings* panel lists status (*Recording*, *Processing*, *Ready*, *Failed*), start, duration, size and
  format, with play, download and delete buttons. Deleting a live stream keeps its recordings.
- After a crash the server repairs unfinished recordings at the next start (FLV truncated to its last
  complete tag, then remuxed).

API:

```bash
H="Authorization: Bearer $TOKEN"; B=http://127.0.0.1:8080/api/v1
curl -s -H "$H" -X POST $B/broadcasts/<name>/record/start          # 200 Recording {status: "recording"}
curl -s -H "$H" -X POST $B/broadcasts/<name>/record/stop           # 200, "processing" while remuxing
curl -s -H "$H" $B/broadcasts/<name>/recordings                    # newest first
curl -s -H "$H" -o rec.mp4 "$B/recordings/<id>/download?download=1"
curl -s -H "$H" -X DELETE $B/recordings/<id>
```

In cluster mode recordings are written by the node that receives the encoder; downloads work from any node.

## Waiting video

While a live stream is **offline** or **paused**, the player loops a waiting video (muted) or shows a waiting
text, and switches to the live stream by itself as soon as it arrives.

![Waiting video panel of a live stream](../screenshots/broadcast-waiting-video.png)

Resolution order for each live stream:

1. its own waiting video (*Waiting video* panel of the live stream);
2. the global default video (*Settings → Default waiting video*);
3. the waiting text: its own, else the global default text, else the built-in text in the player's language
   ("The live stream will start shortly").

Uploads accept MP4, MOV, M4V, WebM, MKV and AVI up to `server.max_upload_mb` (1024 MiB by default). With
ffmpeg every upload is normalised to H.264/AAC MP4 (at most 1920 px wide, `+faststart`) and a poster frame
is extracted; without ffmpeg only MP4 files are accepted as they are.

```bash
curl -s -H "$H" -F file=@starting-soon.mp4 $B/broadcasts/<name>/waiting-video    # this live stream
curl -s -H "$H" -X DELETE $B/broadcasts/<name>/waiting-video
curl -s -H "$H" -F file=@stay-tuned.mp4 $B/settings/waiting-video                # global default
curl -s -H "$H" -H 'Content-Type: application/json' -X PATCH \
     -d '{"waiting_text":"We will be live at 10:00"}' $B/broadcasts/<name>
curl -s -H "$H" -H 'Content-Type: application/json' -X PATCH \
     -d '{"default_waiting_text":"Stay tuned"}' $B/settings
```

![The player showing the waiting video](../screenshots/player-waiting.png)

## Pause

*Pause* (dashboard or `POST /api/v1/broadcasts/<name>/pause`) keeps the encoder connected, keeps recording and
keeps every playback endpoint running, but sets the public status to `paused`: embed players switch to the
waiting video with a *paused* badge. *Resume* (`/resume`) brings viewers back to the live picture. Use it for
breaks without stopping OBS.

To end a broadcast from the server side use *Disconnect encoder* (`POST /api/v1/broadcasts/<name>/disconnect`);
regenerate the stream key if the encoder must not reconnect.

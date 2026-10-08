# rust-media-server (rms)

Media server per dirette a bassa latenza scritto in Rust, in un unico binario. Ispirato ad
Ant Media Server e a `xiu`: OBS pubblica in RTMP, il server ridistribuisce ai browser in
HTTP-FLV / WebSocket-FLV, LL-HLS e LL-DASH (CMAF), registra su richiesta, raccoglie le
metriche degli spettatori simultanei e offre una dashboard di gestione (axum) con embed
pronti da incollare e un video d'attesa quando la diretta non c'è.

- Ingest: **RTMP** (stream key segreta per ogni diretta; Enhanced RTMP accettato per HEVC/AV1 ma impacchettato in HLS/DASH solo H.264/AAC)
- Distribuzione: **HTTP-FLV**, **WS-FLV** (≈1–2 s), **LL-HLS** (≈2–3 s), **LL-DASH** (≈2–3 s)
- Dashboard in italiano: dirette, impostazioni OBS, link/embed, metriche con grafici, registrazioni, video d'attesa
- Registrazione: automatica (flag) o manuale; FLV nativo → MP4 via ffmpeg
- Metriche: spettatori simultanei per protocollo campionati su SQLite, grafici Chart.js, picco/sessioni/durata media
- Video d'attesa per diretta o globale, mostrato dal player quando la diretta è offline o **in pausa**
- Zero dipendenze esterne a runtime sul server: SQLite incorporato; `ffmpeg` opzionale (remux MP4, normalizzazione video d'attesa). I browser caricano hls.js, mpegts.js, dash.js (jsDelivr) e Chart.js (cdnjs) da CDN

Architettura e contratto API: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) (inglese).
Standard di streaming a bassa latenza e raccomandazioni: [docs/PROTOCOLS.md](docs/PROTOCOLS.md).
Riferimento API: [docs/API.md](docs/API.md).

## Requisiti

- Linux x86_64 (testato su AlmaLinux 9), Rust stable recente per compilare (edition 2024; sviluppato con 1.99, la dipendenza `uuid` richiede almeno 1.89)
- `ffmpeg`/`ffprobe` nel PATH (opzionali ma consigliati): build statiche vanno benissimo
- Porte: `8080/tcp` (HTTP: dashboard, API, player) e `1935/tcp` (RTMP) — modificabili

## Installazione

```bash
git clone <repo> rust-media-server
cd rust-media-server
cargo build --release
./target/release/rms --help
```

Primo avvio:

```bash
./target/release/rms --data-dir ./data
```

Al primo avvio viene generato un **token amministratore** (stampato nel log e salvato in
`data/admin_token`, permessi 0600). In alternativa impostare `RMS_ADMIN_TOKEN` o
`server.admin_token` nel file di configurazione. Apri `http://<host>:8080/` e incolla il token.

## Configurazione

Copia `config.example.toml` in `config.toml` (letto dalla directory corrente) oppure usa
`--config <file>`. Ogni chiave è opzionale. Precedenza: default < file < variabili
`RMS_<SEZIONE>_<CHIAVE>` < flag CLI (`--data-dir`, `--http-bind`, `--rtmp-bind`).

Chiavi principali:

| Chiave | Default | Note |
|---|---|---|
| `server.http_bind` | `0.0.0.0:8080` | dashboard, API, player, HLS/DASH/FLV |
| `server.public_url` | vuoto | URL pubblico (es. `https://stream.example.com`) usato nei link e negli embed; se vuoto deriva dall'header `Host` |
| `server.trust_proxy` | `false` | usa `X-Forwarded-For` per l'IP degli spettatori dietro reverse proxy |
| `rtmp.bind` | `0.0.0.0:1935` | porta RTMP per OBS |
| `rtmp.max_connections` | `256` | connessioni RTMP simultanee ammesse |
| `hls.part_duration` | `0.5` | durata delle part LL-HLS (s) |
| `hls.window_segments` | `6` | segmenti mantenuti nella playlist |
| `dash.segment_duration` | `2.0` | durata nominale dei segmenti DASH (s) |
| `recording.remux_to_mp4` | `true` | converte le registrazioni FLV in MP4 (richiede ffmpeg) |
| `metrics.sample_interval_secs` | `5` | campionamento spettatori |
| `metrics.retention_days` | `90` | ritenzione delle serie storiche |
| `ffmpeg.path` | vuoto | percorso di ffmpeg; vuoto = cerca nel PATH |

## Flusso di lavoro

1. Nella dashboard crea una **diretta** (titolo → nome URL automatico). Ogni diretta ha una stream key segreta.
2. In OBS: *Impostazioni → Stream → Servizio: Personalizzato*, Server `rtmp://<host>:1935/live`, Chiave `<stream key>`.
   Impostazioni consigliate (vedi `docs/PROTOCOLS.md` §5): **intervallo keyframe 2 s** (1 s per latenza minima),
   x264 `tune=zerolatency` o NVENC *Low latency*, CBR, nessun B-frame, audio AAC 48 kHz.
3. Avvia la diretta in OBS: la dashboard mostra lo stato *In diretta*, codec, risoluzione, spettatori.
4. Copia il codice `<iframe>` dalla sezione *Link e embed* e incollalo nel sito. Il player sceglie da solo
   la sorgente migliore (HTTP-FLV su desktop/Android, LL-HLS nativo su iOS) e mostra il
   **video d'attesa** finché la diretta non arriva, se cade o se la metti **in pausa**.
5. Attiva la registrazione (flag *Registra automaticamente* sulla diretta, oppure *Registra ora* durante la diretta).
   I file finiscono in `data/recordings/<nome>/` e sono scaricabili dalla dashboard.

### URL di riproduzione

| Protocollo | URL | Player |
|---|---|---|
| HTTP-FLV | `http://<host>:8080/live/<nome>.flv` | mpegts.js, VLC, ffplay |
| WebSocket-FLV | `ws://<host>:8080/ws/live/<nome>.flv` | mpegts.js |
| LL-HLS | `http://<host>:8080/hls/<nome>/master.m3u8` | hls.js, Safari/iOS, VLC |
| LL-DASH | `http://<host>:8080/dash/<nome>/manifest.mpd` | dash.js |
| Player embed | `http://<host>:8080/embed/<nome>` | iframe |
| Player pagina intera | `http://<host>:8080/player/<nome>` | browser |

Parametri del player embed (tutti opzionali): `?autoplay=1|0&muted=1|0&controls=1|0&src=auto|flv|hls|dash&latency=1|0` (default: `1`, `1`, `1`, `auto`; `latency` è `0` in `/embed` e `1` in `/player`). Dettagli in [docs/API.md](docs/API.md) §7.

Esempio di embed:

```html
<iframe src="https://stream.example.com/embed/evento-2026?autoplay=1&muted=1"
        width="960" height="540" frameborder="0"
        allow="autoplay; fullscreen; picture-in-picture" allowfullscreen></iframe>
```

## Test senza OBS

```bash
# terminale 1
./target/release/rms --data-dir /tmp/rms-data --http-bind 127.0.0.1:8080 --rtmp-bind 127.0.0.1:1935
# terminale 2: crea una diretta e pubblica una mira di prova con ffmpeg
TOKEN=$(cat /tmp/rms-data/admin_token)
curl -s -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
     -d '{"title":"Prova"}' http://127.0.0.1:8080/api/v1/broadcasts
scripts/test-publish.sh <stream_key> 127.0.0.1:1935
# terminale 3: verifica
DATA_DIR=/tmp/rms-data scripts/smoke.sh http://127.0.0.1:8080   # oppure: scripts/smoke.sh http://127.0.0.1:8080 "$TOKEN"
ffprobe http://127.0.0.1:8080/live/prova.flv
```

## API

Tutte le rotte amministrative stanno sotto `/api/v1` e richiedono `Authorization: Bearer <token>`
oppure il cookie impostato da `POST /api/v1/auth/login`. Le rotte pubbliche per il player sono
sotto `/api/v1/public/`. Elenco completo in [docs/API.md](docs/API.md); esempi rapidi:

```bash
TOKEN=...; H="Authorization: Bearer $TOKEN"; B=http://127.0.0.1:8080/api/v1
curl -s -H "$H" $B/broadcasts                                   # elenco con stato e spettatori
curl -s -H "$H" -X POST $B/broadcasts/prova/pause                # pausa (gli spettatori vedono il video d'attesa)
curl -s -H "$H" -X POST $B/broadcasts/prova/record/start         # registrazione manuale
curl -s -H "$H" "$B/broadcasts/prova/metrics?step=60"            # serie spettatori ultima ora
curl -s -H "$H" $B/broadcasts/prova/embed                        # iframe e snippet pronti
curl -s -H "$H" -F file=@attesa.mp4 $B/broadcasts/prova/waiting-video
curl -N -H "$H" $B/events                                        # SSE: stato, spettatori
```

## Deploy

- `deploy/rms.service`: unit systemd (modificare utente e percorsi).
- `deploy/nginx.conf.example` / `deploy/Caddyfile.example`: reverse proxy con TLS, WebSocket e
  senza buffering (indispensabile per FLV, LL-HLS e LL-DASH). Impostare `server.public_url`
  all'URL pubblico e `server.trust_proxy = true`.
- RTMP resta sulla porta 1935 (opzionalmente RTMPS tramite il modulo `stream` di nginx).

## Struttura del progetto

```
src/main.rs        avvio e wiring          src/hub/        registro stream, GOP cache, pacchetti FLV
src/ingest/rtmp.rs ingest RTMP (rtmp-rs)   src/egress/     HTTP-FLV, WS-FLV, LL-HLS, LL-DASH
src/packager/      muxer CMAF, store, playlist HLS, MPD DASH
src/recorder/      registrazione FLV → MP4 src/metrics/    tracker spettatori, campionatore
src/api/           REST + SSE (axum)       src/web/        dashboard e player (statici incorporati)
src/db/            SQLite (rusqlite)       docs/           architettura, protocolli, API
```

## Licenza

MIT.

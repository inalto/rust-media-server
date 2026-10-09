# rust-media-server (rms)

Media server per dirette a bassa latenza scritto in Rust, in un unico binario. Ispirato ad
Ant Media Server e a `xiu`: OBS pubblica in RTMP, il server ridistribuisce ai browser in
WebRTC (WHEP), HTTP-FLV / WebSocket-FLV, LL-HLS e LL-DASH (CMAF), registra su richiesta, raccoglie le
metriche degli spettatori simultanei e offre una dashboard di gestione (axum) con embed
pronti da incollare e un video d'attesa quando la diretta non c'è.

- Ingest: **RTMP** (stream key segreta per ogni diretta; Enhanced RTMP accettato per HEVC/AV1 ma impacchettato in HLS/DASH solo H.264/AAC)
- Distribuzione: **WebRTC/WHEP** (sotto il secondo), **HTTP-FLV**, **WS-FLV** (≈1–2 s), **LL-HLS** (≈2–3 s), **LL-DASH** (≈2–3 s)
- Dashboard in italiano: dirette, impostazioni OBS, link/embed, metriche con grafici, registrazioni, video d'attesa
- Registrazione: automatica (flag) o manuale; FLV nativo → MP4 via ffmpeg
- Metriche: spettatori simultanei per protocollo campionati su SQLite, grafici Chart.js, picco/sessioni/durata media
- Video d'attesa per diretta o globale, mostrato dal player quando la diretta è offline o **in pausa**
- Zero dipendenze esterne a runtime sul server: SQLite incorporato; `ffmpeg` opzionale (remux MP4, normalizzazione video d'attesa, audio Opus per WebRTC). I browser caricano hls.js, mpegts.js, dash.js (jsDelivr) e Chart.js (cdnjs) da CDN; il client WebRTC non usa librerie

Architettura e contratto API: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) (inglese).
Standard di streaming a bassa latenza e raccomandazioni: [docs/PROTOCOLS.md](docs/PROTOCOLS.md).
Riferimento API: [docs/API.md](docs/API.md).

## Requisiti

- Linux x86_64 (testato su AlmaLinux 9), Rust stable recente per compilare (edition 2024; sviluppato con 1.99, la dipendenza `uuid` richiede almeno 1.89)
- `ffmpeg`/`ffprobe` nel PATH (opzionali ma consigliati): build statiche vanno benissimo
- Porte: `8080/tcp` (HTTP: dashboard, API, player), `1935/tcp` (RTMP) e `8443/udp` (WebRTC: **una sola porta UDP** per tutti gli spettatori, va aperta nel firewall) — modificabili

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
| `webrtc.enabled` | `true` | uscita WebRTC via WHEP (`/whep/<nome>`) |
| `webrtc.udp_port` | `8443` | **unica porta UDP** per tutte le sessioni WebRTC (ICE, DTLS, SRTP): va aperta in ingresso (`8443/udp`); non passa dal reverse proxy |
| `webrtc.public_ip` | `[]` | IP pubblici IPv4 annunciati ai browser (candidati ICE). Vuoto = indirizzi delle interfacce locali: va bene in LAN o su un server con IP pubblico diretto. **Dietro NAT 1:1** (VM cloud con IP elastico, router con port forwarding di `8443/udp`) indicare l'IP pubblico, es. `["203.0.113.7"]` (variabile: `RMS_WEBRTC_PUBLIC_IP=203.0.113.7`) |
| `webrtc.ice_lite` | `true` | ICE-lite: il server risponde solo ai controlli di connettività (ha un indirizzo raggiungibile) |
| `webrtc.max_sessions` | `100` | sessioni WebRTC simultanee (oltre: `503`, il player passa a HTTP-FLV) |
| `webrtc.audio_bitrate_kbps` | `96` | bitrate Opus: l'AAC di OBS viene transcodificato in Opus con ffmpeg (senza ffmpeg le sessioni WebRTC sono solo video) |
| `webrtc.idle_stop_secs` | `30` | ferma il publisher WebRTC di una diretta dopo questi secondi senza spettatori WebRTC |
| `webrtc.stun_server` | vuoto | server STUN opzionale annunciato ai client nell'header `Link` (es. `stun.l.google.com:19302`) |

**Lingua dell'interfaccia.** Dashboard e player sono in italiano (predefinito) o in inglese: si sceglie
nella dashboard in *Impostazioni → Lingua e tema* (oppure `PATCH /api/v1/settings {"ui_language":"en"}`), vale
subito per le dashboard aperte e per i testi del player incorporato (badge di attesa/pausa, pulsanti, testo
di attesa predefinito: «La diretta inizierà a breve» / «The live stream will start shortly»). I testi di
attesa personalizzati restano come sono. Un singolo embed può forzare la lingua con `?lang=it|en`.

**Tema chiaro/scuro.** Il tema predefinito di dashboard e player (*Sistema*, che segue il dispositivo, *Chiaro* o *Scuro*) si sceglie in *Impostazioni → Lingua e tema* (oppure `PATCH /api/v1/settings {"ui_theme":"light"}`); ogni browser può cambiarlo per sé con il pulsante sole/luna in alto nella dashboard e un singolo embed può forzarlo con `?theme=light|dark|system`.

## Flusso di lavoro

1. Nella dashboard crea una **diretta** (titolo → nome URL automatico). Ogni diretta ha una stream key segreta.
2. In OBS: *Impostazioni → Stream → Servizio: Personalizzato*, Server `rtmp://<host>:1935/live`, Chiave `<stream key>`.
   Impostazioni consigliate (vedi `docs/PROTOCOLS.md` §5): **intervallo keyframe 2 s** (1 s per latenza minima),
   x264 `tune=zerolatency` o NVENC *Low latency*, CBR, audio AAC 48 kHz e **nessun B-frame**: per WebRTC è
   obbligatorio (il video H.264 passa senza transcodifica e i browser non riordinano i B-frame; con x264
   `zerolatency` sono già disattivati, con NVENC/QSV impostare *B-frames = 0*). Un nuovo spettatore WebRTC
   parte dal keyframe successivo: con keyframe ogni 2 s attende al massimo 2 s.
3. Avvia la diretta in OBS: la dashboard mostra lo stato *In diretta*, codec, risoluzione, spettatori.
4. Copia il codice `<iframe>` dalla sezione *Link e embed* e incollalo nel sito. Il player sceglie da solo
   la sorgente migliore (WebRTC se disponibile, altrimenti HTTP-FLV su desktop/Android e LL-HLS nativo su
   iOS; se WebRTC non si connette entro pochi secondi, ad esempio per un firewall UDP, passa da solo alla
   sorgente successiva) e mostra il
   **video d'attesa** finché la diretta non arriva, se cade o se la metti **in pausa**.
5. Attiva la registrazione (flag *Registra automaticamente* sulla diretta, oppure *Registra ora* durante la diretta).
   I file finiscono in `data/recordings/<nome>/` e sono scaricabili dalla dashboard.

### URL di riproduzione

| Protocollo | URL | Player |
|---|---|---|
| WebRTC (WHEP) | `http://<host>:8080/whep/<nome>` (POST dell'offerta SDP; media su `8443/udp`) | player embed, snippet WHEP della dashboard, client WHEP (es. GStreamer `whepsrc`) |
| HTTP-FLV | `http://<host>:8080/live/<nome>.flv` | mpegts.js, VLC, ffplay |
| WebSocket-FLV | `ws://<host>:8080/ws/live/<nome>.flv` | mpegts.js |
| LL-HLS | `http://<host>:8080/hls/<nome>/master.m3u8` | hls.js, Safari/iOS, VLC |
| LL-DASH | `http://<host>:8080/dash/<nome>/manifest.mpd` | dash.js |
| Player embed | `http://<host>:8080/embed/<nome>` | iframe |
| Player pagina intera | `http://<host>:8080/player/<nome>` | browser |

Parametri del player embed (tutti opzionali): `?autoplay=1|0&muted=1|0&controls=1|0&src=auto|webrtc|flv|hls|dash&latency=1|0&lang=it|en&theme=system|light|dark` (default: `1`, `1`, `1`, `auto`; `latency` è `0` in `/embed` e `1` in `/player`; `lang` e `theme` predefiniti = lingua e tema impostati in *Impostazioni*). Con `auto` l'ordine è WebRTC → HTTP-FLV → LL-HLS → LL-DASH. Dettagli in [docs/API.md](docs/API.md) §6.5 e §7.

**WebRTC: rete.** Il segnale WHEP passa in HTTP(S) dal reverse proxy come il resto, ma l'audio/video viaggia
su UDP direttamente verso `webrtc.udp_port` (default `8443/udp`): la porta va aperta nel firewall e, dietro
NAT, inoltrata al server con `webrtc.public_ip` impostato all'IP pubblico. Non serve un server TURN per
spettatori con accesso UDP; reti che bloccano l'UDP in uscita (alcune aziendali) non riescono a
connettersi e il player ripiega su HTTP-FLV/LL-HLS. Per la riproduzione WebRTC del player embed l'audio AAC
viene convertito in Opus con ffmpeg (senza ffmpeg la sessione è solo video).

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
# WebRTC: apri http://127.0.0.1:8080/player/prova?src=webrtc (porta UDP 8443 libera)
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
curl -s -H "$H" -H 'Content-Type: application/json' -X PATCH -d '{"ui_language":"en"}' $B/settings   # interfaccia in inglese
```

## Deploy

- `deploy/rms.service`: unit systemd (modificare utente e percorsi).
- `deploy/nginx.conf.example` / `deploy/Caddyfile.example`: reverse proxy con TLS, WebSocket e
  senza buffering (indispensabile per FLV, LL-HLS e LL-DASH). Impostare `server.public_url`
  all'URL pubblico e `server.trust_proxy = true`. Il proxy inoltra anche `/whep/` (HTTP), ma **non** la
  porta UDP WebRTC: aprire `8443/udp` direttamente sul server (es. `firewall-cmd --add-port=8443/udp --permanent`).
- RTMP resta sulla porta 1935 (opzionalmente RTMPS tramite il modulo `stream` di nginx).

## Struttura del progetto

```
src/main.rs        avvio e wiring          src/hub/        registro stream, GOP cache, pacchetti FLV
src/ingest/rtmp.rs ingest RTMP (rtmp-rs)   src/egress/     HTTP-FLV, WS-FLV, LL-HLS, LL-DASH
src/webrtc/        WHEP: porta UDP condivisa, sessioni WebRTC (rtc), H.264 → RTP, AAC → Opus (ffmpeg)
src/packager/      muxer CMAF, store, playlist HLS, MPD DASH
src/recorder/      registrazione FLV → MP4 src/metrics/    tracker spettatori, campionatore
src/api/           REST + SSE (axum)       src/web/        dashboard e player (statici incorporati)
src/db/            SQLite (rusqlite)       docs/           architettura, protocolli, API
```

## Limiti noti e roadmap

- Il packager LL-HLS/LL-DASH impacchetta solo **H.264 + AAC**; HEVC/AV1 (Enhanced RTMP) passano solo via HTTP-FLV/WS-FLV.
- WebRTC: solo video H.264 senza B-frame (HEVC/AV1 → `415`, il player usa un'altra sorgente); solo IPv4; nessun TURN né trickle ICE; un nuovo spettatore parte dal keyframe successivo; senza ffmpeg niente audio.
- Nessun transcoding/ABR: la qualità è quella inviata da OBS (consigliati 2–6 Mbit/s CBR, keyframe 2 s).
- Safari nativo e HTTP/2 non sono stati verificati in questo ambiente (vedi `docs/ARCHITECTURE.md` §15).
- Prossimi passi previsti: ingest SRT/WHIP, token per gli spettatori, TURN/IPv6 per WebRTC.

## Licenza

MIT.

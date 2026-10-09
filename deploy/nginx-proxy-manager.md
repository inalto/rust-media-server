# Nginx Proxy Manager davanti a rms

Guida passo-passo per pubblicare rms con **Nginx Proxy Manager** (NPM): TLS Let's Encrypt, HTTP/2, WebSocket, media senza buffering. Verificata il 2026-10-09 su **NPM 2.16.0** (rilasciata il 2026-09-24): il sito ufficiale documenta poco le singole opzioni, quindi i template nginx (`backend/templates/*.conf`, `docker/rootfs/etc/nginx/**`) e le etichette dell'interfaccia (`frontend/src/locale/src/en.json`) sono quelli del tag `v2.16.0` [1][2]. Per l'esecuzione con più istanze vedere `docs/CLUSTER.md`.

```
Internet ──80/443──> NPM ──http──> rms:8080     dashboard, API, player, FLV, WS-FLV, HLS, DASH, segnalazione WHEP
OBS ───────1935/tcp─> NPM Stream ─> rms:1935    (oppure diretto su rms: §7)
Browser ───8443/udp──────────────> rms:8443     WebRTC: DIRETTO, mai attraverso il Proxy Host HTTP
```

## 1. Configurazione di rms

```toml
[server]
public_url = "https://stream.example.com"   # link ed embed della dashboard
trust_proxy = true                           # IP spettatori da X-Forwarded-For / X-Real-IP
[webrtc]
public_ip = ["203.0.113.10"]                 # IP pubblico dell'host che riceve 8443/udp
```

Equivalenti ambiente: `RMS_SERVER_PUBLIC_URL`, `RMS_SERVER_TRUST_PROXY=true`, `RMS_WEBRTC_PUBLIC_IP`. **Limite noto:** con `trust_proxy` rms legge il *primo* elemento di `X-Forwarded-For`, ma il Proxy Host di NPM invia `$proxy_add_x_forwarded_for`, cioè *accoda* all'header già presente nella richiesta del client [2]: un client può falsificare il proprio IP nelle metriche (verifica al punto 8 del §8). L'effetto è limitato alle statistiche; la correzione giusta sta in rms (usare `X-Real-IP`, che NPM imposta a `$remote_addr`, o l'ultimo hop).

## 2. NPM e rms sulla stessa rete (Docker/Podman Compose)

```yaml
services:
  npm:
    image: jc21/nginx-proxy-manager:2.16.0
    restart: unless-stopped
    ports:
      - "80:80"
      - "443:443"
      - "127.0.0.1:81:81"      # interfaccia di amministrazione: non esporla a Internet
      - "1935:1935"            # Stream TCP verso rms (§7); omettere se OBS va diretto su rms
    volumes: [ "./npm/data:/data", "./npm/letsencrypt:/etc/letsencrypt" ]
    networks: [ proxy ]
  rms:
    image: rms:latest
    restart: unless-stopped
    environment:
      RMS_ADMIN_TOKEN: "una-stringa-lunga-e-casuale"
      RMS_SERVER_PUBLIC_URL: "https://stream.example.com"
      RMS_SERVER_TRUST_PROXY: "true"
      RMS_WEBRTC_PUBLIC_IP: "203.0.113.10"
    volumes: [ "rms-data:/data" ]
    ports: [ "8443:8443/udp" ]  # WebRTC, diretto
    expose: [ "8080", "1935" ]  # raggiungibili solo da NPM sulla rete "proxy"
    networks: [ proxy ]
networks: { proxy: {} }
volumes: { rms-data: {} }
```

Nel Proxy Host si usa il nome del servizio (`rms`): NPM lo risolve a runtime con il resolver generato dal container (`valid=10s`) [2], quindi un cambio di IP dopo un riavvio di rms è visto entro ~10 s. Le porte dello Stream vanno pubblicate a mano sul container NPM: la UI non apre porte [3]. **Podman rootless** (host con Podman 5.8): 80/443 richiedono `sysctl net.ipv4.ip_unprivileged_port_start=80`; le reti bridge rootless inoltrano le porte con `rootlessport`, che non conserva l'IP del client, mentre `pasta` sì [4]; `podman-compose` richiede netavark/aardvark-dns per il DNS fra container [5].

## 3. Proxy Host: scheda *Details*

*Hosts → Proxy Hosts → Add Proxy Host*:

| Campo | Valore | Perché |
|---|---|---|
| Domain Names | `stream.example.com` | Un solo host per tutto: dashboard, API, player, media |
| Scheme / Forward Hostname / Forward Port | `http` / `rms` / `8080` | Il TLS termina su NPM |
| **Cache Assets** | **OFF** | Attiva `assets.conf`: cache di 30 min per i file `.css`, `.js`, `.jpg`/`.jpeg`, `.png`, `.gif`, `.webp`, `.svg`, `.ico`, `.woff`…, ignorando `Cache-Control` e togliendo `Last-Modified` [2]. rms serve dashboard e player con `no-cache` + ETag, e poster `.jpg` che cambiano: con la cache si vedrebbero versioni vecchie |
| Block Common Exploits | ON (consigliato) | Regole su query string e `User-Agent` (UNION SELECT, `<script>`, `libwww-perl`, `GetRight`…) [2]. Non toccano `_HLS_msn=…&_HLS_part=…`, `?g=1`, `?lang=it`. Se un client legittimo riceve `403`, disattivare e rivalutare |
| **Websockets Support** | **ON** | Necessario per `/ws/live/<nome>.flv` (WS-FLV). Aggiunge `Upgrade $http_upgrade`, `Connection $http_connection` e HTTP/1.1; per le richieste normali il `Connection` del browser (`keep-alive`) passa invariato, con HTTP/2 è vuoto e nginx non lo invia [2][6] |
| Access List | *Publicly Accessible* | Vedi §6 |

## 4. Proxy Host: scheda *SSL*

- *SSL Certificate → Request a new SSL Certificate* (Let's Encrypt): il dominio deve già puntare a NPM e la porta 80 essere raggiungibile (HTTP-01); in alternativa *Use DNS Challenge* con il provider DNS [2].
- **Force SSL** ON. **HTTP/2 Support** ON: il profilo Low-Latency di HLS richiede playlist e segmenti serviti via HTTP/2 o HTTP/3 [7], e HTTP/2 evita il limite tipico di 6 connessioni per host di HTTP/1.1 quando una pagina apre player, SSE e richieste HLS insieme (il template emette `http2 on;`) [2].
- *HSTS Enabled* solo quando tutto il dominio è sicuramente in HTTPS (il template imposta due anni) [2]. *Trust Upstream Forwarded Proto Headers* resta OFF, a meno che NPM sia a sua volta dietro un altro terminatore TLS.

## 5. Proxy Host: scheda *Advanced*

Incollare nel campo *Custom Nginx Configuration*:

```nginx
# rms: media in streaming (HTTP-FLV, SSE, parti LL-HLS, chunk LL-DASH) e upload grandi
proxy_buffering off;
proxy_request_buffering off;
proxy_cache off;
proxy_read_timeout 3600s;
proxy_send_timeout 3600s;
client_max_body_size 1100m;
gzip off;
```

Il testo viene inserito a livello **server**, prima di `location /` [2]; vale in `location /` (e nelle Custom Locations) per le direttive che la location non ridefinisce.

| Direttiva | Motivo |
|---|---|
| `proxy_buffering off` | Di default nginx accumula la risposta in buffer: HTTP-FLV, SSE, parti LL-HLS e chunk LL-DASH arriverebbero a raffiche. rms invia già `X-Accel-Buffering: no` su FLV, SSE e risposte low-latency (nginx lo rispetta) [6]; la direttiva rende il comportamento indipendente dall'header |
| `proxy_request_buffering off` | Gli upload (video d'attesa, VOD) vanno a rms man mano che arrivano, invece di essere scritti per intero in `/tmp/nginx/body` (`client_body_temp_path` di NPM) prima di partire [2][6] |
| `proxy_cache off` | NPM ha già `proxy_cache off` a livello http (lo cambia solo *Cache Assets*); esplicito per sicurezza |
| `proxy_read_timeout` / `proxy_send_timeout 3600s` | Il default di NPM è 90 s *tra due letture* [2][6]: un publisher fermo, o un WS-FLV senza traffico (rms risponde ai ping ma non ne invia), verrebbe tagliato |
| `client_max_body_size 1100m` | Il default di NPM è 2000m: qui è una restrizione. Deve essere ≥ `server.max_upload_mb` (1024 MiB di default; rms accetta quel valore + 64 KiB). Se si alza `max_upload_mb` (VOD), alzare anche questo. Oltre il limite risponde nginx (`413` HTML), non rms (JSON) |
| `gzip off` | NPM ha `gzip on` a livello http con i tipi di default di nginx (`text/html`): tocca solo l'HTML. I media non vanno mai compressi; esplicito per evitare sorprese |

**Cosa non serve (e perché è stato tolto dalla proposta iniziale).** `proxy_http_version 1.1`: NPM lo imposta già globalmente in `nginx.conf` e nel blocco WebSocket. `proxy_set_header Connection ""`: a livello server è **inefficace**, perché `proxy_set_header` si eredita dal livello precedente solo se nel livello corrente non ce ne sono [6], e `location /` ne ha (`proxy.conf` e il blocco WebSocket) [2]; se funzionasse toglierebbe `Connection: Upgrade` e romperebbe WS-FLV. Stessa regola per `add_header`: nell'Advanced tab non arriva a `location /`.

## 6. Access List e Custom Locations

- **Access Lists.** L'Access List di un Proxy Host si applica a tutte le location, incluse quelle custom [2]. Con utenti genera `auth_basic` e, senza *Pass Auth to Upstream*, `proxy_set_header Authorization ""`: il Basic blocca il player pubblico e gli embed su altri siti (e il preflight CORS, che non porta credenziali) e cancella il `Bearer` dell'API rms (il cookie `rms_token` passerebbe). Lasciare *Publicly Accessible*; l'accesso amministrativo è protetto dal token di rms.
- **Custom Locations** (facoltative; un solo host basta). Per limitare l'amministrazione a una rete: *Add Location* `/api/v1/` → `rms:8080` con, nel campo di configurazione personalizzata della location (il template lo inserisce dentro il blocco `location`), `allow 203.0.113.0/24; deny all;`; poi *Add Location* `/api/v1/public/` → `rms:8080` senza restrizioni (nginx sceglie il prefisso più lungo). Ogni location custom imposta `X-Forwarded-For $remote_addr` (non falsificabile) [2]. Da provare sulla propria istanza. In cluster aggiungere anche `/internal/` con `return 404;`.

## 7. Streams: RTMP (TCP) e WebRTC (UDP)

- **RTMP.** *Hosts → Streams → Add Stream*: Incoming Port `1935`, Forward Host `rms`, Forward Port `1935`, TCP. NPM genera `listen 1935 reuseport; proxy_pass rms:1935;` [2]; il `proxy_timeout` di nginx per gli stream è 10 minuti di inattività [6] (OBS trasmette di continuo; per allungarlo: `/data/nginx/custom/server_stream_tcp.conf` con `proxy_timeout 1h;` [3]). rms vedrà l'IP di NPM come `publisher_ip`. In OBS: `rtmp://stream.example.com:1935/live` + stream key. Se si assegna un certificato allo Stream il template aggiunge `ssl` (RTMPS): da verificare nella UI e in OBS; rms ascolta comunque RTMP in chiaro. Alternativa più semplice: pubblicare `1935:1935` direttamente su rms.
- **WebRTC (UDP).** Sconsigliato passare da NPM: lo Stream UDP (`listen 8443 udp reuseport;`) è possibile, ma (1) l'ICE annuncia `public_ip:udp_port` e la porta in ingresso dovrebbe essere identica a `webrtc.udp_port` (8443), (2) rms non potrebbe pubblicare la stessa porta sull'host, (3) rms vede l'IP di NPM e la sessione scade dopo 10 min di silenzio [6], (4) c'è un hop in più sul media. Meglio `ports: ["8443:8443/udp"]` su rms, firewall aperto (`firewall-cmd --add-port=8443/udp --permanent`) e `webrtc.public_ip` = IP pubblico dell'host (con NAT 1:1: inoltro UDP 8443 sul router). La segnalazione `POST /whep/<nome>` passa invece dal Proxy Host come ogni altra richiesta (rms risponde con CORS `*`). Senza UDP raggiungibile il player ripiega da solo su HTTP-FLV/LL-HLS.

## 8. Verifica

```bash
H=https://stream.example.com; T=$(cat data/admin_token)         # token amministratore; serve una diretta "prova" attiva
# 1. HTTP/2 e intestazioni
curl -sI --http2 $H/api/v1/public/ui | head -3                    # HTTP/2 200; x-served-by presente
# 2. HTTP-FLV in streaming: primo byte subito, non a raffiche
curl -sN -m 5 -o /dev/null -w 'ttfb=%{time_starttransfer}s\n' $H/live/prova.flv     # < 1 s
curl -sN -m 5 $H/live/prova.flv | head -c 3; echo                  # FLV
# 3. SSE: heartbeat subito, poi ogni 15 s
curl -sN -m 40 -H "Authorization: Bearer $T" $H/api/v1/events
# 4. WS-FLV (richiede Websockets Support)
curl -si --http1.1 -m 3 -H 'Connection: Upgrade' -H 'Upgrade: websocket' -H 'Sec-WebSocket-Version: 13' \
     -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' $H/ws/live/prova.flv | head -1   # HTTP/1.1 101
# 5. LL-HLS: il blocking reload risponde dopo ~una part, non subito e non con 504
curl -s $H/hls/prova/stream.m3u8 | grep -E 'MEDIA-SEQUENCE|PART-INF|PRELOAD-HINT'
curl -s -o /dev/null -w '%{http_code} %{time_total}s\n' "$H/hls/prova/stream.m3u8?_HLS_msn=<msn>&_HLS_part=<part+1>"
# 6. Upload: ~500 MiB → 200 (o 422/415 da rms); > 1100 MiB → 413 HTML di nginx
curl -s -o /dev/null -w '%{http_code}\n' -H "Authorization: Bearer $T" -F file=@grande.mp4 $H/api/v1/broadcasts/prova/waiting-video
# 7. WHEP: preflight e rotta (la sessione completa si prova col player ?src=webrtc, chrome://webrtc-internals)
curl -si -X OPTIONS $H/whep/prova | head -6                        # 204, Allow: POST, OPTIONS
# 8. IP reale e falsificabilità di X-Forwarded-For
curl -s -H "Authorization: Bearer $T" $H/api/v1/broadcasts/prova/sessions          # ip = il vostro IP pubblico
curl -sN -m 6 -o /dev/null -H 'X-Forwarded-For: 198.51.100.9' $H/live/prova.flv & sleep 2    # sessione FLV aperta
curl -s -H "Authorization: Bearer $T" $H/api/v1/broadcasts/prova/sessions          # se compare 198.51.100.9: spoofing (§1)
wait
```

## 9. Problemi tipici

| Sintomo | Causa probabile |
|---|---|
| `502` subito | `rms` non raggiungibile: nomi/rete diversi, rms fermo; NPM cache DNS ~10 s |
| FLV/SSE a raffiche, latenza alta | Manca `proxy_buffering off` (Advanced non salvato o sul Proxy Host sbagliato) |
| WS-FLV `400`/`502` | *Websockets Support* spento |
| Stream tagliato dopo 90 s di silenzio | `proxy_read_timeout` non applicato |
| Upload `413` HTML | `client_max_body_size` inferiore al file; `413` JSON: limite di rms (`server.max_upload_mb`) |
| Dashboard con CSS/JS vecchi, poster non aggiornati | *Cache Assets* attivo |
| `403` su una richiesta legittima | *Block Common Exploits* (query string o User-Agent) |
| IP spettatori = IP di NPM | `server.trust_proxy = false` |
| WebRTC non si connette, FLV sì | `8443/udp` non raggiungibile o `webrtc.public_ip` errato (vedi `candidates` in `GET /api/v1/system`) |

## Fonti

[1] https://github.com/NginxProxyManager/nginx-proxy-manager/releases (v2.16.0, 2026-09-24) · [2] sorgenti NPM `v2.16.0`: `backend/templates/proxy_host.conf`, `_location.conf`, `_access.conf`, `_listen.conf`, `stream.conf`, `docker/rootfs/etc/nginx/nginx.conf`, `conf.d/include/proxy.conf`, `assets.conf`, `block-exploits.conf`, `docker/rootfs/etc/s6-overlay/s6-rc.d/prepare/40-dynamic.sh`, `frontend/src/locale/src/en.json` (https://github.com/NginxProxyManager/nginx-proxy-manager/tree/v2.16.0) · [3] https://nginxproxymanager.com/setup/ e https://nginxproxymanager.com/advanced-config/ (file `/data/nginx/custom/*.conf`, porte degli Stream) · [4] https://man.archlinux.org/man/extra/podman/podman-rootless.7.en · [5] https://github.com/containers/podman-compose · [6] nginx: https://nginx.org/en/docs/http/ngx_http_proxy_module.html (`proxy_buffering`, `proxy_request_buffering`, `proxy_set_header`, `proxy_read_timeout`), https://nginx.org/en/docs/http/websocket.html, https://nginx.org/en/docs/stream/ngx_stream_proxy_module.html (`proxy_timeout`) · [7] https://datatracker.ietf.org/doc/html/draft-pantos-hls-rfc8216bis (profilo Low-Latency: HTTP/2 o HTTP/3).

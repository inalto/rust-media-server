# rust-media-server (rms): protocolli e standard per lo streaming live a bassa latenza

Guida per l'operatore: da OBS Studio ai viewer web, quali standard usare, perché, e come configurarli.
Complementare a `docs/ARCHITECTURE.md` (contratto di implementazione, in inglese).

- Verifiche delle fonti eseguite l'8 ottobre 2026. I numeri tra parentesi quadre rimandano alla sezione 7.
- Dove una cifra viene da un fornitore o da una misura di un singolo autore è indicato esplicitamente; nessun valore di latenza qui riportato è stato inventato.
- Stato nel progetto: **Core** = Fasi 1-2 di ARCHITECTURE.md (RTMP, HTTP/WS-FLV, LL-HLS, LL-DASH, registrazione, video di attesa); **Stretch** = Fase 4 (WHEP, SRT via ffmpeg); **Roadmap** = non ancora pianificato in fase (WHIP e oltre).

**In breve**

- Ingest: RTMP con H.264+AAC, CBR, `tune=zerolatency`, niente B-frame e **keyframe ogni 2 s impostato esplicitamente** (il default di OBS è "auto"). SRT per reti difficili (Stretch); WHIP in Roadmap.
- Delivery: desktop e Android su HTTP-FLV (mpegts.js) con ripiego LL-HLS (hls.js); iOS e Safari su LL-HLS nativo; grandi audience con LL-HLS/LL-DASH dietro CDN e HTTP/2; interattività sotto il secondo con WHEP (Stretch).
- Dietro nginx/Caddy: TLS + HTTP/2, niente buffering (`proxy_buffering off` / `flush_interval -1`), timeout di lettura alti, header WebSocket.
- HEVC/AV1 in ingresso funzionano solo sui percorsi FLV; per LL-HLS/LL-DASH restare su H.264+AAC.
- Le cifre di latenza in letteratura sono indicative: misurare sempre con un timecode impresso nel video (sezione 5.4).

## 1. Panoramica: catena ingest, server, delivery e origine della latenza

```
OBS (cattura, encoder) --contribuzione--> rms (hub, GOP cache, packager) --delivery--> player nel browser
  GOP/B-frame/lookahead    RTMP | SRT | WHIP     parti/segmenti, blocking     HTTP-FLV/WS-FLV (push)
  tune, bitrate            (TCP | UDP | UDP)     reload, chunked transfer     LL-HLS / LL-DASH (pull, CDN)
                                                                              WHEP (WebRTC, stretch)
```

La latenza "glass-to-glass" è la somma dei ritardi di ogni stadio. Ogni stadio ha una manopola precisa:

| Stadio | Cosa aggiunge ritardo | Manopola / regola verificata |
|---|---|---|
| Encoder | lookahead, B-frame, mbtree, GOP lungo | x264 `tune=zerolatency` azzera lookahead, B-frame e mbtree [16]; NVENC in OBS >= 28.1 ha "Tuning" Low/Ultra Low Latency e "Multipass" disattivabile [12] |
| GOP / keyframe | rms taglia i segmenti sui keyframe (ARCHITECTURE 6.3); un player può agganciarsi solo a un frame indipendente | OBS ha keyframe interval = 0 ("auto") di default, che lascia a x264 il valore 250 frame con scenecut [15][16]. La bozza HLS raccomanda GOP di 1-2 s [1] |
| Contribuzione | RTMP su TCP (ritrasmissioni bloccanti); SRT ha un buffer di latenza configurabile; WHIP/WebRTC il minimo | vedi sezione 2 |
| Server | GOP cache (riproduce dall'ultimo keyframe: il nuovo viewer parte "indietro"); dimensione delle parti HLS | La GOP cache "aumenta sempre la latenza" (SRS) [24]; `hls.part_duration` (0.5 s di default) |
| Rete / CDN | RTT, jitter, richieste bloccanti non gestite dalla CDN | HTTP/2 o HTTP/3 e collasso delle richieste bloccanti richiesti dal profilo low-latency HLS [1] |
| Buffer del player | distanza minima dal bordo live | LL-HLS: `PART-HOLD-BACK` >= 2x (obbligo) e >= 3x (raccomandato) la durata delle parti [1]; HLS classico: `HOLD-BACK` >= 3x `TARGETDURATION` [1]; dash.js: `liveDelay`, ServiceDescription [4][6] |
| Decodifica / rendering | decoder hardware, tab in background | non configurabile dal server |

Ordini di grandezza citati da fonti (non garanzie di rms):

| Classe | Valore | Fonte |
|---|---|---|
| HLS classico | "8 secondi o più" (Dolby OptiView); "10-12 s" (Ant Media) | [21][23] |
| LL-HLS | "due secondi o meno" (Apple, WWDC20); "3-5 s" LL-HLS/LL-DASH CMAF (Ant Media, fornitore) | [2][23] |
| RTMP end-to-end | "circa 0,8-3 s" a rete sana (SRS, misura dell'autore con un telefono) | [24] |
| WebRTC | "circa 0,5 s" (Ant Media); "sotto i 500 ms" (Dolby OptiView) | [23][21] |
| YouTube (classi di prodotto) | Low: "la maggior parte degli spettatori sotto 10 s"; Ultra-low: "sotto 5 s" | [31] |

Regola pratica (ragionamento dell'autore, non una citazione): in LL-HLS il ritardo minimo è dell'ordine di una parte + `PART-HOLD-BACK` + RTT; il GOP pesa sull'aggancio iniziale e sulla dimensione dei segmenti. La scelta di keyframe, dimensione delle parti e buffer del player conta quanto la scelta del protocollo.

## 2. Protocolli di contribuzione (OBS verso il server)

### 2.1 RTMP e Enhanced RTMP (HEVC/AV1)

- **Latenza tipica**: 0,8-3 s end-to-end a rete sana (SRS, misura indicativa) [24]. Il trasporto è TCP: una perdita di pacchetti blocca il flusso finché non arriva la ritrasmissione.
- **Pro**: supportato da OBS, ffmpeg e da tutti gli encoder hardware; semplice; autenticazione con stream key; implementazione matura (`rtmp-rs`).
- **Contro**: nessuna cifratura (la stream key viaggia in chiaro, a meno di RTMPS); il RTMP storico è in pratica limitato a H.264/AAC.
- **Enhanced RTMP** (Veovera) [17]: v1 introduce FourCC, HEVC, AV1, VP9 e HDR; v2 aggiunge multitrack, ModEx, audio Opus/FLAC/AC-3/E-AC-3, reconnect request e offset a precisione di nanosecondo. È un progetto industriale (YouTube, Twitch, OBS, FFmpeg, Ant Media tra i sostenitori); non risulta ratificato da un ente di standardizzazione.
- **OBS**: AV1/HEVC su Enhanced RTMP v1 dalla 29.1 (maggio 2023, inizialmente abilitato solo per YouTube) [12]; multitrack audio/video dalla 30.2 (luglio 2024) [12]. Nel sorgente del servizio "Custom" (`rtmp-custom.c`) non compaiono restrizioni di codec e l'output RTMP dichiara `h264;hevc;av1` (HEVC solo nelle build che lo abilitano) [15].
- **`rtmp-rs` 0.6.0** implementa Enhanced RTMP v2 (HEVC, AV1, VP9, VP8; Opus, FLAC, AC-3, E-AC-3) con modalità `Auto`, `EnhancedOnly`, `LegacyOnly` [18].
- **Conseguenza per rms**: ARCHITECTURE.md prevede il packager CMAF solo per H.264+AAC; con HEVC/AV1 funzionano solo i percorsi FLV. mpegts.js riproduce HEVC da 1.7.3 e AV1 da 1.8.0 in FLV [8], se il browser decodifica quel codec. **Per ora usare H.264+AAC** se servono anche LL-HLS e LL-DASH.
- **Impostazioni OBS**: sezione 5.2. **Stato**: **Core**.

### 2.2 SRT

- **Latenza tipica**: è un parametro. Il buffer di ricezione (`latency`) vale 120 ms di default in libsrt [19]; Haivision indica come regola pratica 4x RTT su un collegamento con perdite circa 0,1-0,2% e senza raffiche [19]; la guida OBS indica almeno 2,5x RTT [13]. La latenza negoziata è il maggiore tra i due estremi [19]. Attenzione alle unità: **microsecondi** nell'URL di OBS e ffmpeg (`latency=1000000` = 1 s) [13][19], millisecondi nell'API libsrt.
- **Pro**: UDP con recupero ARQ pensato per reti con perdite, cifratura AES con passphrase, ottimo su Internet pubblico e su connessioni mobili/Wi-Fi.
- **Contro**: non è riproducibile dai browser (solo contribuzione); la specifica è una Internet-Draft individuale scaduta (`draft-sharabayko-srt-01`, 2021, Independent Submission, nessun RFC) [19]; OBS non ha un campo "chiave": va nell'URL.
- **OBS**: output SRT/RIST nativi dalla 28.0, cifratura/autenticazione dalla 29.0 [12]. Impostazioni > Stream > Servizio "Custom", server `srt://host:porta?streamid=<chiave>&latency=<us>`, campo chiave vuoto [13]; OBS riconosce i prefissi `srt://` e `rist://` [15]. `streamid` massimo 512 caratteri, senza interpretazione da parte di SRT [19].
- **Nel progetto** (ARCHITECTURE 5): ponte opzionale `ffmpeg -i "srt://0.0.0.0:<porta>?mode=listener&streamid=<chiave>" -c copy -f flv rtmp://127.0.0.1:1935/live/<chiave>`. Avvertenze: ffmpeg non interpreta lo `streamid` [19], quindi il controllo accessi va fatto con una porta UDP per broadcast e/o `passphrase` (la lunghezza minima è 10 caratteri); un processo per broadcast; un hop in più; con HEVC verificare che la versione di ffmpeg scriva HEVC in FLV.
- **Stato**: **Stretch** (Fase 4, ponte ffmpeg); SRT nativo in Roadmap (sezione 6).

### 2.3 WHIP (WebRTC-HTTP Ingestion Protocol)

- **Standard**: RFC 9725, Standards Track, marzo 2025 [9]. POST di un'offerta SDP (`application/sdp`) e risposta `201 Created` con SDP answer e header `Location`; `DELETE` per chiudere; `PATCH` per trickle ICE e riavvio ICE (opzionale per il server); Bearer token obbligatorio da supportare; HTTPS obbligatorio; media unidirezionali, senza rinegoziazione [9]. L'RFC non impone codec (gli esempi usano Opus e VP8) [9].
- **Latenza tipica**: sub-secondo (come WebRTC: circa 0,5 s [23], sotto 500 ms [21]). Un articolo tecnico su OBS 30 beta riporta circa 42 ms glass-to-glass in un test locale dell'autore: non rappresentativo di Internet [28].
- **Pro**: latenza minima, cifratura DTLS-SRTP, un solo endpoint HTTP, nessun transcoding se il server inoltra RTP ai viewer WHEP.
- **Contro**: richiede ICE/NAT traversal (porte UDP, a volte TURN); l'implementazione server è complessa (ICE, DTLS, SRTP, NACK/PLI, controllo di congestione); l'audio è Opus (HLS e FLV classico richiedono AAC: serve transcodifica); sensibile al sovraccarico dell'encoder.
- **OBS**: output WHIP/WebRTC dalla 30.0 (novembre 2023) [12]. Al lancio solo H.264 + Opus e con x264 serviva `zerolatency` (profilo baseline funzionante) [28]; AV1 dalla 30.1, HEVC e WHIP solo audio/solo video dalla 30.2, simulcast dalla 32.1 [12][14]. Configurazione: Servizio WHIP, Server = URL, Bearer Token = chiave [14]. Il PPA Ubuntu 24.04 non supporta WHIP: usare Flatpak [14].
- **Impostazioni OBS consigliate** (la guida OBS non ne dà [14]): x264 con `tune=zerolatency`, B-frame 0, CBR, keyframe 1-2 s, profilo baseline/constrained.
- **Stato**: **Roadmap**.

### 2.4 RIST (menzione)

Famiglia di raccomandazioni tecniche del Video Services Forum: TR-06-1 Simple Profile (2020), TR-06-2 Main Profile (edizione corrente 12/06/2024), TR-06-3 Advanced Profile (edizione corrente 12/06/2024) [20]. Gira su RTP/UDP con ritrasmissione NACK via RTCP (ARQ). Usato soprattutto in ambito broadcast professionale. OBS supporta RIST dalla 27.2 (febbraio 2022), output nativo dalla 28.0 [12]. Non è previsto in rms: per un'eventuale ingestione si passerebbe da ffmpeg/libRIST, con le stesse limitazioni del ponte SRT. **Stato**: non pianificato.

## 3. Protocolli di distribuzione (server verso viewer)

### 3.1 HTTP-FLV e WebSocket-FLV

- **Latenza tipica**: obiettivo di progetto 1-2 s (ARCHITECTURE 1). mpegts.js dichiara "meno di 1 secondo nel caso migliore" [8]. Il nuovo viewer riceve la GOP cache dall'ultimo keyframe: parte in ritardo di fino a un GOP e deve recuperare (sotto).
- **Compatibilità**: nessun supporto nativo; serve MSE + fetch streaming. mpegts.js: Chrome 43+, Firefox 42+, Safari 10.1+, Edge [8]. iPhone: solo iOS >= 17.1 tramite ManagedMediaSource [8], e Apple lo abilita su iPhone solo se è presente una sorgente alternativa AirPlay o il remote playback è disattivato [25]. Smart TV e set-top box: in generale no.
- **Scalabilità**: una connessione persistente per viewer verso l'origine; niente cache di segmenti, quindi le CDN generiche non aiutano. Adatto ad audience contenute su un solo nodo (il limite reale dipende da banda e CPU dell'host: da misurare). WS-FLV serve dove un proxy non gestisce bene il chunked streaming; resta TCP (blocco in testa alla coda).
- **Player consigliato**: mpegts.js (ultima release al controllo: 1.8.2) [8]. HTTP-FLV richiede header CORS `Access-Control-Allow-Origin` [8]. Da una pagina https usare `https://` o `wss://` (mixed content).
- **Parametri chiave** (default da [8]): `liveBufferLatencyChasing` (max 1,5 s, minimo residuo 0,5 s) oppure `liveSync` (soglia 1,2 s, target 0,8 s, rate 1,2); `enableStashBuffer: false` riduce la latenza ma può andare in stallo con jitter di rete.

```js
const p = mpegts.createPlayer({ type: 'flv', isLive: true, url: 'https://tv.example.com/live/evento.flv' },
  { enableWorker: true, liveBufferLatencyChasing: true,
    liveBufferLatencyMaxLatency: 1.5, liveBufferLatencyMinRemain: 0.5 });
p.attachMediaElement(video); p.load(); p.play();
```

- **Stato**: **Core**.

### 3.2 LL-HLS (Apple Low-Latency HLS, CMAF/fMP4)

- **Standard**: estensioni integrate in HLS 2nd edition, `draft-pantos-hls-rfc8216bis-22` (1 maggio 2026): ancora Internet-Draft individuale (Informational, via Independent Submission), senza numero RFC, descrive la versione 13 del protocollo e farà decadere RFC 8216 se approvato [1]. Apple: LL-HLS fuori beta in iOS 14, tvOS 14 e macOS dell'epoca, senza entitlement [2]. Il push HTTP/2 è stato sostituito dai Blocking Preload Hints [2].
- **Latenza tipica**: "due secondi o meno" (Apple) [2]; 3-5 s citati da un fornitore per LL-HLS/LL-DASH CMAF [23]; obiettivo rms 2-3 s.
- **Compatibilità**: Safari/iOS/iPadOS/tvOS/macOS in modo nativo; Chrome, Firefox, Edge via hls.js (supporta parti, blocking reload, delta update e rendition report) [7]. **iOS: usare il player nativo**. Il supporto HEVC di hls.js dipende dall'ambiente [7].
- **Scalabilità/CDN**: è semplice HTTP cacheabile, la strada per grandi audience. La CDN deve gestire HTTP/2 (o 3), le richieste bloccanti (collasso di richieste duplicate in attesa dello stesso riempimento di cache) e la risposta `Age` [1][2]. Fastly elenca LL-HLS tra i protocolli di Streaming Delivery [21]. Verificare sempre con una prova reale che la CDN inoltri `_HLS_msn`/`_HLS_part` e tenga aperta la richiesta.
- **Parametri chiave** (regole della bozza [1] confrontate con i default rms):

| Parametro | Regola | Default rms |
|---|---|---|
| `EXT-X-TARGETDURATION` | durata massima del segmento arrotondata | segue il GOP (2 s con keyframe a 2 s) |
| `PART-TARGET` | ogni parte <= target e >= 85% (salvo INDEPENDENT, ultima del segmento, GAP) | 0,5 s (`hls.part_duration`) |
| `PART-HOLD-BACK` | OBBLIGO >= 2x PART-TARGET, raccomandato >= 3x; obbligatorio con EXT-X-PART-INF | 3x = 1,5 s |
| `HOLD-BACK` | >= 3x TARGETDURATION | 3x (assente = 3x) |
| `CAN-SKIP-UNTIL` | >= 6x TARGETDURATION; un `EXT-X-SKIP` nella playlist richiede `EXT-X-VERSION` >= 9 | omesso se non implementato |
| `EXT-X-VERSION` | >= 6 per `EXT-X-MAP` (fMP4) | 9 |
| Blocking reload | niente risposta finché msn/part esiste; 400 se `_HLS_msn` > ultimo+2 o `_HLS_part` oltre il "limite di anticipo"; 503 dopo 3 target | 503 dopo 3x target |
| Nuova parte | entro una durata di parte dalla precedente; nuova versione playlist entro 1,5x target | da rispettare nel packager |
| Finestra playlist | non meno di 3x target; `EXT-X-PART` rimosse dopo >3 target; parti scaricabili ancora per 3 target | `window_segments` = 6 |
| `EXT-X-PRELOAD-HINT` | la risorsa deve essere disponibile alla richiesta | PART successiva |
| `EXT-X-RENDITION-REPORT` | richiesto per ogni *altra* rendition | non necessario con una sola variante |
| `EXT-X-PROGRAM-DATE-TIME` | su tutte le playlist (profilo low-latency) | presente |

- **Attenzione al PRELOAD-HINT**: per una parte non ancora completa il server **non deve trasmettere alcun byte finché la parte intera può essere inviata a piena velocità** [1]: la richiesta va tenuta in attesa e la parte inviata solo completa. Lo streaming chunked di `seg-N.m4s` in produzione (ARCHITECTURE 6.3) resta lecito se si emette una parte alla volta, ciascuna solo quando è completa; per `part-N-P.m4s` si risponde solo a parte completa. In DASH, invece, i chunk CMAF partono man mano che vengono prodotti.
- **Profilo di consegna** (appendice B.1 [1]): HTTP/2 o HTTP/3, TLS 1.3 raccomandato, playlist compresse gzip, risposte bloccanti riuscite in cache per 6 target e non bloccanti per mezzo target, GOP raccomandato 1-2 s. Se il server non rispetta il profilo il client può ripiegare su latenza normale: **servire LL-HLS dietro TLS e HTTP/2** (sezione 5.3).
- **Player**: hls.js (ultima release al controllo: 1.7.3) con `lowLatencyMode: true` (è il default; parte da `PART-HOLD-BACK`) [7]. Per recuperare ritardo senza salti: `maxLiveSyncPlaybackRate` > 1 (default 1 = disattivato, massimo 2) [7]; un valore di partenza come 1,1 va provato sul campo. In modalità LL il ritardo target è `PART-HOLD-BACK` del manifest, a meno che l'applicazione imposti `liveSyncDuration` o `liveSyncDurationCount`, che lo sovrascrivono; inoltre ogni stallo aumenta il target di `liveSyncOnStallIncrease` [7] (verificato nel sorgente del latency controller). Safari nativo: nessuna manopola, rispetta `PART-HOLD-BACK`.
- **Validazione**: `mediastreamvalidator` e `hlsreport` degli HLS Tools Apple [3].
- **Stato**: **Core**.

### 3.3 LL-DASH (CMAF low latency)

- **Latenza tipica**: dash.js indica come esempio ~3 s con chunk da 1 s [4]; fornitore: 3-5 s [23]; obiettivo rms 2-3 s.
- **Compatibilità**: nessun browser ha DASH nativo; serve dash.js (MSE + Fetch API) [4]: desktop e Android. Su iPhone non c'è player nativo e il supporto a ManagedMediaSource della versione di dash.js in uso non è stato verificato: **per iOS usare LL-HLS**.
- **Scalabilità/CDN**: i chunk CMAF sono serviti con HTTP/1.1 chunked transfer; la CDN deve inoltrarli man mano che arrivano. Gli stessi chunk servono LL-HLS e LL-DASH (`SegmentStore` condiviso, ARCHITECTURE 6.5).
- **Parametri chiave** [4][5] (le regole su `availabilityTimeOffset` vengono dalla bozza DASH-IF 2019): `availabilityTimeOffset` > 0 e < durata massima del segmento, con (durata segmento - offset) < latenza target; `availabilityTimeComplete="false"`; `ServiceDescription` con `Latency` (target/min/max) e `PlaybackRate`; `UTCTiming` per allineare l'orologio del client. L'esempio dash.js usa target 4000 ms con intervallo 2000-6000 ms [4]; ARCHITECTURE 6.4 usa target 2000, min 1500, max 6000 e `PlaybackRate` 0,96-1,04. Impostazioni dash.js [6]: `streaming.delay.liveDelay`, `streaming.liveCatchup.{enabled, mode, maxDrift, playbackRate.{min,max}}`, `streaming.applyServiceDescription`; modalità di catch-up `liveCatchupModeDefault` o `liveCatchupModeLoLP` [6]. Metriche: `getCurrentLiveLatency()`, `getTargetLiveDelay()` [6].
- **Versione**: ARCHITECTURE indica dash.js >= 4.7; la release corrente al controllo è la 5.2.1 [6]. Provare e fissare la versione con un test di playback prima di pinnarla.
- **Fonti normative**: linee guida DASH-IF "Low-latency Modes for DASH" (change request 2019, community review) e DVB-DASH [5].
- **Stato**: **Core**.

### 3.4 WebRTC / WHEP

- **Standard**: WHEP, `draft-ietf-wish-whep-04` (22 giugno 2026): Internet-Draft del WG WISH, intended status Standards Track, **nessun RFC** [10]. Flusso: POST `application/sdp` con l'offerta, `201 Created` con SDP answer e `Location`; il server può rispondere `406` con una controfferta; `PATCH` per ICE; `DELETE` per chiudere [10].
- **Latenza tipica**: sub-secondo (circa 0,5 s [23]; sotto 500 ms [21]).
- **Compatibilità**: tutti i browser moderni, iOS Safari incluso, senza MSE. Vincoli sui codec: i browser devono implementare H.264 Constrained Baseline e VP8 [11] e, per l'audio, Opus e G.711 [11]. Quindi l'AAC di OBS **va transcodificato in Opus**, e il video deve essere senza B-frame con profilo negoziato via SDP.
- **Scalabilità**: stato per viewer (ICE, DTLS, SRTP, feedback RTCP): non si appoggia a una CDN HTTP; si scala con più nodi/SFU. Servono porte UDP raggiungibili (a volte TURN). Per rms è adatta a pubblici interattivi di dimensione contenuta, non a grandi audience.
- **Player**: `RTCPeerConnection` + una `fetch` verso l'endpoint WHEP (poche righe di JS); nessuna libreria obbligatoria.
- **Crate Rust candidati** (versioni crates.io al controllo [30]): `webrtc` 0.21.0 (async, Tokio) e `str0m` 0.24.1 (stile sans-IO).
- **Stato**: **implementato** (Fase 4, ARCHITECTURE §16): `POST /whep/{name}` con ICE-lite su un'unica porta UDP (`webrtc.udp_port`), H.264 inoltrato senza transcodifica, AAC → Opus con ffmpeg; primo tentativo del player embed in modalità `auto`.

### 3.5 HESP (menzione)

High Efficiency Streaming Protocol (THEO): streaming HTTP "a frame" per bassa latenza e cambio canale rapido. La bozza `draft-theo-hesp-06` (aprile 2024) è individuale, scaduta e archiviata, senza RFC, con 4 dichiarazioni IPR [21]. Latenza: sub-secondo secondo il promotore; "1-5 secondi" secondo la documentazione Dolby OptiView [21]. Richiede packager e player dedicati e supporto CDN (range request, chunked) [21]. Non previsto in rms.

### 3.6 MoQ (Media over QUIC), futuro

Pub/sub su QUIC e WebTransport con relay. `draft-ietf-moq-transport-22` (1 ottobre 2026): WG document, intended Standards Track, nessun RFC; il milestone del WG per la richiesta di pubblicazione IESG è dicembre 2026 [22]. Il formato dei media è in `draft-ietf-moq-msf-01` (2 giugno 2026), WG document, Informational [22]. WebTransport è "Baseline 2026, newly available" da marzo 2026 secondo MDN (il dettaglio per Safari non è stato verificato) [22]. Ant Media elenca MoQ tra ingest e playback [23]. Non adatto alla produzione oggi: nessuna decisione finché i draft non diventano RFC.

### 3.7 Confronto con Ant Media Server (riferimento del progetto)

Ant Media dichiara ingest WebRTC, RTMP, SRT, RTSP, WHIP, MoQ e NDI; playback WebRTC (~0,5 s, Enterprise), MoQ, LL-HLS/LL-DASH, HLS/DASH; la Community è "WebRTC ingest only (no playback)", con latenza end-to-end 8-12 s; LL-HLS è indicato come "paid plugin" e LL-DASH (CMAF) come Enterprise [23]. rms offre FLV, LL-HLS e LL-DASH senza componenti a pagamento, a costo di un solo bitrate e senza WebRTC fino alla Fase 4.

## 4. Tabella comparativa riassuntiva

| Protocollo | Latenza (fonte) | Compatibilità | Scalabilità | Complessità | Uso consigliato |
|---|---|---|---|---|---|
| RTMP / Enhanced RTMP (ingest) | 0,8-3 s e2e [24] | OBS, ffmpeg, encoder HW | 1 publisher/stream | Bassa | Ingest predefinito; H.264+AAC |
| SRT (ingest) | buffer configurabile, default 120 ms [19] | OBS >= 28, ffmpeg | 1 publisher/stream | Media | Reti difficili, Internet pubblico (Stretch) |
| WHIP (ingest) | sub-secondo [23][21] | OBS >= 30.0 | 1 publisher/stream | Alta | Contribuzione interattiva (Roadmap) |
| RIST (ingest) | n.d. | OBS >= 27.2 | n.d. | Media | Solo ambito broadcast; non previsto |
| HTTP-FLV | 1-2 s (obiettivo); <1 s migliore caso [8] | MSE; iOS >= 17.1 con vincoli | Bassa: 1 conn./viewer | Bassa | Desktop/Android, minima latenza senza WebRTC |
| WS-FLV | come HTTP-FLV | come HTTP-FLV | Bassa | Bassa | Dove i proxy bloccano il chunked |
| LL-HLS | circa 2 s o meno (Apple) [2]; 3-5 s [23] | Safari/iOS nativo; hls.js altrove | Alta (CDN) | Media-alta | Default per pubblico generale e iOS |
| LL-DASH | ~3 s con chunk da 1 s [4] | dash.js (MSE); no iOS nativo | Alta (CDN) | Alta | Alternativa a LL-HLS dove serve DASH |
| HLS classico | >= 8-12 s [21][23] | Ovunque | Altissima | Bassa | Solo fallback |
| WHEP (WebRTC) | ~0,5 s [23] | Tutti i browser | Media (stato per viewer) | Alta | Interattività < 1 s (Stretch) |
| HESP | 1-5 s [21] | player dedicato | Alta (CDN) | Alta | Non previsto |
| MoQ | n.d. (draft) | WebTransport (2026) | in definizione | Alta | Solo monitoraggio |

## 5. Raccomandazioni pratiche per rms

### 5.1 Quale sorgente nell'embed player

| Piattaforma | Prima scelta | Ripiego | Note |
|---|---|---|---|
| Desktop Chrome / Edge / Firefox | HTTP-FLV (mpegts.js, `liveBufferLatencyChasing`) | hls.js LL-HLS, poi dash.js | LL-HLS se serve passare da CDN o proxy poco adatti al chunked |
| Android Chrome | come desktop | hls.js LL-HLS | |
| Safari macOS | LL-HLS nativo | HTTP-FLV | Proposta: ARCHITECTURE 9 sceglie FLV se mpegts.js è supportato, ma Safari desktop ha anche l'HLS nativo (AirPlay, consumi) |
| iOS / iPadOS (Safari e WebView) | **LL-HLS nativo** (`video.canPlayType('application/vnd.apple.mpegurl')`) | hls.js con ManagedMediaSource (iOS >= 17.1) [7][25] | Il controllo "non iOS" di ARCHITECTURE 9 per mpegts.js può diventare un feature test (`mpegts.getFeatureList().mseLivePlayback`), ma resta preferibile il nativo |
| Grandi audience via CDN | LL-HLS (+ LL-DASH per player non Apple) | HLS classico | FLV/WS-FLV non scalano su CDN generiche |
| Interattività < 1 s (aste, quiz, Q&A) | WHEP (Fase 4) | LL-HLS | |
| Smart TV / set-top box | HLS dal player nativo del dispositivo | | Il supporto LL-HLS varia: da verificare caso per caso |

### 5.2 Impostazioni OBS passo-passo (ingest RTMP, latenza bassa)

1. **Impostazioni > Stream**: Servizio *Personalizzato* (Custom); Server `rtmp://<host>:1935/live`; Chiave = stream key della dashboard.
2. **Impostazioni > Output**: *Modalità di output* = *Avanzata* (Advanced). La modalità Semplice non espone l'intervallo di keyframe (il sorgente non lo imposta [15]).
3. **Scheda Streaming, x264**: Controllo velocità (Rate Control) **CBR**; bitrate sostenibile dall'uplink con margine (il default di OBS 32.1 è 6000 kbps [12]); **Intervallo fotogrammi chiave (Keyframe Interval) = 2 s** (1 s per minima latenza, a costo di più bitrate); preset CPU *veryfast* o più veloce; profilo *main* o *high* (*baseline* se si prevede WHIP/WHEP); **Tune = zerolatency**; opzioni x264 `bframes=0` (già implicito in zerolatency [16]).
4. **Se si usa NVENC**: preset intermedio (es. P4, da valutare), **Tuning = Low Latency** o **Ultra Low Latency**, **Multipass = Disattivato** [12], **B-frame massimi = 0**, look-ahead disattivato se presente.
5. **Audio**: AAC 128-160 kbps, 48 kHz (compatibile con una futura conversione in Opus).
6. **Video**: frame rate costante (30 o 60), risoluzione 720p o 1080p.
7. **Impostazioni > Avanzate**: Ritardo flusso (Stream Delay) disattivato.
8. **Verifica del GOP**: l'intervallo di keyframe predefinito di OBS è 0 (auto) [15][16]. Controllare dopo l'avvio con `ffprobe -v error -select_streams v:0 -read_intervals "%+10" -show_entries packet=pts_time,flags -of csv=p=0 https://host/live/evento.flv | grep K`: i tempi devono distare 2,0 s.
9. **Variante SRT**: Servizio Personalizzato, Server `srt://host:porta?streamid=<chiave>&latency=<us>` con latency in microsecondi (>= 2,5-4x RTT [13][19]), chiave vuota [13]; opzionale `&passphrase=...`.
10. **Variante WHIP** (quando disponibile): Servizio WHIP, Server = endpoint, Bearer Token = chiave [14].

Il segmento LL-HLS di rms segue il GOP: con GOP 2 s e parti da 0,5 s si hanno 4 parti per segmento, di cui solo la prima INDEPENDENT. Con GOP > `hls.max_segment_duration` (6 s) rms divide comunque il segmento alla parte successiva.

### 5.3 TLS e reverse proxy (nginx / Caddy)

rms non termina TLS (ARCHITECTURE, non-goal). Obblighi pratici:

- **HTTPS e HTTP/2 (o 3) per LL-HLS**: il profilo low-latency lo richiede [1]; con HTTP/1.1 si incontra anche il limite di connessioni per host del browser (tipicamente 6) quando convivono playlist bloccanti e parti.
- **No buffering** sul percorso dei media; **chunked transfer** preservato; **timeout di lettura** oltre i default; **header WebSocket** per `/ws/`; **nessuna cache** delle playlist e **nessuna compressione** dei segmenti.
- `server.trust_proxy = true` e `X-Forwarded-For` per i conteggi dei viewer (ARCHITECTURE 4.4).

```nginx
server {
  listen 443 ssl; http2 on; server_name tv.example.com;     # certificati e TLS 1.3 omessi
  location / {
    proxy_pass http://127.0.0.1:8080;
    proxy_http_version 1.1;               # default 1.1 solo da nginx 1.29.7, prima era 1.0 [26]
    proxy_set_header Connection "";
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_buffering off;                  # default on: nginx bufferizza la risposta [26]
    proxy_cache off;
    proxy_read_timeout 3600s;             # default 60 s [26]
    gzip off;                             # solo i .m3u8/.mpd possono essere compressi
  }
  location /ws/ {
    proxy_pass http://127.0.0.1:8080;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade"; # Upgrade/Connection sono hop-by-hop [26]
    proxy_read_timeout 3600s;              # chiude dopo 60 s senza dati di default [26]
  }
}
```

```caddy
tv.example.com {
  reverse_proxy 127.0.0.1:8080 {
    flush_interval -1        # modalità low-latency: niente buffering, flush a ogni scrittura [27]
  }
}
```

- Caddy abilita di default `h1 h2 h3` lato client [27] e promuove da solo i WebSocket [27]; `flush_interval -1` è il parametro decisivo per FLV e segmenti chunked.
- In alternativa a `proxy_buffering off` si può far inviare a rms `X-Accel-Buffering: no` (nginx lo rispetta [26]).
- **RTMP/SRT non passano da un proxy HTTP**: aprire `1935/tcp` (RTMP) e la porta UDP SRT. Per RTMPS (la stream key non viaggia in chiaro) usare un listener TLS davanti a 1935, ad esempio il modulo `stream` di nginx (`ngx_stream_ssl_module`) con `ssl_certificate` e `proxy_pass 127.0.0.1:1935`; OBS accetta server `rtmps://`.
- Con una CDN davanti a LL-HLS: deve inoltrare i parametri di query `_HLS_msn`/`_HLS_part`, tenere aperte le richieste bloccanti e collassare le duplicate; TTL suggeriti dalla bozza [1]: 6 target per le risposte bloccanti riuscite, mezzo target per quelle non bloccanti.

### 5.4 Misura della latenza (timecode burn-in)

Le metriche interne dei player (`hls.latency`/`hls.targetLatency` [7]; dash.js `getCurrentLiveLatency()` [6]) misurano solo dal bordo live in poi: non includono encoder e ingest. Per il glass-to-glass si usa un orologio impresso nel video.

1. **Sorgente di test**: `drawtext` supporta `%{localtime}` con frazioni di secondo `%3N` [29] e imprime l'ora a cui il filtro elabora il frame; il comando seguente è stato eseguito con l'ffmpeg locale:

```sh
ffmpeg -re -f lavfi -i "testsrc2=s=1280x720:r=30" -f lavfi -i "sine=frequency=440:sample_rate=48000" \
  -vf "drawtext=text='%{localtime\:%T.%3N}':fontsize=72:fontcolor=white:box=1:boxcolor=black@0.6:x=20:y=20" \
  -c:v libx264 -preset veryfast -tune zerolatency -profile:v main -b:v 3000k -maxrate 3000k -bufsize 3000k \
  -g 60 -keyint_min 60 -sc_threshold 0 -c:a aac -b:a 128k -ar 48000 -f flv rtmp://localhost:1935/live/<chiave>
```

   Con OBS: aggiungere una Sorgente Browser su una pagina locale con un orologio in millisecondi (`setInterval` su `new Date()`).
2. **Lettura**: encoder e viewer sullo stesso PC (o orologi sincronizzati con NTP/chrony). Tenere affiancati il player e un orologio di riferimento (ad esempio `watch -n0.05 date +%T.%3N`), fare uno screenshot: latenza = orologio di riferimento - orologio impresso.
3. **Campione**: almeno 10 misure per protocollo, a freddo (appena avviato il player) e a regime; riportare mediana e massimo. Confrontare FLV, LL-HLS e LL-DASH in parallelo sullo stesso flusso.
4. **Cosa guardare**: una latenza vicina a `HOLD-BACK` (3x `TARGETDURATION` [1]) indica che il client non sta usando la modalità low-latency: controllare HTTP/2, `PART-HOLD-BACK` e blocking reload.

### 5.5 Note di allineamento con ARCHITECTURE.md

- 6.3 afferma che il target del segmento segue il keyframe di OBS "default 2 s": in realtà il default è 0 (auto, GOP fino a 250 frame con scenecut) [15][16]. La dashboard dovrebbe mostrare un avviso quando il GOP misurato supera 2-3 s.
- 6.3: per le parti (PRELOAD-HINT) il server invia solo parti complete [1] (vedi 3.2).
- 9: mpegts.js gira su iPhone da iOS 17.1 [8]: oltre a "non iOS" si può usare un feature test, preferendo comunque il nativo.
- 2: dash.js >= 4.7 indica una major precedente alla corrente (5.2.1) [6]; ultime release di hls.js e mpegts.js al controllo: 1.7.3 e 1.8.2 [7][8].
- HEVC/AV1 in ingresso non hanno percorso LL-HLS/LL-DASH (1, non-goal): documentare che il flusso consigliato è H.264+AAC.

## 6. Roadmap

Ordine proposto, per rapporto valore/costo:

1. **Consolidare LL-HLS/LL-DASH**: validazione con `mediastreamvalidator` [3], test dietro nginx/Caddy con HTTP/2, test con una CDN reale, delta update (`CAN-SKIP-UNTIL` >= 6x target [1]) se la banda delle playlist pesa. Prerequisito di tutto il resto.
2. **WHEP egress (Fase 4)**: endpoint `POST /whep/{name}` con `201 Created` + `Location` e `DELETE` [10]; crate `webrtc` o `str0m` [30]; video H.264 senza B-frame inoltrato come RTP senza transcodifica; audio AAC verso Opus (decodifica AAC + codifica Opus, con ffmpeg come processo ausiliario o con librerie native); porta/range UDP e IP pubblico/ICE; metriche `Protocol::WebRtc`. Seguire l'evoluzione della bozza (versione 04, non RFC [10]).
3. **SRT nativo**: sostituire il ponte ffmpeg con un listener libsrt/`srt-tokio` (ultima release 0.4.4, maggio 2024 [30]; valutare la manutenzione) che legga lo `streamid` (max 512 caratteri) per autenticare per broadcast e imponga la passphrase [19]; latenza configurabile per broadcast.
4. **WHIP ingest**: RFC 9725 [9]; riusa lo stack WebRTC della WHEP; audio Opus da riconvertire in AAC per HLS/FLV (mpegts.js riproduce Opus in HTTP-FLV da 1.8.0 [8]); autenticazione con Bearer = stream key; OBS >= 30.0 [12].
5. **ABR e transcoding**: ladder con ffmpeg/GPU, GOP allineati tra rendition, master playlist con più varianti e `EXT-X-RENDITION-REPORT` per ogni altra rendition, rendition aggiornate entro una durata di parte [1]. Costo CPU/GPU elevato: opzionale per broadcast.
6. **Packaging CMAF HEVC/AV1**: oggi solo FLV; Safari riproduce HEVC fMP4 in LL-HLS, ma il supporto varia per browser e hardware.
7. **MoQ**: monitorare `draft-ietf-moq-transport` e MSF [22]; riconsiderare quando esisterà un RFC e un supporto stabile nei browser.

## 7. Fonti

Verificate l'8 ottobre 2026 (consultazione diretta di pagine, sorgenti o API; dove la pagina non era accessibile è indicato nel testo).

1. IETF, *HTTP Live Streaming 2nd Edition*, `draft-pantos-hls-rfc8216bis-22`, 1 maggio 2026: https://datatracker.ietf.org/doc/draft-pantos-hls-rfc8216bis/ (testo: https://www.ietf.org/archive/id/draft-pantos-hls-rfc8216bis-22.txt). Sezioni 4.4.3.7-4.4.3.8, 4.4.4.9, 4.4.5.3-4.4.5.4, 6.2.5-6.2.6, appendici B.1 e C.
2. Apple, WWDC20 sessione 10228 *Low-Latency HLS*: https://developer.apple.com/videos/play/wwdc2020/10228/
3. Apple, *Using Apple's HTTP Live Streaming (HLS) Tools*: https://developer.apple.com/documentation/http-live-streaming/using-apple-s-http-live-streaming-hls-tools
4. dash.js, *Low Latency Streaming*: https://dashif.org/dash.js/pages/usage/low-latency.html
5. DASH-IF, change request *Low-latency Modes for DASH* (2019): https://dashif.org/docs/DASH-IF-IOP-CR-Low-Latency-Live-Community-Review.pdf ; DASH-IF Restricted Timing Model: https://dashif.org/Guidelines-TimingModel/
6. dash.js, definizioni TypeScript e release: https://github.com/Dash-Industry-Forum/dash.js (`index.d.ts`, branch development)
7. hls.js, README e API: https://github.com/video-dev/hls.js ; https://github.com/video-dev/hls.js/blob/master/docs/API.md
8. mpegts.js, README e documenti `api.md`, `livestream.md`: https://github.com/xqq/mpegts.js
9. IETF, RFC 9725 *WebRTC-HTTP Ingestion Protocol (WHIP)*, marzo 2025: https://www.rfc-editor.org/rfc/rfc9725.html
10. IETF, *WebRTC-HTTP Egress Protocol (WHEP)*, `draft-ietf-wish-whep-04`: https://datatracker.ietf.org/doc/draft-ietf-wish-whep/
11. IETF, RFC 7742 (codec video WebRTC): https://www.rfc-editor.org/rfc/rfc7742.html ; RFC 7874 (codec audio WebRTC): https://www.rfc-editor.org/rfc/rfc7874.html
12. OBS Studio, note di rilascio (GitHub Releases, versioni 27.2, 28.0, 28.1, 29.0, 29.1, 30.0-30.2, 31.x, 32.x): https://github.com/obsproject/obs-studio/releases
13. OBS Knowledge Base, *SRT Protocol Streaming Guide*: https://obsproject.com/kb/srt-protocol-streaming-guide
14. OBS Knowledge Base, *WHIP Streaming Guide*: https://obsproject.com/kb/whip-streaming-guide
15. OBS Studio, sorgenti `plugins/obs-x264/obs-x264.c`, `plugins/rtmp-services/rtmp-custom.c`, `plugins/obs-outputs/rtmp-stream.c`, `SimpleOutput.cpp`: https://github.com/obsproject/obs-studio
16. x264, `common/base.c` (default `keyint` 250, tune `zerolatency`): https://code.videolan.org/videolan/x264/-/blob/master/common/base.c
17. Veovera, *Enhanced RTMP* (v1, v2): https://github.com/veovera/enhanced-rtmp
18. `rtmp-rs` 0.6.0, README: https://crates.io/crates/rtmp-rs
19. SRT: IETF `draft-sharabayko-srt-01`: https://datatracker.ietf.org/doc/draft-sharabayko-srt/ ; Haivision, *Latency*: https://doc.haivision.com/SRT/1.5.4/Haivision/latency ; libsrt, opzioni socket: https://github.com/Haivision/srt/blob/master/docs/API/API-socket-options.md ; FFmpeg, protocollo srt: https://ffmpeg.org/ffmpeg-protocols.html
20. Video Services Forum, raccomandazioni TR-06 (RIST): https://www.vsf.tv/technical_recommendations.shtml ; descrizione del trasporto: https://en.wikipedia.org/wiki/Reliable_Internet_Stream_Transport
21. HESP: https://datatracker.ietf.org/doc/draft-theo-hesp/ ; Dolby OptiView, *Streaming protocols*: https://optiview.dolby.com/docs/theolive/media-engine/streaming-protocols/ ; Fastly, *Streaming Delivery*: https://docs.fastly.com/products/fastlys-streaming-delivery
22. MoQ: https://datatracker.ietf.org/doc/draft-ietf-moq-transport/ ; https://datatracker.ietf.org/doc/draft-ietf-moq-msf/ ; MDN, *WebTransport API*: https://developer.mozilla.org/en-US/docs/Web/API/WebTransport_API
23. Ant Media Server, documentazione: https://docs.antmedia.io/ ; funzionalità Enterprise: https://antmedia.io/ant-media-server-enterprise-edition-features/
24. SRS, wiki *low latency*: https://github.com/ossrs/srs/wiki/migrate_v4_EN_low-latency
25. WebKit, *WebKit Features in Safari 17.1* (ManagedMediaSource su iPhone): https://webkit.org/blog/14735/
26. nginx: https://nginx.org/en/docs/http/websocket.html ; https://nginx.org/en/docs/http/ngx_http_proxy_module.html
27. Caddy: https://caddyserver.com/docs/caddyfile/directives/reverse_proxy ; https://caddyserver.com/docs/caddyfile/options
28. webrtcHacks, *WebRTC cracks the WHIP on OBS*: https://webrtchacks.com/webrtc-cracks-the-whip-on-obs/
29. FFmpeg, filtro `drawtext`: https://ffmpeg.org/ffmpeg-filters.html#drawtext
30. crates.io (API, versioni al 8/10/2026): https://crates.io/crates/webrtc ; https://crates.io/crates/str0m ; https://crates.io/crates/srt-tokio
31. YouTube, *Understand live streaming latency*: https://support.google.com/youtube/answer/7444635

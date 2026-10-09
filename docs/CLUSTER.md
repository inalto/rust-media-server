# Esecuzione multi-istanza: studio e architettura

> **Stato: studio e proposta; fasi 1–2 di §3.12 realizzate in 7C-1** (backend PostgreSQL, registro dei nodi, `/healthz`, prerequisiti: ARCHITECTURE.md §20). Il resto non è ancora implementato (hub in memoria, nessun relay fra nodi). Verificato il 2026-10-09. Convenzioni: **[V]** fatto verificato su fonte (numero tra parentesi quadre, elenco in §8), **[S]** stima da misurare, **[P]** proposta di progetto; le citazioni tra «» sono traduzioni nostre, quelle in inglese sono letterali. Riferimenti al codice: `docs/ARCHITECTURE.md` (§4 hub e tracker, §5 ingest, §6 egress, §7 schema, §14 packager, §16 WebRTC) e `docs/API.md` §6. Setup di Nginx Proxy Manager: `deploy/nginx-proxy-manager.md`.

## 0. Sintesi

**Raccomandazione.** Control plane su **un solo PostgreSQL**: cataloghi e impostazioni, registro dei nodi con heartbeat, proprietà degli stream, code di job con `FOR UPDATE SKIP LOCKED`, metriche per nodo aggregate in SQL; `LISTEN/NOTIFY` solo come *segnale di invalidazione*. Data plane **origin → edge**: l'edge tira in modo lazy l'HTTP-FLV dall'origin (`/internal/relay/{name}.flv`) nel proprio hub. File su volume condiviso (o indice + fetch interno). Davanti: HAProxy come LB (NPM resta il terminatore TLS), ingest RTMP su hostname dedicato o TCP LB. SQLite resta per il single-node. Redis o NATS non servono finché i nodi sono pochi (≲ 10) e gli eventi rari (transizioni di stato, non campioni).

**Cosa del piano preliminare è sbagliato o rischioso** (dettagli nelle sezioni indicate):

| # | Ipotesi del piano | Esito | Motivo e fonte |
|---|---|---|---|
| 1 | «Nessuna sticky session: SSE e WS-FLV sono per connessione» | Vero per FLV, WS-FLV, SSE, `POST /whep`. **Falso per LL-HLS/LL-DASH** (e per `DELETE /whep/...`) | Il packager ancora il tempo all'arrivo del primo keyframe e numera i segmenti dal secondo Unix di quel keyframe (ARCH §14): due nodi, anche con lo stesso flusso relayed, producono MSN, `PROGRAM-DATE-TIME` e URL diversi. Un client che cambia nodo a metà rischia di rompersi (reload del player) e i contatori si gonfiano (§3.5, §3.6) |
| 2 | «Un solo codice SQL per SQLite e Postgres con `sqlx::Any`» | Rischioso | `Any` ha 9 tipi, non traduce i placeholder, non include `PgListener`; il SQL attuale usa `INSERT OR REPLACE`, `strftime`, `julianday`, `MAX(a,b)` [7][8] (§2.2) |
| 3 | «Eventi via NOTIFY» | Solo come invalidazione | Non durevole: le notifiche perse durante una disconnessione non si recuperano [1][6] |
| 4 | «S3-compatibile, es. MinIO» | **MinIO community è archiviato** (read-only dal 25/04/2026) | [32] (§6) |
| 5 | NPM: `proxy_set_header Connection ""` | Inefficace nell'Advanced tab e, se funzionasse, romperebbe WS-FLV | Template NPM 2.16.0, regola di ereditarietà di `proxy_set_header` [39][40] |
| 6 | NPM «con upstream multipli via config custom» | **Non fattibile** | Il template NPM scrive sempre `host:porta` nel `proxy_pass`; con una porta esplicita nginx non riconosce un gruppo `upstream` (variabili: passa al resolver DNS; `proxy_pass` statico: errore) [39][40] (§4) |
| 7 | `trust_proxy` e primo elemento di `X-Forwarded-For` | **Falsificabile dietro NPM** | NPM manda `$proxy_add_x_forwarded_for` (accoda all'header del client), rms leggeva il *primo* elemento (`metrics::client_ip`). HAProxy: «il server deve usare l'ultima occorrenza» [30]. **Corretto in 7C-1:** `X-Real-IP`, poi l'ultimo elemento |
| 8 | `/internal/*` protetto da shared secret | Insufficiente da solo | NPM/HAProxy inoltrano `/`: le rotte interne sarebbero raggiungibili da Internet. Listener separato + blocco sul proxy (§3.1) |
| 9 | «Redis già presente su questo host» | Non risolve il problema | È 6.2.22 su `127.0.0.1:6379` (supportato fino al 2027-04-01 [15]) e non sostituisce il database SQL: sarebbero due dipendenze (§2.3) |
| 10 | Relay +0,1–0,5 s | Limite superiore plausibile | In LAN [S] decine di ms; il costo vero è l'avvio a freddo del primo spettatore su un edge (§2.6) |

Altre lacune emerse leggendo il codice: `admin_token` generato per nodo (`data/admin_token`), `webrtc.public_ip` senza `public_port`, requeue dei job VOD senza proprietario, inbox VOD scansionato da ogni nodo (§1).

## 1. Cosa cambia con più istanze

| Tema | Oggi (single-node) | Con N nodi | Risposta |
|---|---|---|---|
| Ingest | Un listener RTMP; il secondo publisher con la stessa chiave è rifiutato dall'hub in memoria | OBS apre TCP 1935 su **un** nodo (LB TCP o hostname dedicato); gli spettatori HTTP arrivano su **qualunque** nodo | Proprietà dello stream in DB + relay (§3.3, §3.4) |
| Hub | `LiveStream` e GOP cache nel processo | Un nodo senza il publisher risponde `404 stream offline` | Pull lazy dall'owner (§3.4) |
| Control plane | SQLite locale, `HubEvent` in-process, SSE locale, `admin_token` per nodo | Cataloghi divergenti, dashboard cieca agli eventi altrui, login diverso per nodo | Postgres + NOTIFY, `RMS_ADMIN_TOKEN` identico (§3.1) |
| Metriche | Tracker e sampler per processo; sessioni HLS/DASH chiave `(ip, ua, name, proto)` | Un utente HLS servito da più nodi è contato più volte; `viewer_samples` senza `node_id` | `node_id` nei campioni, MAX per nodo poi SUM; stickiness per IP (§3.6) |
| HLS/DASH | Un packager per stream, timeline ancorata all'arrivo | Nodi diversi producono playlist e segmenti diversi | §3.5 |
| File | `data/recordings`, `data/waiting`, `data/vod` locali, serviti da `/waiting/*` e dai download | Il file esiste solo sul nodo che l'ha scritto | Volume condiviso o indice + fetch interno (§3.7, §6) |
| WebRTC | Una porta UDP, ICE-lite, candidati host = `public_ip` o interfacce, sessioni in memoria | NPM e LB HTTP non proxano UDP; il candidato è del nodo che ha risposto al POST; `DELETE` su un altro nodo → 404 | IP e porta per nodo, instradamento per sessione (§3.8) |
| Job | VOD: CAS SQL `queued→processing` (`db/vod.rs`), remux MP4 in-process | Il CAS regge la concorrenza ma manca lease: un nodo morto lascia `processing` per sempre; il requeue «shutdown» non conosce il proprietario | Lease + `SKIP LOCKED`, output atomico (§3.7) |
| Periodici | Retention giornaliera, chiusura sessioni orfane all'avvio | Eseguiti N volte; il riavvio di un nodo chiude sessioni vive di altri | Task singleton con lease; filtro per `node_id` |
| Schema | `PRAGMA user_version` | N nodi migrano insieme | Migrazioni con lock (sqlx `Migrator` ha `set_locking` [9]) + expand/contract |
| Orologi | Uno | PDT HLS, `/dash/time` e TTL dipendono dall'ora | chrony ovunque; heartbeat e lease con `now()` del DB |

## 2. Tecnologie per la comunicazione tra istanze

### 2.1 Crate e server: versioni (crates.io, 2026-10-09 [10])

| Crate | Ultima | Data | Download | Note di maturità |
|---|---|---|---|---|
| `sqlx` | 0.9.0 | 2026-05-21 | 161 M | async, Postgres/MySQL/SQLite; MSRV 1.94 [9]; 0.9.0 dopo un'alpha di ottobre 2025 |
| `tokio-postgres` | 0.7.18 | 2026-06-12 | 74 M | basso livello; i NOTIFY arrivano come `AsyncMessage::Notification` dalla `Connection` |
| `redis` | 1.7.1 | 2026-09-25 | 112 M | tokio, pub/sub, streams, cluster, sentinel, TLS; cita Valkey [18] |
| `async-nats` | 0.50.0 | 2026-07-20 | 49 M | client ufficiale (Core, JetStream, KV); «API stabile ma 0.x» [20] |
| `chitchat` | 0.13.0 | 2026-08-06 | 0,26 M | gossip Scuttlebutt di Quickwit; tre minor in tre mesi: API in movimento |
| `foca` | 2.0.0 | 2026-08-02 | 0,24 M | SWIM; il trasporto è a carico vostro |
| `mdns-sd` | 0.21.5 | 2026-10-05 | 6 M | solo multicast, «non unicast» (docs.rs) |
| `tonic` | 0.14.6 | 2026-05-07 | 420 M | gRPC; richiede protoc/prost |
| `object_store` | 0.14.2 | 2026-09-15 | 94 M | S3/GCS/Azure/locale |

Server: PostgreSQL 18 è l'ultima major (18.6); la 14 termina il 12/11/2026 [11]. Redis 6.2 è supportato fino al 2027-04-01 [15].

### 2.2 Solo PostgreSQL (raccomandato)

**Uso.** Discovery: tabella `nodes`, UPSERT ogni `heartbeat_secs`, «vivo» = `heartbeat > now() - node_ttl` con l'ora del DB. Eventi: `NOTIFY rms_*` con payload minimo (`{"name":"x","rev":42}`): il ricevente rilegge la riga. Stato condiviso: tutte le tabelle di ARCH §7 più `vod_items`. Code: `UPDATE … WHERE id = (SELECT … FOR UPDATE SKIP LOCKED LIMIT 1) RETURNING *`. Lock: advisory *transazionali* per i task singleton.

**NOTIFY, fatti verificati [V].**
- Payload «minore di 8000 byte» nella configurazione di default; coda da 8 GB; se è piena le transazioni che fanno `NOTIFY` **falliscono al commit**. La consegna avviene solo dopo il commit e solo fra transazioni; notifiche identiche nella stessa transazione si fondono; l'ordine segue quello dei commit [1].
- Non è durevole. `sqlx::PgListener` si riconnette e si re-iscrive, ma «any received while the connection was lost, will not be returned» [6]. Conseguenza di progetto: ogni listener fa un **resync completo** a ogni (ri)connessione e un polling di sicurezza (es. ogni 30 s su `rev`/`updated_at`).
- Scala: `NOTIFY` prende un lock globale al commit; un incidente di Recall.ai ha serializzato i commit; il blog riporta una correzione nel core (commit linkato, versione non indicata) e un dibattito sulla causa (thundering herd con molti listener) [5]. Per rms (decine di eventi al minuto, ≲ 10 listener) è irrilevante, ma **non** va usato per i campioni spettatori (uno ogni 5 s per stream): quelli passano da SQL.
- Latenza: nessun dato ufficiale; [S] pochi ms in LAN, da misurare.
- PgBouncer in *transaction pooling*: `LISTEN` «Never» e advisory lock di sessione «Never» [4]. Il listener usa una connessione diretta; i lock sono `pg_try_advisory_xact_lock` [3].

**Code.** `SKIP LOCKED` «fornisce una vista inconsistente dei dati, quindi non è adatto a uso generale, ma evita la contesa fra più consumatori di una tabella-coda» [2]: è esattamente il caso d'uso. Garanzia reale: *at-least-once* con output idempotente (scrittura in directory temporanea + `rename` atomico), non «exactly once».

**`sqlx::Any` [V].** `AnyTypeInfoKind` = Null, Bool, SmallInt, Integer, BigInt, Real, Double, Text, Blob [7]: niente timestamp, uuid, json, array. Il driver si sceglie dall'URL e senza `install_default_drivers()` ogni uso va in panic [7]. `Any` non riscrive il SQL: Postgres vuole `$N`; `sqlx-sqlite` mappa `$N` al valore N-esimo e accetta anche `?` e `?N` [8], quindi **solo `$N`** funziona su entrambi. Le macro `query!` richiedono un database concreto per la verifica a compile time: con `Any` non c'è un solo backend (inferenza [S], non trovata una fonte esplicita). Il gap di dialetto reale in `src/db/`: `INSERT OR REPLACE` (→ `ON CONFLICT DO UPDATE`), `strftime('%s', ts)` e `julianday` (→ colonna `ts_epoch BIGINT`, la `/` fra interi è intera in entrambi), `MAX(a, b)` scalare (→ `CASE`), booleani come `INTEGER` 0/1 (non `BOOLEAN`). NOTIFY, `SKIP LOCKED`, advisory lock e `now()` sono solo Postgres e non passano da `Any` (`PgListener` è un tipo concreto).

| Strada | Come | Costo | Rischio |
|---|---|---|---|
| A. `sqlx::Any` | Un solo set di query `$N` su sottoinsieme portabile; rusqlite sparisce | Riscrittura async di `src/db/` (2559 righe, 55 `Db::call`) | Cambia il motore del single-node; tipi limitati; query di metriche comunque divise |
| **B. Trait `Store` con due backend** | `SqliteStore` = codice attuale spostato dietro il trait; `PgStore` con `sqlx-postgres` concreto | Stesse 2559 righe da isolare + nuovo `PgStore`; più codice | Basso per il single-node; ogni query esiste due volte (test su entrambi) |
| C. Solo Postgres | Anche per il single-node | Minimo | Perde «zero dipendenze»: contraddice README |

Raccomandata la **B**: il single-node non cambia comportamento, le parti cluster (`nodes`, `live_streams`, NOTIFY, code) esistono solo in `PgStore` e hanno un'implementazione banale (no-op/in-process) in `SqliteStore`.

**Costo e guasti.** Un Postgres in più (su questo host non c'è: nessun `psql`, nessuna porta 5432); immagine `postgres:18`. DB giù → control plane giù (admin, nuove chiavi non in cache, impostazioni) ma il data plane continua se i `broadcasts` sono in cache in memoria (già «cache candidate» in ARCH §15). Failover del DB → connessioni resettate: pool e listener si riconnettono, resync. Nodo isolato dal DB → non rinnova l'heartbeat: gli altri lo danno per morto dopo `node_ttl` e **il nodo deve fermare i publisher locali** (auto-fencing) quando non riesce a rinnovare oltre il TTL.

### 2.3 Redis (e Valkey)

- **Uso.** Presenza: `SET node:{id} … EX 15`. Eventi: pub/sub, che è *at-most-once*: se il subscriber è scollegato il messaggio è perso per sempre [12]. Job: Streams con consumer group (`XREADGROUP`, `XACK`, `XAUTOCLAIM`, at-least-once) [13]. Lock: `SET k v NX PX` [14].
- **Limiti [V].** Con replica asincrona un failover può far acquisire lo stesso lock a due client [14]: servono fencing token. Redlock è oggetto di una controversia documentata nella stessa pagina [14].
- **Su questo host:** Redis 6.2.22, solo `127.0.0.1`. Per più host servono bind di rete, ACL/TLS e un'istanza dedicata (non condivisa con altre app).
- **Valkey** è un fork BSD-3 sotto LF Projects [17]; il crate `redis` lo cita nella documentazione [18] (compatibilità delle feature usate da verificare). Redis 8 è tri-licenza RSALv2/SSPLv1/AGPLv3 [16].
- **Limite strutturale:** Redis non sostituisce lo SQL (cataloghi, `SUM` per bucket temporale, sessioni). Con Redis si avrebbero **due** dipendenze (Redis + SQL condiviso), mai una. Ha senso solo con molti nodi o con eventi ad alta frequenza, o se Redis è già in infrastruttura e il team lo preferisce per presenza/pub-sub.

### 2.4 NATS / JetStream

- **Uso.** Subject `rms.stream.started.<name>` (Core NATS: at-most-once [19]); request-reply per `kick`/status al posto di HTTP interno; JetStream stream + consumer per i job (redelivery se manca l'ack, at-least-once [19]); KV con TTL per chiave e CAS sulla revision per presenza e ownership [19]; clustering integrato.
- **Crate:** `async-nats` 0.50.0, ufficiale, API «stabile» ma 0.x e dipendenze 0.x (rustls) [20].
- **Costo:** un servizio in più; JetStream replicato vuole un numero dispari di server, 3 per R3, e senza maggioranza i flussi smettono di accettare scritture [19]. Non sostituisce lo SQL. Ottimo per decine di nodi o più regioni; sproporzionato qui.

### 2.5 Gossip, mDNS, DNS (solo discovery)

- `chitchat`: membership + failure detector (phi accrual) + KV per nodo su UDP; i nodi morti restano nello stato per una grace period (24 h di default) [21]. `foca`: SWIM, trasporto a carico vostro. `mdns-sd`: solo multicast su LAN.
- DNS: il nome di un servizio Compose scalato può restituire più IP (round-robin, comportamento da verificare su Podman/aardvark); Swarm espone `tasks.<servizio>` e `endpoint_mode: dnsrr` [23]; un Service Kubernetes headless restituisce A/AAAA dei pod pronti, senza LB [22].
- Danno **solo** la lista dei nodi: niente stato condiviso, code o storage. Con Postgres la discovery è già la tabella `nodes`; il DNS serve all'LB (`server-template` di HAProxy), non a rms. Guasti: le partizioni producono viste diverse per nodo; il multicast di norma non attraversa overlay e VPC cloud [S].

### 2.6 HTTP/gRPC interno e relay dei media

**Controllo.** Axum `/internal/*` con segreto condiviso (confronto a tempo costante) contro `tonic`: gRPC porta protoc/prost per 4-5 chiamate (kick, ping, avvio relay, fetch file): HTTP semplice basta. Sicurezza: listener separato (`cluster.internal_bind`), mai dietro NPM; blocco di `/internal/` anche sul proxy.

| Relay | Come | Pro | Contro |
|---|---|---|---|
| **a. HTTP-FLV pull** (proposto) | L'edge fa `GET /internal/relay/{name}.flv`, riusa l'egress FLV esistente | Lazy; la GOP cache dà avvio immediato; nessun nuovo server | Serve un client HTTP in streaming (oggi assente da `Cargo.toml`: `reqwest` o `hyper-util`); head-of-line TCP in caso di perdita |
| b. RTMP | `rtmp-rs` 0.6 ha già `RtmpPuller`/`RtmpPublisher` (README del crate) ma rms non implementa il `play` RTMP; il *push* verso l'ingest dell'edge (come `push` di nginx-rtmp [27]) è fattibile ma eager: copia sempre, anche senza spettatori | Nessuna nuova dipendenza | Banda sprecata; l'ingest deve distinguere le chiavi di relay |
| c. SRT | Utile su WAN con perdita | | Oggi nemmeno l'ingest SRT è nativo (bridge ffmpeg) |
| d. WebRTC cascading | WHEP fra nodi | Latenza minima | Client WHEP + ICE fra nodi: complessità alta, fuori scope |

**Latenza e banda [S].** Il relay aggiunge un hop TCP: circa RTT/2 più un flush, quindi decine di ms in LAN e al massimo qualche centinaio di ms su WAN. L'avvio a freddo (primo spettatore su un edge) costa una richiesta HTTP più lo scarico della GOP cache (≤ 4 s / 4 MB, ARCH §4.3) a velocità di rete. Banda: una copia per edge e per stream *con spettatori*; al massimo (N−1) × bitrate sull'origin (4 edge × 6 Mbit/s = 24 Mbit/s).

### 2.7 Come lo risolvono gli altri

| Prodotto | Modello | Stato condiviso | Relay |
|---|---|---|---|
| Ant Media (Enterprise) | Origin (ingest) ed edge (playback): ruoli assegnati dall'instradamento del LB [24] | MongoDB **o Redis**: l'edge cerca l'origin dello stream nel DB [24] | L'edge tira lo stream sulla rete interna (porta 5000 aperta fra i nodi); il cluster è solo Enterprise [24] |
| SRS | Origin cluster: l'origin interroga i `coworkers` (HTTP API) e rimanda il client con RTMP302 [25] | Nessun DB: lista statica di `coworkers`/`origin` in config | Edge `mode remote` con elenco di origin, pull; cluster solo RTMP, FLV tramite edge [25] |
| MediaMTX | Il README non documenta funzioni di cluster [26] | Nessuno | «Proxy requests»: path regex con `source` verso un altro server e `sourceOnDemand`, relay lazy statico [26] |
| nginx-rtmp | `pull` (parte quando c'è un player locale) e `push` fra server [27] | Nessuno | Statico in config |

Lezione: tutti usano origin/edge con pull dall'edge (esplicitamente lazy in MediaMTX e nginx-rtmp); solo Ant Media ha un registro condiviso. rms = registro SQL (come Ant Media) + relay semplice (come SRS/MediaMTX).

### 2.8 Confronto

| Opzione | Dipendenze nuove | Discovery | Eventi | Stato condiviso | Lock / code | Latenza eventi | Complessità | Fit per rms |
|---|---|---|---|---|---|---|---|---|
| **Solo Postgres** | 1 (Postgres) | Tabella `nodes` | NOTIFY, best-effort | Sì (SQL, metriche con `SUM`) | `SKIP LOCKED`, advisory xact | ms in LAN [S] | Media (riscrittura `db/`) | **Alto** |
| Redis + SQL | Redis **e** Postgres | Chiavi con TTL | Pub/sub at-most-once o Streams | Cache/KV, non SQL | `SET NX`, Streams | < ms [S] | Media | Medio |
| NATS + SQL | NATS **e** Postgres | KV con TTL | Subject / JetStream | KV | JetStream | < ms [S] | Alta | Basso |
| Gossip / DNS / mDNS | 0 | Sì | No (KV gossip) | No | No | Secondi (round di gossip) [S] | Bassa ma incompleta | Solo discovery |
| HTTP interno | 0 | Da config / DB | No | No | No | RTT diretto | Bassa | Sì, per controllo e relay |
| SQLite su NFS | 0 | — | — | «Tutti i processi sullo stesso host; WAL non funziona su filesystem di rete» [31] | — | — | — | **No** |
| rqlite | rqlite | — | No | Sì, ma solo API HTTP/JSON, nessun driver SQLite [33] | — | — | Alta (riscrittura) | Basso |

### 2.9 Raccomandazione

Postgres come unica dipendenza esterna, HTTP interno per controllo e relay FLV. Aggiungere **Redis** se: i nodi superano ≈10, servono contatori/presenza ad alta frequenza (aggiornamenti per secondo), o Redis è già gestito. Aggiungere **NATS** se: più regioni, o un bus di eventi con replay e fan-out grande. In entrambi i casi Postgres resta, perché catalogo e metriche sono relazionali.

## 3. Architettura proposta per rms

### 3.1 Configurazione [P]

```toml
[cluster]
enabled = false            # false = single-node (SQLite, nessun comportamento nuovo)
node_id = ""               # univoco e stabile; default: hostname (RMS_CLUSTER_NODE_ID)
database_url = ""          # postgres://rms:***@db:5432/rms (RMS_CLUSTER_DATABASE_URL)
advertise_url = ""         # URL interno del nodo, raggiungibile dagli altri: http://rms1:8081
internal_bind = "0.0.0.0:8081"   # listener /internal/* separato da quello pubblico (aggiunta rispetto al brief)
internal_secret = ""       # ≥ 32 byte casuali, uguale su tutti i nodi
heartbeat_secs = 5
node_ttl_secs = 15         # nodo/stream considerati morti dopo (aggiunta); 10 = failover più rapido
relay_idle_secs = 30       # ferma il relay senza spettatori locali (aggiunta)
```

In modalità cluster `server.admin_token` è **obbligatorio e identico** su tutti i nodi (il file `data/admin_token` è per nodo). `server.public_url` uguale ovunque.

### 3.2 Schema (nuove tabelle e differenze rispetto ad ARCH §7, Postgres) [P]

Nel `PgStore` le tabelle esistenti nascono già con queste colonne (gli `ALTER` sono solo per leggere le differenze).

```sql
CREATE TABLE nodes (
  node_id TEXT PRIMARY KEY, advertise_url TEXT NOT NULL, whep_url TEXT,   -- base pubblica del WHEP del nodo, se esiste
  version TEXT NOT NULL, started_at TIMESTAMPTZ NOT NULL, heartbeat TIMESTAMPTZ NOT NULL DEFAULT now(),
  live_streams INT NOT NULL DEFAULT 0, viewers INT NOT NULL DEFAULT 0, draining BOOLEAN NOT NULL DEFAULT false);
CREATE TABLE live_streams (           -- una riga = un proprietario
  name TEXT PRIMARY KEY, node_id TEXT NOT NULL REFERENCES nodes(node_id), session_id TEXT NOT NULL,
  started_at TIMESTAMPTZ NOT NULL, heartbeat TIMESTAMPTZ NOT NULL DEFAULT now());
ALTER TABLE viewer_samples  ADD COLUMN node_id TEXT NOT NULL DEFAULT '', ADD COLUMN ts_epoch BIGINT;  -- PK (broadcast_id, node_id, ts)
ALTER TABLE viewer_sessions ADD COLUMN node_id TEXT NOT NULL DEFAULT '';
ALTER TABLE recordings      ADD COLUMN node_id TEXT, ADD COLUMN lease_until TIMESTAMPTZ;   -- dove sta il file / chi lo finalizza
ALTER TABLE vod_items       ADD COLUMN claimed_by TEXT, ADD COLUMN lease_until TIMESTAMPTZ, ADD COLUMN attempts INT NOT NULL DEFAULT 0, ADD COLUMN storage_node TEXT;
```

### 3.3 Proprietà degli stream ed eventi

- **Publish:** `INSERT INTO live_streams … ON CONFLICT (name) DO UPDATE SET node_id=…, session_id=…, started_at=now(), heartbeat=now() WHERE live_streams.heartbeat < now() - make_interval(secs => $ttl) RETURNING session_id`. Nessuna riga = lo stream è vivo altrove: `POST /internal/kick/{name}` all'owner (se non risponde, attendere il TTL) e un solo retry; altrimenti rifiuto come oggi (`AlreadyLive`). Vale anche per la riconnessione di OBS su un altro nodo.
- **Heartbeat** ogni `heartbeat_secs`: `UPDATE live_streams SET heartbeat=now() WHERE node_id=$me`; se le righe aggiornate sono meno degli stream locali la proprietà è persa → kick locale. **Reaper** su ogni nodo: `DELETE … WHERE heartbeat < now() - ttl RETURNING name` + `NOTIFY`.
- **Canali NOTIFY** (payload ≤ ~200 B): `rms_broadcasts` (CRUD, pausa, rigenerazione chiave: invalida la cache e genera l'evento SSE `broadcast`), `rms_streams` (start, end, cambio owner: evento SSE `stream`, avvio/stop relay), `rms_settings` (`ui_language`, `ui_theme`, attesa predefinita), `rms_vod` (nuovo job o `ready`: sveglia i worker). Listener su connessione dedicata, resync a ogni riconnessione.
- L'evento SSE `viewers` **non** passa da NOTIFY: ogni nodo legge l'aggregato dal DB (cache 2–5 s).
- **Stato effettivo** (ARCH §4.2): `live` se l'hub locale ha lo stream o `live_streams` ha una riga fresca.
- **Cache `broadcasts`** in memoria (nome → riga, chiave → nome), invalidata da NOTIFY: oggi ogni richiesta FLV/pubblica interroga SQLite (ARCH §15); in cluster serve per latenza e per sopravvivere a un DB giù.

### 3.4 Relay origin → edge (FLV, WS-FLV, WebRTC)

- Una richiesta su un nodo che non ha lo stream, ma `live_streams` lo dà a un altro nodo, avvia **un solo** `RelayTask` (single-flight): `GET {advertise_url}/internal/relay/{name}.flv` con il segreto, e alimenta un `LiveStream` locale marcato `origin = Remote(node)`.
- L'handler `/internal/relay` sull'owner fa `subscribe()` (GOP cache prima, poi live) **senza** `ViewerSession`; lo stesso criterio su un edge che ritrasmette: nessuna catena (l'owner non fa mai relay, un edge rifiuta richieste relay per stream non propri), quindi niente loop.
- Sull'edge lo `LiveStream` relayed alimenta FLV, WS-FLV, packager HLS/DASH e WHEP come uno stream locale; **non** registra `stream_sessions` né registra su disco: lo fa solo l'owner.
- Il relay termina quando il token `ended` scatta (EOF dell'owner o cambio di owner in `rms_streams`) o dopo `relay_idle_secs` senza consumatori locali. Un edge lento è un consumatore lento dell'owner: vale la politica esistente (salta al prossimo keyframe, il publisher non si blocca).

### 3.5 HLS e DASH: il punto delicato

L'ancoraggio al clock di arrivo (ARCH §14) rende playlist e segmenti dei nodi **non intercambiabili**. Tre strade:

| Strada | Descrizione | Pro | Contro |
|---|---|---|---|
| **A. Stickiness per IP (fase 1)** | L'LB manda `/hls/` e `/dash/` sempre allo stesso nodo per lo stesso spettatore (hash consistente su `X-Real-IP`) | Zero codice; ogni nodo impacchetta da solo | Se il nodo muore il player ricarica; con NPM davanti `balance source` metterebbe tutti sullo stesso nodo (§4); CGNAT sbilancia |
| B. Proxy verso l'owner | L'edge inoltra playlist e segmenti a `/internal/hls/...` dell'owner e mette in cache i segmenti completi (immutabili, `max-age=3600`) | Un solo packager e una sola timeline | Va inoltrato anche il blocking reload; più codice (è l'approccio «edge HTTP/CDN» che anche il README di SRS suggerisce per HLS) |
| C. Packaging deterministico | Ancora la timeline a un istante di riferimento condiviso (`live_streams.started_at` / timestamp FLV) invece che all'arrivo | Qualunque nodo serve gli stessi URL | Cambia packager e test; i tagli di segmento devono coincidere dopo il primo keyframe |

Fase 1 = A (nessun cambio al packager); B o C quando serve il failover trasparente. Il viewer tracking resta locale: con A ogni spettatore è su un solo nodo e il doppio conteggio sparisce, tranne nei momenti di failover.

### 3.6 Metriche

- I campioni hanno `node_id`; chiave primaria `(broadcast_id, node_id, ts)`. I sampler dei nodi non sono sincronizzati: la query prima prende il **MAX per (nodo, bucket)** e poi fa la **SUM** fra i nodi.

```sql
SELECT bucket, SUM(t) AS total, SUM(flv) AS flv, SUM(hls) AS hls /* … */ FROM (
  SELECT (ts_epoch / $4) * $4 AS bucket, node_id, MAX(total) t, MAX(flv) flv, MAX(hls) hls
  FROM viewer_samples WHERE broadcast_id = $1 AND ts_epoch >= $2 AND ts_epoch < $3 GROUP BY 1, 2) s
GROUP BY bucket ORDER BY bucket;
```
- «Picco» = massimo della somma, non somma dei massimi. `viewer_sessions` e `stream_sessions` hanno `node_id`; la chiusura delle sessioni orfane all'avvio vale solo per le righe del nodo che riparte.
- I contatori live per l'SSE `viewers` sono la somma delle righe `nodes.viewers`/campioni recenti.

### 3.7 Registrazioni, video d'attesa, VOD

- **Registrazioni:** registra l'owner (il recorder consuma il suo hub). `recordings.node_id` + percorso relativo. Con volume condiviso qualunque nodo serve il download (Range incluso); senza, il nodo contattato fa fetch da `GET /internal/files/recordings/…` dell'owner. La crash recovery (riparazione FLV e remux) oggi parte all'avvio *per le proprie righe*; in cluster con storage condiviso un nodo vivo può rilevare `lease_until` scaduto e finalizzare il file di un nodo morto.
- **Video d'attesa:** l'upload arriva a un nodo qualsiasi e scrive su `data/waiting` (volume condiviso, o `node_id` + fetch interno per `/waiting/*` e poster).
- **VOD:** `vod_items` è già la coda (`queued → processing → ready | failed`). Claim: `UPDATE vod_items SET status='processing', claimed_by=$me, lease_until=now()+interval '60 s', attempts=attempts+1 WHERE id=(SELECT id FROM vod_items WHERE status='queued' ORDER BY created_at LIMIT 1 FOR UPDATE SKIP LOCKED) RETURNING *`. Il worker rinnova la lease ogni 15 s insieme al `progress`; il reaper rimette in `queued` le righe con lease scaduta (`attempts` < 3, poi `failed`). Output in `vod/<id>.tmp/` poi `rename` atomico; `ready` solo con `WHERE claimed_by=$me` (se la lease è stata persa il risultato viene scartato). Sorgente e rendition su storage condiviso; senza, il claim è limitato ai job con `storage_node=$me`. `max_parallel_jobs` resta per nodo. Il watcher dell'inbox: un solo nodo per volta (lease su una riga `singleton`) e chiave unica `(origin, source_path, size, mtime)` per evitare doppi import.
- **Task singleton** (retention, scansione inbox): `UPDATE singleton_tasks SET leased_until=now()+$d WHERE name=$n AND leased_until < now() RETURNING name`; funziona anche con PgBouncer e non richiede advisory lock di sessione.

### 3.8 WebRTC / WHEP

- Ogni nodo ha un proprio endpoint UDP raggiungibile: `webrtc.public_ip` del nodo e una porta propria. rms annuncia `public_ip:public_port` (API.md §4.5): con `webrtc.public_port` (fatto in 7C-1; 0 = `udp_port`) dietro un NAT con più nodi e un solo IP pubblico la porta esterna (8443, 8444, …) può differire da quella interna.
- Il `POST /whep/{name}` può arrivare a qualunque nodo (dopo relay lazy dell'hub locale). Le risorse `DELETE /whep/{name}/{session}` e `OPTIONS` sono per nodo: oggi un `DELETE` su un altro nodo dà `404` e la sessione viene chiusa dopo ≈ 15 s dal rilevamento ICE (ARCH §16). Proposta: prefisso del nodo nell'id di sessione (`n2-<uuid>`) e instradamento nell'LB (§4), oppure inoltro interno.
- Il draft WHEP-04 prevede il bilanciamento con **`307 Temporary Redirect`** (non 301/302) e `503` con `Retry-After` in caso di sovraccarico [28]: un nodo senza UDP raggiungibile o saturo può rimandare il client a un nodo con `nodes.whep_url`. Da provare nei browser (redirect cross-origin con `Content-Type: application/sdp`); preferibile restare sullo stesso hostname e instradare nell'LB.
- Nodi senza UDP raggiungibile: `webrtc.enabled=false`; il POST dà `403` e il player passa a FLV/HLS. Meglio non mandarglieli proprio (backend `rms_whep` solo con i nodi WebRTC, §4).
- **TURN:** rms oggi annuncia solo uno STUN (`webrtc.stun_server`, header `Link`). Il draft consente anche server TURN con credenziali nel `Link` (`rel="ice-server"`) [28]: un `coturn` davanti aiuterebbe gli spettatori con UDP bloccato; richiede una piccola estensione (`webrtc.turn_*`).

### 3.9 Salute e drain

- `GET /healthz` (pubblico, senza auth; **fatto in 7C-1**): 200 con `{"node_id", "cluster", "db": true|false, "live_streams", "version", "draining"}`; **liveness**, non dipende dal DB (se il DB è giù l'LB non deve togliere tutti i nodi, il data plane funziona). 503 solo in drain o durante l'arresto. Lo usano l'`HEALTHCHECK` del Dockerfile e il compose.
- Drain: `SIGTERM` o `POST /internal/drain` → `nodes.draining=true`, `/healthz` 503, nessun nuovo relay, le connessioni attive finiscono.

### 3.10 Cosa resta per-nodo

Hub e `LiveStream`, GOP cache, `SegmentStore` e packager HLS/DASH, publisher WebRTC e sessioni WHEP, tracker spettatori, recorder (sul nodo owner), code locali di ffmpeg (`max_parallel_jobs`), cache `broadcasts`, directory temporanee (`upload-tmp`, `waiting/tmp`).

### 3.11 Diagrammi

**Un publisher si collega**

```
OBS ──RTMP :1935──> LB/hostname ingest ──> Nodo A                         Postgres
                                            │ on_publish(chiave) ── cache broadcasts (miss → SELECT)
                                            │ INSERT live_streams ON CONFLICT … WHERE heartbeat vecchio ──> 1 riga: A è owner
                                            │ hub.publish_start, packager, recorder (se record), stream_sessions
                                            │ NOTIFY rms_streams {"name":"x","node":"A"} ──────────────────>
Nodo B, C ◄── LISTEN rms_streams ───────────┘  (cache name→A; SSE "stream: live"; nessun relay finché nessuno guarda)
```

**Uno spettatore su un altro nodo**

```
Browser ── GET /live/x.flv ──> LB ──> Nodo B        (hub di B: nessuno stream "x")
B: cache live_streams → x è di A, riga fresca → RelayTask(x) single-flight
B ── GET http://A:8081/internal/relay/x.flv  [X-RMS-Secret] ──> A
A: subscribe() = GOP cache + live, nessuna ViewerSession ──> FLV ──> B
B: LiveStream locale (origin=Remote(A)) ──> FLV/WS-FLV/HLS/DASH/WHEP di B ──> Browser   (ViewerSession su B)
… ultimo spettatore va via → dopo relay_idle_secs B chiude il relay
```

**Un nodo muore**

```
t0       Nodo A muore (kill -9 o rete): OBS perde il TCP; A non rinnova heartbeat
t0+0 s   B: il relay verso A va in EOF/reset (se cade la rete: timeout di lettura del relay) → LiveStream locale terminato; gli spettatori FLV ricevono EOF e il player riprova
t0+2..5  OBS si riconnette (hostname/LB) → Nodo C: INSERT live_streams … WHERE heartbeat < now()-ttl → 0 righe (la riga di A è ancora fresca)
         C: POST A:/internal/kick fallisce per timeout → C attende e ritenta
t0+ttl   riga di A scaduta → C diventa owner (10–15 s di buco con node_ttl 10–15 [S]); NOTIFY rms_streams {"x","C"}
t0+ttl   B vede owner=C → nuovo relay da C; HLS/DASH: nuova sessione di packaging (MSN e Period nuovi, come dopo un riavvio single-node)
sempre   reaper (un nodo qualsiasi): DELETE nodes/live_streams scaduti; vod_items con lease scaduta → queued (attempts+1)
```

### 3.12 Fasi di realizzazione

1. Trait `Store` + `SqliteStore` (nessun cambio funzionale). — **Fatto (7C-1):** `db::Db` con due backend (enum, non trait) e le stesse firme dei repository; SQLite invariato. 2. `PgStore`, migrazioni con lock, cache `broadcasts`, `nodes`, `/healthz`. — **Fatto (7C-1):** PostgreSQL via sqlx (migrazioni sotto `pg_advisory_xact_lock`), cache `broadcasts` con invalidazione locale, registro `nodes` con heartbeat/reaper, `/healthz`; anche `node_id` nelle tabelle, lease VOD, task singleton, `public_port`, IP dal proxy (ARCHITECTURE.md §20). 3. `live_streams`, NOTIFY, listener con resync, listener interno `/internal/*`. 4. Relay FLV (client HTTP), metriche per nodo. 5. VOD con lease, file condivisi/fetch interno. 6. WebRTC per nodo (`public_port`, prefisso sessione). 7. B/C per HLS/DASH, S3 (roadmap).

## 4. Load balancer e ingest

- **NPM da solo non bilancia.** La UI ha un solo `Forward Hostname / IP` e una sola porta; il template genera `proxy_pass $forward_scheme://$server:$port$request_uri`. In nginx un gruppo `upstream` si trova solo se la porta è assente: con le variabili e una porta esplicita il nome va al resolver DNS, con un `proxy_pass` statico c'è l'errore `upstream "x" may not have port N` [39][40]; un nome che risolve più IP farebbe round-robin *per richiesta*, senza health check e senza stickiness (rompe LL-HLS). Schema: **Internet → NPM (TLS) → HAProxy → rms1..N**, con NPM che punta a `lb:8080`.
- **HAProxy** [30]: `balance leastconn`, `option httpchk` su `/healthz`, `timeout client/server/tunnel` ≥ 1 h (FLV, SSE e WS-FLV sono connessioni lunghe), nessuna compressione. `/hls/` e `/dash/` con `balance hdr(X-Real-IP)` + `hash-type consistent` (§3.5-A; senza l'header HAProxy ripiega su roundrobin [30]). Il profilo Low-Latency di HLS richiede HTTP/2 o HTTP/3 verso il player [29]: «HTTP/2 Support» su NPM, oppure `alpn h2,http/1.1` se TLS termina su HAProxy. Se HAProxy sta dietro NPM, `X-Real-IP` (impostato da NPM a `$remote_addr`) è l'IP reale; `balance source` darebbe a tutti l'IP di NPM. `option forwardfor` *accoda* un header e il server deve usare l'ultima occorrenza [30]: rms (da 7C-1) usa `X-Real-IP` e poi l'ultimo elemento; l'esempio sotto riscrive comunque `X-Forwarded-For` da `X-Real-IP`.
- **Alternative:** nginx con `upstream`, Traefik, LB cloud. Qualsiasi LB va bene se: health check HTTP, timeout lunghi, nessun buffering/compressione, hash per IP su `/hls/` e `/dash/`.
- **Niente sticky per FLV, WS-FLV, SSE, `POST /whep`**: sono per connessione.
- **RTMP (1935/tcp).** Due modi. (1) Hostname dedicato `ingest.example.com` verso un solo nodo o un VIP (keepalived): semplice, un publisher vede sempre lo stesso nodo. (2) TCP LB (HAProxy `mode tcp`, o uno **Stream** di NPM verso `lb:1935`): la chiave finisce su un nodo arbitrario, cosa che il relay e `live_streams` gestiscono; si perde l'IP del publisher (rms vede l'LB; il PROXY protocol non è supportato dal listener).
- **WebRTC.** Non passa da NPM né dall'LB HTTP: UDP per nodo con `public_ip`/porta propri (§3.8). Alternative: TURN, oppure WHEP disattivato (il player cade su FLV/HLS da solo, ARCH §16).

```
# haproxy.cfg (estratto) — dietro NPM
global
    log stdout format raw local0
defaults
    mode http
    log global
    timeout connect 3s
    timeout client 1h
    timeout server 1h
    timeout tunnel 1h
    timeout client-fin 10s
resolvers docker                    # DNS embedded di Docker; con Podman: IP del gateway della rete (aardvark-dns)
    nameserver dns1 127.0.0.11:53
    resolve_retries 3
    timeout resolve 1s
    timeout retry 1s
    hold valid 10s
frontend http_in
    bind :8080
    http-request del-header X-Forwarded-For
    http-request set-header X-Forwarded-For %[req.hdr(X-Real-IP)] if { req.hdr(X-Real-IP) -m found }
    http-request deny if { path_beg /internal/ }
    use_backend rms_ll   if { path_beg /hls/ /dash/ }
    use_backend rms_whep if { path_beg /whep/ }
    default_backend rms_http
backend rms_http
    balance leastconn
    option httpchk
    http-check send meth GET uri /healthz
    http-check expect status 200
    default-server check inter 2s fall 3 rise 2 resolvers docker init-addr libc,none
    server rms1 rms1:8080
    server rms2 rms2:8080
backend rms_ll                      # stesso spettatore → stesso nodo
    balance hdr(X-Real-IP)
    hash-type consistent
    option httpchk
    http-check send meth GET uri /healthz
    default-server check inter 2s fall 3 rise 2 resolvers docker init-addr libc,none
    server rms1 rms1:8080
    server rms2 rms2:8080
backend rms_whep                    # solo nodi con UDP raggiungibile
    balance leastconn
    default-server check inter 2s fall 3 rise 2 resolvers docker init-addr libc,none
    server rms1 rms1:8080
    server rms2 rms2:8080
frontend rtmp_in
    mode tcp
    bind :1935
    default_backend rms_rtmp
backend rms_rtmp
    mode tcp
    balance leastconn
    default-server check resolvers docker init-addr libc,none
    server rms1 rms1:1935
    server rms2 rms2:1935
```

Con un numero variabile di repliche si può usare `server-template rms 1-6 rms:8080 check` con `resolvers` (DNS di Docker `127.0.0.11`; con Podman l'IP del gateway della rete) [30], ma con WebRTC attivo ogni nodo ha porte diverse (§3.8) e i server restano espliciti.

## 5. Docker

Sul branch `feat/docker` (commit `7bb3cc0`) esistono già `Dockerfile` e `docker-compose.yml` per il single-node; qui solo struttura e raccomandazioni.

- **Immagine** (multi-stage): `rust:1.99-bookworm` (il tag `1.99-bookworm` esiste [37]; meglio fissarlo che usare `1-bookworm`) per `cargo build --release --locked`; stage ffmpeg; runtime `debian:bookworm-slim` con `ca-certificates`, `tini` (e `curl` per l'healthcheck), utente non-root `rms` (UID 10001), `VOLUME /data`, `EXPOSE 8080/tcp 1935/tcp 8443/udp`, `ENV RMS_*`. Nessuna libreria nativa extra: `Cargo.lock` non contiene `openssl-sys` né `native-tls` (c'è `ring`) e SQLite è incorporato (`rusqlite` con `bundled`).
- **ffmpeg:** BtbN `linux64-gpl` è statico e include libx264 [34], ma il tag `latest` è mobile e le build giornaliere vengono potate (le ultime 14 più una al mese per due anni [34]): fissare una release versionata (`n7.1`) o usare il pacchetto Debian (bookworm: 5.1.9; trixie: 7.1.5 [38]). Per `h264_nvenc` serve il runtime NVIDIA nel container. La GPL implica obblighi di redistribuzione: non pubblicare l'immagine senza verificarli.
- **Postgres 18:** il volume va montato su `/var/lib/postgresql`, non su `/var/lib/postgresql/data` (PGDATA è `/var/lib/postgresql/18/docker`) [41].
- **HEALTHCHECK e Podman:** `podman build` produce il formato OCI, che non conserva `HEALTHCHECK`: costruire con `--format docker` (o `BUILDAH_FORMAT=docker`) oppure dichiarare `healthcheck:` nel compose [35].
- **Podman su questo host** (5.8.2, rootless, netavark): `podman-compose` richiede la risoluzione DNS fra container (netavark/aardvark-dns) [36]; le porte < 1024 (NPM 80/443) richiedono `sysctl net.ipv4.ip_unprivileged_port_start=80`; le reti bridge rootless usano `rootlessport`, che **non preserva l'IP del client**, mentre `pasta` sì [36]: per WebRTC e per l'IP dei publisher preferire pasta (`rootless_port_forwarder="pasta"`, sperimentale) o Podman rootful / `network_mode: host`. `podman play kube` accetta un Pod YAML Kubernetes ma non il file Compose (`podman kube generate` produce lo YAML).
- `--scale` non va bene per i nodi con WebRTC (stesse variabili, stessa porta UDP): servizi espliciti `rms1`, `rms2` con ancora YAML.

```yaml
# docker-compose.cluster.yml — 2 nodi rms + Postgres + HAProxy (TLS: NPM esterno, che punta a lb:8080)
x-rms: &rms
  image: rms:latest
  restart: unless-stopped
  depends_on: { db: { condition: service_healthy } }
  volumes: [ "shared:/data" ]            # su più host: volume NFS (§6)
  healthcheck: { test: ["CMD","curl","-fsS","http://127.0.0.1:8080/healthz"], interval: 10s, timeout: 3s, retries: 3 }
x-env: &env
  RMS_ADMIN_TOKEN: ${RMS_ADMIN_TOKEN:?}            # identico su tutti i nodi
  RMS_SERVER_PUBLIC_URL: https://stream.example.com
  RMS_SERVER_TRUST_PROXY: "true"
  RMS_CLUSTER_ENABLED: "true"
  RMS_CLUSTER_DATABASE_URL: postgres://rms:${PG_PASSWORD:?}@db:5432/rms
  RMS_CLUSTER_INTERNAL_SECRET: ${RMS_INTERNAL_SECRET:?}
  RMS_WEBRTC_PUBLIC_IP: ${PUBLIC_IP:?}
services:
  db:
    image: postgres:18
    environment: { POSTGRES_DB: rms, POSTGRES_USER: rms, POSTGRES_PASSWORD: "${PG_PASSWORD:?}" }
    volumes: [ "pgdata:/var/lib/postgresql" ]      # PG ≥ 18: non /var/lib/postgresql/data
    healthcheck: { test: ["CMD-SHELL","pg_isready -U rms -d rms"], interval: 5s, retries: 10 }
  rms1:
    <<: *rms
    environment: { <<: *env, RMS_CLUSTER_NODE_ID: rms1, RMS_CLUSTER_ADVERTISE_URL: "http://rms1:8081", RMS_WEBRTC_UDP_PORT: "8443" }
    ports: [ "8443:8443/udp" ]                     # WebRTC: diretto, non via proxy
  rms2:
    <<: *rms
    environment: { <<: *env, RMS_CLUSTER_NODE_ID: rms2, RMS_CLUSTER_ADVERTISE_URL: "http://rms2:8081", RMS_WEBRTC_UDP_PORT: "8444" }
    ports: [ "8444:8444/udp" ]
  lb:
    image: haproxy:3.4
    volumes: [ "./haproxy.cfg:/usr/local/etc/haproxy/haproxy.cfg:ro" ]
    ports: [ "1935:1935" ]                         # RTMP; 8080 resta interno (NPM → lb:8080)
    depends_on: [ rms1, rms2 ]
volumes: { shared: {}, pgdata: {} }
```

Con Podman: `podman-compose -f docker-compose.cluster.yml up -d`. Le variabili `RMS_CLUSTER_*` esistono da 7C-1 (§3.1); serve anche `RMS_CLUSTER_ADVERTISE_URL` per nodo.

## 6. Storage condiviso (registrazioni, VOD, video d'attesa)

| Opzione | Quando | Note |
|---|---|---|
| Volume locale (stesso host) | Prove, un solo host con più container | Zero rete; nessuna HA |
| **NFS v4.1+** | Più host, nessun cambio al codice | SPOF del NAS; ffmpeg scrive molti file piccoli (VOD); la cache attributi NFS può mostrare file con ritardo: scrivere con nomi nuovi e `rename` atomico; mappare UID 10001 sul server. **Mai il database SQLite su NFS** [31] (in cluster non serve: usa Postgres) |
| SMB/CIFS | Solo se è tutto ciò che c'è | Semantica POSIX incompleta (permessi, rename, Range): sconsigliato per ffmpeg [S] |
| Locale al nodo + indice | Nessuna infrastruttura | `node_id` + percorso nelle tabelle; fetch interno con Range; i file si perdono col nodo |
| S3-compatibile | Roadmap | Serve `object_store` 0.14.2 [10] e riscrivere recorder (multipart), VOD (ffmpeg su disco temporaneo poi upload) e download (redirect/presigned); HLS/DASH VOD servibili direttamente da bucket/CDN. **MinIO community è archiviato** (README «NO LONGER MAINTAINED», repo read-only dal 25/04/2026 [32]): alternative SeaweedFS (Apache-2.0), Garage (AGPL), Ceph RGW o un servizio gestito |

Primo rilascio: volume condiviso **oppure** locale + fetch interno, dietro la stessa astrazione `FileRef { node_id, relative_path }`.

## 7. Prove da fare prima di fidarsi

Latenza reale del relay FLV (cold start e regime); durata del failover con `node_ttl` 10 e 15 s; NOTIFY sotto carico con 3–6 nodi; redirect 307 di WHEP nei browser; aardvark-dns con più container sullo stesso alias; MSN/PDT di un nodo edge vs owner per confermare §3.5.

## 8. Fonti

Consultate il 2026-10-09. Le citazioni del codice rms sono quelle del repository (branch `main`, commit `9222f8d`).

[1] https://www.postgresql.org/docs/current/sql-notify.html
[2] https://www.postgresql.org/docs/current/sql-select.html (The Locking Clause)
[3] https://www.postgresql.org/docs/current/explicit-locking.html (Advisory Locks)
[4] https://www.pgbouncer.org/features.html
[5] https://recall.ai/blog/postgres-listen-notify-does-not-scale
[6] https://docs.rs/sqlx/0.9.0/sqlx/postgres/struct.PgListener.html
[7] https://docs.rs/sqlx/0.9.0/sqlx/any/index.html e https://github.com/launchbadge/sqlx/blob/main/sqlx-core/src/any/type_info.rs
[8] https://github.com/launchbadge/sqlx/blob/main/sqlx-sqlite/src/arguments.rs
[9] https://github.com/launchbadge/sqlx/blob/main/CHANGELOG.md (0.9.0)
[10] https://crates.io/crates/sqlx, /tokio-postgres, /redis, /async-nats, /chitchat, /foca, /mdns-sd, /tonic, /object_store (API: https://crates.io/api/v1/crates/<nome>)
[11] https://www.postgresql.org/support/versioning/
[12] https://redis.io/docs/latest/develop/pubsub/
[13] https://redis.io/docs/latest/develop/data-types/streams/
[14] https://redis.io/docs/latest/develop/clients/patterns/distributed-locks/
[15] https://redis.io/docs/latest/operate/oss_and_stack/install/version-mgmt/
[16] https://redis.io/legal/licenses/
[17] https://github.com/valkey-io/valkey
[18] https://docs.rs/redis/latest/redis/
[19] https://docs.nats.io/nats-concepts/jetstream, /key-value-store, /core-nats e https://docs.nats.io/running-a-nats-service/configuration/clustering/jetstream_clustering
[20] https://github.com/nats-io/nats.rs
[21] https://github.com/quickwit-oss/chitchat
[22] https://kubernetes.io/docs/concepts/services-networking/service/#headless-services
[23] https://docs.docker.com/reference/compose-file/deploy/ (`endpoint_mode: dnsrr`); `tasks.<servizio>`: https://docs.w3cub.com/docker~1.13/engine/swarm/networking/index (fonte secondaria, Docker 1.13)
[24] https://docs.antmedia.io/guides/clustering-and-scaling/ e .../supported-databases/scaling-with-redis/
[25] https://ossrs.io/lts/en-us/docs/v6/doc/origin-cluster e .../sample-origin-cluster; README https://github.com/ossrs/srs (edge HTTP per HLS)
[26] https://mediamtx.org/docs/features/proxy e https://github.com/bluenviron/mediamtx
[27] https://github.com/arut/nginx-rtmp-module/wiki/Directives
[28] https://www.ietf.org/archive/id/draft-ietf-wish-whep-04.html (§4.7, §4.8)
[29] https://datatracker.ietf.org/doc/html/draft-pantos-hls-rfc8216bis (profilo Low-Latency: HTTP/2 o HTTP/3)
[30] https://docs.haproxy.org/3.0/configuration.html (`server-template`, `option forwardfor`, `timeout tunnel`)
[31] https://sqlite.org/wal.html e https://www.sqlite.org/howtocorrupt.html
[32] https://github.com/minio/minio ; https://garagehq.deuxfleurs.fr/ ; https://crates.io/crates/object_store
[33] https://rqlite.io/
[34] https://github.com/BtbN/FFmpeg-Builds
[35] https://docs.redhat.com/en/documentation/red_hat_enterprise_linux/9/html/building_running_and_managing_containers/assembly_monitoring-containers_configuring-unified-configuration-for-rootless-podman (HEALTHCHECK solo nel formato docker)
[36] https://github.com/containers/podman-compose e https://man.archlinux.org/man/extra/podman/podman-rootless.7.en
[37] Docker Hub: `rust:1.99-bookworm`, `postgres:18.6`, `haproxy:3.4`, `jc21/nginx-proxy-manager:2.16.0`
[38] https://packages.debian.org/bookworm/ffmpeg e https://packages.debian.org/trixie/ffmpeg
[39] nginx: https://nginx.org/en/docs/http/ngx_http_proxy_module.html, https://nginx.org/en/docs/stream/ngx_stream_proxy_module.html, https://nginx.org/en/docs/http/websocket.html; sorgente `src/http/ngx_http_upstream.c` (errore `upstream "X" may not have port N`)
[40] Nginx Proxy Manager v2.16.0: https://github.com/NginxProxyManager/nginx-proxy-manager/tree/v2.16.0 (`backend/templates/proxy_host.conf`, `_location.conf`, `stream.conf`, `docker/rootfs/etc/nginx/conf.d/include/proxy.conf`)
[41] https://github.com/docker-library/docs/blob/master/postgres/content.md (PGDATA e VOLUME dalla 18)

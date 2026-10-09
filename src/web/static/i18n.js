// rms UI translations for the dashboard and the embedded player: Italian (the
// reference wording) and English. Vanilla ES module, no build step; imported by
// app.js, core.js, detail.js, metrics.js and player.js.
//
// - t(key, params?)      text in the current language; `{name}` placeholders are filled
//                        from `params`. A key missing in the current language falls back
//                        to Italian and logs one console.warn per key.
// - tn(key, n, params?)  plural forms: `${key}.one` / `${key}.other` (Intl.PluralRules).
// - setLanguage(lang) / getLanguage() / getLocale() / onLanguageChange(fn)
// - translateDom(root)   resolves data-i18n="key" (text content), data-i18n-title,
//                        data-i18n-aria-label and data-i18n-placeholder (attributes).
// - formatNumber / formatDate / formatBytes / formatDuration: Intl with it-IT / en-US.
//
// Dictionaries: exactly one `'key': 'text',` per line in both objects. A Rust unit test
// (src/web/mod.rs) checks that `it` and `en` have the same keys and that every key used
// by the other static files (t('...'), tn('...'), data-i18n*="...") exists.

export const it = {
  // Page shell (index.html)
  'app.docTitle': 'rms · Pannello dirette',
  'app.skip': 'Vai al contenuto',
  'nav.brandLabel': 'rms, elenco dirette',
  'nav.label': 'Navigazione principale',
  'nav.broadcasts': 'Dirette',
  'nav.settings': 'Impostazioni',
  'logout.title': 'Esci dal pannello',
  'logout.label': 'Esci',
  'conn.connecting': 'Connessione…',
  'conn.live': 'Tempo reale',
  'conn.polling': 'Riconnessione…',
  'conn.liveTitle': 'Aggiornamenti in tempo reale attivi',
  'conn.pollingTitle': 'Aggiornamenti in tempo reale non disponibili: aggiorno ogni 10 secondi',
  'boot.unreachable': 'Server non raggiungibile',

  // Common words and actions
  'common.loading': 'Caricamento…',
  'common.cancel': 'Annulla',
  'common.confirm': 'Conferma',
  'common.close': 'Chiudi',
  'common.save': 'Salva',
  'common.saveText': 'Salva testo',
  'common.retry': 'Riprova',
  'common.copy': 'Copia',
  'common.no': 'No',
  'common.rec': 'REC',
  'common.total': 'Totale',
  'common.status': 'Stato',
  'common.viewers': 'Spettatori',
  'common.protocol': 'Protocollo',
  'common.broadcast': 'Diretta',
  'common.bytesIn': 'Dati ricevuti',
  'common.liveNow': 'Dirette in onda',
  'action.pause': 'Pausa',
  'action.resume': 'Riprendi',
  'action.edit': 'Modifica',
  'action.delete': 'Elimina',
  'action.deleteVideo': 'Elimina video',
  'action.play': 'Riproduci',
  'action.download': 'Scarica',
  'action.regenerate': 'Rigenera',
  'action.disconnect': 'Disconnetti encoder',
  'action.recordNow': 'Registra ora',
  'action.stopRecording': 'Ferma registrazione',

  // Broadcast status badges
  'status.live': 'In diretta',
  'status.paused': 'In pausa',
  'status.offline': 'Offline',

  // Time and durations
  'time.since': 'da {duration}',
  'duration.days': '{d} g {h} h',
  'duration.hours': '{h} h {m} min',
  'duration.minutes': '{m} min {s} s',
  'duration.seconds': '{s} s',

  // Error messages
  'error.unknown': 'Errore sconosciuto.',
  'error.aborted': 'Operazione annullata.',
  'error.unexpected': 'Errore imprevisto: {message}',
  'error.network': 'Impossibile contattare il server. Controlla la connessione.',
  'error.sessionExpired': 'Sessione scaduta: accedi di nuovo.',
  'error.notImplemented': 'Funzione non ancora disponibile in questa versione del server.',
  'error.server': 'Errore del server ({status}). Riprova tra poco.',
  'error.request': 'Richiesta non riuscita ({status}): {message}',
  'error.broadcastNotFound': 'Diretta non trovata.',
  'error.alreadyExists': 'Esiste già una diretta con questo nome.',
  'error.titleRequired': 'Il titolo è obbligatorio.',
  'error.invalidName': "Nome non valido: usa solo lettere minuscole, cifre, '-' e '_' (max 64 caratteri).",
  'error.nameImmutable': 'Il nome di una diretta non si può cambiare.',
  'error.tooLong': 'Testo troppo lungo.',
  'error.alreadyRecording': 'La registrazione è già in corso.',
  'error.notRecording': 'Nessuna registrazione in corso.',
  'error.notLive': 'La diretta non è attiva in questo momento.',
  'error.fileTooLarge': 'Il file supera la dimensione massima consentita.',
  'error.noFfmpeg': 'ffmpeg non è disponibile sul server: carica un file MP4 (H.264/AAC).',
  'error.unsupportedType': 'Formato non supportato: carica un video MP4, MOV, M4V, WebM, MKV o AVI.',
  'error.cannotProcess': 'Impossibile elaborare il video: il file è danneggiato o non contiene una traccia video valida.',
  'error.timedOut': 'Elaborazione del video troppo lunga: interrotta.',
  'error.emptyFile': 'Il file caricato è vuoto.',
  'error.noFreeName': 'Impossibile generare un nome libero: indicane uno.',
  'error.invalidLanguage': 'Lingua non supportata.',

  // Toasts, dialogs, copy fields (core.js)
  'toast.close': 'Chiudi notifica',
  'toast.copied': 'Copiato negli appunti.',
  'toast.copyFailed': 'Copia non riuscita: seleziona il testo e usa Ctrl+C.',
  'toast.copyFailedShort': 'Copia non riuscita.',
  'field.showKey': 'Mostra chiave',
  'field.hideKey': 'Nascondi chiave',
  'field.copyLabel': 'Copia {label}',

  // Upload widget (waiting videos)
  'upload.defaultLabel': 'Carica un video',
  'upload.noFile': 'Nessun file selezionato',
  'upload.choose': 'Scegli file…',
  'upload.send': 'Carica',
  'upload.progressLabel': 'Avanzamento caricamento',
  'upload.maxSize': 'Dimensione massima: {mb} MB.',
  'upload.limitsFfmpeg': "Formati: MP4, MOV, M4V, WebM, MKV, AVI. {max} Il video viene convertito in MP4 H.264/AAC e viene creata un'immagine di anteprima.",
  'upload.limitsNoFfmpeg': "ffmpeg non è disponibile sul server: carica un file MP4 (H.264/AAC), verrà usato così com'è. {max}",
  'upload.unsupported': 'Formato non supportato: scegli un file video.',
  'upload.mp4Only': 'Senza ffmpeg sul server si possono caricare solo file MP4 (H.264/AAC).',
  'upload.tooLarge': 'Il file è troppo grande ({size}): il massimo è {max} MB.',
  'upload.progressPct': 'Caricamento: {pct}%',
  'upload.progress': 'Caricamento: {pct}% ({loaded} di {total})',
  'upload.converting': 'File inviato. Conversione del video in corso…',
  'upload.saving': 'File inviato. Salvataggio in corso…',
  'upload.done': 'Video di attesa aggiornato.',
  'upload.toastDone': 'Video di attesa caricato.',

  // Broadcast flags (create and edit dialogs)
  'flags.legend': 'Opzioni',
  'flags.record': 'Registra automaticamente',
  'flags.recordHint': 'La registrazione parte da sola quando arriva il segnale da OBS.',
  'flags.flv': 'HTTP-FLV e WebSocket-FLV',
  'flags.flvHint': 'Latenza 1–2 s: la scelta predefinita del player su desktop e Android.',
  'flags.hls': 'LL-HLS',
  'flags.hlsHint': 'Necessario per iPhone/iPad e Safari; adatto alle CDN.',
  'flags.dash': 'LL-DASH',
  'flags.dashHint': 'Alternativa standard per i player basati su MSE.',
  'flags.webrtc': 'WebRTC (WHEP)',
  'flags.webrtcHint': 'Latenza sotto il secondo; il server deve avere la porta UDP WebRTC raggiungibile.',

  // Login
  'login.docTitle': 'Accesso · rms',
  'login.title': 'Pannello delle dirette',
  'login.lead': 'Accedi per gestire le dirette, le impostazioni di OBS e il codice di embed.',
  'login.tokenLabel': 'Token di amministrazione',
  'login.tokenHint': 'Lo trovi nel file data/admin_token del server (o nel log al primo avvio).',
  'login.submit': 'Accedi',
  'login.emptyToken': 'Inserisci il token.',
  'login.invalidToken': 'Token non valido.',

  // Broadcast list
  'list.docTitle': 'Dirette · rms',
  'list.title': 'Dirette',
  'list.new': 'Nuova diretta',
  'list.emptyTitle': 'Nessuna diretta',
  'list.emptyText': 'Crea la prima diretta: otterrai le impostazioni da inserire in OBS e il codice da incorporare nel sito.',
  'list.broadcasts.one': '{n} diretta',
  'list.broadcasts.other': '{n} dirette',
  'list.onAir': '{n} in onda',
  'list.viewers.one': '{n} spettatore',
  'list.viewers.other': '{n} spettatori',
  'card.recTitle': 'Registrazione in corso',
  'card.viewersLabel.one': '{n} spettatori',
  'card.viewersLabel.other': '{n} spettatori',
  'card.embed': 'Embed',
  'card.embedTitle': 'Copia il codice iframe da incollare nel sito',
  'card.embedLabel': 'Copia il codice embed di {title}',
  'card.open': 'Apri',
  'card.openLabel': 'Apri {title}',
  'card.signalFor': 'segnale da {duration}',
  'card.waitingSignal': 'In attesa del segnale da OBS',
  'card.pauseLabel': 'Metti in pausa {title}',
  'card.resumeLabel': 'Riprendi {title}',
  'toast.iframeCopied': 'Codice iframe copiato negli appunti.',
  'toast.resumed': 'Diretta ripresa.',
  'toast.paused': 'Diretta in pausa: i player mostrano il video di attesa.',
  'toast.streamLive': '«{title}» riceve il segnale ed è in diretta.',
  'toast.streamOffline': '«{title}»: il segnale si è interrotto.',

  // New broadcast dialog
  'create.title': 'Nuova diretta',
  'create.titleLabel': 'Titolo',
  'create.titlePlaceholder': 'Es. Messa della domenica',
  'create.nameLabel': "Nome nell'URL",
  'create.nameHint': 'Generato dal titolo; usato in tutti i link (lettere minuscole, cifre, «-» e «_»). Non si può cambiare dopo.',
  'create.descLabel': 'Descrizione (facoltativa)',
  'create.waitingLabel': 'Testo di attesa (facoltativo)',
  'create.waitingHint': "Mostrato dal player prima dell'inizio e durante le pause. Se lo lasci vuoto si usa il testo predefinito globale (Impostazioni).",
  'create.defaultPlaceholder': 'Predefinito: {text}',
  'create.playerLink': 'Link del player: ',
  'create.invalidName': 'Nome non valido: usa solo lettere minuscole, cifre, «-» e «_» (max 64 caratteri).',
  'create.submit': 'Crea diretta',
  'toast.created': 'Diretta «{title}» creata.',

  // Settings page
  'settings.docTitle': 'Impostazioni · rms',
  'settings.title': 'Impostazioni',
  'settings.lead': 'Valori predefiniti per tutte le dirette e informazioni sul server.',
  'settings.languageSection': 'Lingua',
  'settings.language': "Lingua dell'interfaccia",
  'settings.languageNote': 'Vale per il pannello e per i testi del player incorporato (attesa, pausa, pulsanti). Un singolo embed può forzare la lingua con il parametro ?lang=it oppure ?lang=en.',
  'settings.languageSave': 'Salva lingua',
  'settings.languageSaved': 'Lingua salvata.',
  'lang.name.it': 'Italiano',
  'lang.name.en': 'English',
  'settings.waitingSection': 'Video di attesa predefinito',
  'settings.waitingLead': 'Mostrato in loop dai player delle dirette che non hanno un proprio video di attesa, quando sono offline o in pausa.',
  'settings.uploadLabel': 'Carica il video predefinito',
  'settings.textLabel': 'Testo di attesa predefinito',
  'settings.textHint': 'Usato dalle dirette che non hanno un proprio testo di attesa. Lascia vuoto per ripristinare «{text}».',
  'settings.textSaved': 'Testo predefinito salvato.',
  'settings.noVideo': 'Nessun video predefinito',
  'settings.noVideoHint': 'Le dirette senza un proprio video mostrano il titolo e il testo di attesa su sfondo animato.',
  'settings.videoHint': 'Usato da tutte le dirette senza un video di attesa proprio.',
  'settings.deleteVideoTitle': 'Eliminare il video predefinito?',
  'settings.deleteVideoText': 'Le dirette senza un video proprio mostreranno il testo di attesa.',
  'settings.videoDeleted': 'Video predefinito eliminato.',
  'settings.overview': 'Panoramica',
  'settings.totalViewers': 'Spettatori totali',
  'settings.noBroadcasts': 'Nessuna diretta.',
  'settings.system': 'Sistema',
  'system.ffmpegMissingTitle': 'ffmpeg non è disponibile sul server. ',
  'system.ffmpegMissingText': "Il video di attesa deve essere un file MP4 (H.264/AAC) e viene usato così com'è (senza immagine di anteprima); le registrazioni restano in formato FLV. Installa ffmpeg o indica il percorso in [ffmpeg] path.",
  'system.version': 'Versione',
  'system.uptime': 'In esecuzione da',
  'system.ffmpeg': 'ffmpeg',
  'system.available': 'disponibile',
  'system.unavailable': 'non disponibile',
  'system.ffmpegPath': 'Percorso ffmpeg',
  'system.rtmpBind': 'RTMP in ascolto su',
  'system.httpBind': 'HTTP in ascolto su',
  'system.publicUrl': 'URL pubblico',
  'system.publicUrlDerived': 'dedotto dalla richiesta (Host)',
  'system.webrtc': 'WebRTC (WHEP)',
  'system.iceAddresses': 'Indirizzi ICE annunciati',
  'system.rtcDisabled': 'disattivato',
  'system.rtcUnavailableReason': 'non disponibile: {error}',
  'system.rtcStatus': 'porta UDP {port} · {sessions} sessioni',
  'system.rtcVideoOnly': 'solo video (manca ffmpeg)',

  // Broadcast detail: header and navigation
  'detail.notFound': 'Diretta non trovata',
  'detail.notFoundText': 'Non esiste una diretta chiamata «{name}» (forse è stata eliminata).',
  'detail.back': 'Torna alle dirette',
  'detail.viewersTitle': 'Spettatori connessi',
  'detail.viewersLabel.one': '{n} spettatori connessi',
  'detail.viewersLabel.other': '{n} spettatori connessi',
  'detail.openPlayer': 'Apri player',
  'detail.sections': 'Sezioni della diretta',
  'detail.nav.preview': 'Anteprima',
  'detail.nav.obs': 'OBS',
  'detail.nav.links': 'Link e embed',
  'detail.nav.metrics': 'Metriche',
  'detail.nav.recordings': 'Registrazioni',
  'detail.nav.waiting': 'Video di attesa',
  'detail.deletedElsewhere': 'Questa diretta è stata eliminata.',

  // Broadcast detail: status panel
  'state.since': 'In diretta da',
  'state.ip': 'IP encoder',
  'state.video': 'Video',
  'state.audio': 'Audio',
  'state.bitrate': 'Bitrate in ingresso',
  'state.recording': 'Registrazione',
  'state.pausedHint': ' Il segnale arriva ma i player mostrano il video di attesa.',
  'state.measuring': 'misura in corso…',
  'state.measured': '{rate} (misurato)',
  'state.recSince': 'In corso dalle {time}',
  'state.resumeTitle': 'Torna a mostrare il live ai player',
  'state.pauseTitle': 'Mostra il video di attesa ai player senza fermare OBS',
  'state.recDisabledTitle': 'Disponibile quando la diretta riceve il segnale',
  'state.byProtocol': 'Spettatori per protocollo',
  'state.sessions': 'Sessioni connesse ',
  'state.colIp': 'IP',
  'state.colClient': 'Client',
  'state.colConnected': 'Connesso',
  'state.noSessions': 'Nessuno spettatore connesso.',
  'state.disconnectTitle': "Disconnettere l'encoder?",
  'state.disconnectText': 'OBS verrà scollegato dal server. Se è configurato per riconnettersi, ripartirà da solo dopo qualche secondo: per fermarlo davvero rigenera la chiave o metti in pausa.',
  'state.disconnectOk': 'Disconnetti',
  'state.disconnected': 'Encoder disconnesso.',
  'state.deleteTitle': 'Eliminare la diretta?',
  'state.deleteText': "La diretta «{title}» verrà eliminata definitivamente insieme al suo video di attesa. L'encoder connesso viene scollegato; le registrazioni restano disponibili.",
  'state.deleteOk': 'Elimina diretta',
  'state.deleted': 'Diretta eliminata.',
  'state.resumed': 'Diretta ripresa: i player tornano al live.',
  'state.recStopped': 'Registrazione fermata.',
  'state.recStarted': 'Registrazione avviata.',

  // Broadcast detail: live preview
  'preview.title': 'Anteprima live',
  'preview.reload': 'Ricarica anteprima',
  'preview.turnOff': 'Disattiva',
  'preview.turnOn': 'Attiva',
  'preview.off': 'Anteprima disattivata.',
  'preview.frameTitle': 'Anteprima della diretta',
  'preview.hint': "È il player embed, come lo vedono gli spettatori (senza audio). L'anteprima conta come uno spettatore.",

  // Broadcast detail: OBS settings
  'obs.title': 'Impostazioni OBS',
  'obs.server': 'Server',
  'obs.serverHint': 'In OBS: Impostazioni › Stream › Servizio «Personalizzato».',
  'obs.key': 'Chiave stream',
  'obs.keyHint': 'Segreta: chi la conosce può trasmettere su questa diretta.',
  'obs.regenTitle': 'Genera una nuova chiave stream',
  'obs.regenConfirmTitle': 'Rigenerare la chiave stream?',
  'obs.regenConfirmText': 'La chiave attuale non sarà più accettata: dovrai incollare quella nuova in OBS. Un encoder già collegato resta connesso finché non si disconnette.',
  'obs.regenConfirmOk': 'Rigenera chiave',
  'obs.regenerated': 'Nuova chiave generata: aggiornala in OBS.',
  'obs.serverNote': 'Il server accetta qualsiasi nome di applicazione: basta che la chiave sia corretta. Sono supportati H.264 + AAC (consigliato) e, solo per HTTP-FLV, HEVC/AV1 con Enhanced RTMP.',
  'obs.recoTitle': 'Impostazioni consigliate per la bassa latenza',
  'obs.encoder': 'Encoder',
  'obs.keyframeNote': "In OBS l'intervallo dei fotogrammi chiave predefinito è «0» (automatico, fino a 8 s): impostalo esplicitamente, altrimenti LL-HLS e LL-DASH avranno latenza alta.",
  'obs.gopWarnTitle': 'Intervallo tra fotogrammi chiave troppo lungo: {secs} s. ',
  'obs.gopWarnText': 'In OBS imposta «Intervallo fotogrammi chiave» a 2 s (o 1 s): con GOP lunghi LL-HLS e LL-DASH hanno latenza alta e avvio lento.',
  'obs.outputMode': 'Modalità di output',
  'obs.outputModeValue': 'Avanzata (Impostazioni › Output)',
  'obs.rateControl': 'Controllo frequenza',
  'obs.bitrate': 'Bitrate',
  'obs.bitrateValue': '2500–6000 kbps, secondo la banda in upload (lascia margine)',
  'obs.keyframe': 'Intervallo fotogrammi chiave',
  'obs.keyframeValue': '2 s (1 s per la latenza minima)',
  'obs.cpuPreset': 'Preset utilizzo CPU',
  'obs.cpuPresetValue': 'veryfast (o più veloce)',
  'obs.profile': 'Profilo',
  'obs.profileValue': 'main o high',
  'obs.tune': 'Tune',
  'obs.x264Options': 'Opzioni x264',
  'obs.preset': 'Preset',
  'obs.presetValue': 'P4 (equilibrato) o più veloce',
  'obs.tuning': 'Tuning',
  'obs.tuningValue': 'Bassa latenza o Latenza ultra bassa',
  'obs.multipass': 'Multipass',
  'obs.off': 'Disattivato',
  'obs.maxBframes': 'B-frame massimi',
  'obs.lookahead': 'Look-ahead',
  'obs.audio': 'Audio',
  'obs.audioValue': 'AAC 128–160 kbps, 48 kHz',
  'obs.video': 'Video',
  'obs.videoValue': '720p o 1080p, frame rate costante (30 o 60)',
  'obs.streamDelay': 'Ritardo flusso',
  'obs.streamDelayValue': 'Disattivato (Impostazioni › Avanzate)',

  // Broadcast detail: links and embed
  'links.embedPage': 'Pagina embed',
  'links.playerPage': 'Pagina player',
  'links.disabled': 'disattivato',
  'links.optsLegend': 'Opzioni embed',
  'links.autoplay': 'Avvio automatico',
  'links.muted': "Senza audio all'avvio",
  'links.controls': 'Controlli del player',
  'links.source': 'Sorgente',
  'links.sourceAuto': 'Automatica (consigliata)',
  'links.lang': 'Lingua del player',
  'links.langDefault': 'Predefinita',
  'links.langIt': 'Italiano',
  'links.langEn': 'Inglese',
  'links.autoplayHint': "I browser bloccano l'avvio automatico con l'audio: lascia «Senza audio» attivo, lo spettatore può attivarlo con un tocco.",
  'links.snippetHlsjs': 'Esempio con hls.js (LL-HLS)',
  'links.snippetMpegtsjs': 'Esempio con mpegts.js (HTTP-FLV)',
  'links.snippetDashjs': 'Esempio con dash.js (LL-DASH)',
  'links.snippetWhep': 'Esempio WebRTC (WHEP, senza librerie)',
  'links.copyCode': 'Copia codice',
  'links.iframeLabel': 'Codice iframe da incollare nel sito',
  'links.copyIframe': 'Copia iframe',
  'links.jsTitle': 'Integrazione con un player JavaScript',

  // Broadcast detail: recordings
  'rec.statusRecording': 'In registrazione',
  'rec.statusProcessing': 'In elaborazione',
  'rec.statusReady': 'Pronta',
  'rec.statusFailed': 'Errore',
  'rec.refresh': 'Aggiorna registrazioni',
  'rec.deleteTitle': 'Eliminare la registrazione?',
  'rec.deleteText': 'La registrazione del {date} ({size}) verrà cancellata dal disco.',
  'rec.deleted': 'Registrazione eliminata.',
  'rec.none': 'Nessuna registrazione',
  'rec.noneHint': 'Avvia «Registra ora» durante la diretta, oppure attiva la registrazione automatica in «Modifica».',
  'rec.delete': 'Elimina registrazione',
  'rec.colStart': 'Inizio',
  'rec.colDuration': 'Durata',
  'rec.colSize': 'Dimensione',
  'rec.colFormat': 'Formato',
  'rec.colActions': 'Azioni',
  'rec.flvHint': 'Le registrazioni FLV (senza ffmpeg sul server) non si riproducono nel browser: scaricale e aprile con VLC.',
  'rec.unavailable': 'Registrazioni non ancora disponibili',
  'rec.unavailableText': "Questa versione del server non gestisce ancora l'archivio delle registrazioni.",
  'rec.loadError': 'Impossibile caricare le registrazioni',

  // Broadcast detail: waiting video
  'waiting.uploadLabel': 'Carica un nuovo video di attesa',
  'waiting.textLabel': 'Testo di attesa',
  'waiting.textHint': "Mostrato quando non c'è un video di attesa. Se lo lasci vuoto viene usato il testo predefinito globale.",
  'waiting.textSaved': 'Testo di attesa salvato.',
  'waiting.none': 'Nessun video di attesa',
  'waiting.noneHint': 'Il player mostra il titolo e il testo di attesa su uno sfondo animato. Puoi caricare un video qui oppure impostarne uno predefinito per tutte le dirette in «Impostazioni».',
  'waiting.own': 'Video specifico di questa diretta.',
  'waiting.usingDefault': 'Nessun video specifico: è in uso il video predefinito globale (modificabile in «Impostazioni»).',
  'waiting.deleteTitle': 'Eliminare il video di attesa?',
  'waiting.deleteText': 'Il file verrà cancellato. Se è impostato, i player useranno il video predefinito globale.',
  'waiting.deleted': 'Video di attesa eliminato.',
  'waiting.lead': 'Il player lo mostra in loop, senza audio, quando la diretta è offline o in pausa, e passa da solo al live quando arriva il segnale.',
  'waiting.builtin': 'La diretta inizierà a breve',

  // Broadcast detail: edit dialog
  'edit.title': 'Modifica diretta',
  'edit.description': 'Descrizione',
  'edit.nameNote': 'Nome (non modificabile): ',
  'edit.saved': 'Modifiche salvate.',

  // Broadcast detail: metrics
  'metrics.range1h': '1h',
  'metrics.range6h': '6h',
  'metrics.range24h': '24h',
  'metrics.range7d': '7g',
  'metrics.rangeCustom': 'Personalizzato',
  'metrics.rangeLabel': 'Intervallo di tempo',
  'metrics.from': 'Dal',
  'metrics.to': 'Al',
  'metrics.apply': 'Applica',
  'metrics.refresh': 'Aggiorna metriche',
  'metrics.now': 'Spettatori ora',
  'metrics.peak': 'Picco',
  'metrics.sessions': 'Sessioni totali',
  'metrics.avgWatch': 'Durata media visione',
  'metrics.chartLabel': 'Grafico degli spettatori nel tempo',
  'metrics.empty': 'Nessuno spettatore nel periodo selezionato.',
  'metrics.showTable': 'Mostra i dati in tabella',
  'metrics.time': 'Ora',
  'metrics.legend': 'Serie visibili nel grafico',
  'metrics.rangeError': "L'inizio deve precedere la fine.",
  'metrics.unavailable': 'Metriche non ancora disponibili',
  'metrics.unavailableText': 'Questa versione del server non registra ancora lo storico degli spettatori. Il conteggio in tempo reale è nella sezione «Stato».',
  'metrics.loadError': 'Impossibile caricare le metriche',
  'metrics.peakAt': 'alle {when}',
  'metrics.liveSince': 'in diretta dalle {when}',
  'metrics.libError': 'Impossibile caricare la libreria dei grafici',
  'metrics.libErrorText': 'Controlla la connessione a Internet (Chart.js viene caricato da cdnjs).',
  'metrics.noData': 'Nessun dato.',

  // Embedded player (embed.html, player.js)
  'player.defaultTitle': 'Diretta',
  'player.docTitle': '{title} · Diretta',
  'player.waiting': 'In attesa della diretta',
  'player.paused': 'Diretta in pausa',
  'player.playing': 'Diretta in corso',
  'player.notFound': 'Diretta non trovata',
  'player.notFoundText': 'Controlla il link: questa diretta non esiste o è stata eliminata.',
  'player.loading': 'Caricamento della diretta',
  'player.live': 'LIVE',
  'player.latencyTitle': 'Ritardo stimato rispetto al live',
  'player.play': 'Riproduci',
  'player.goLive': 'Vai al live',
  'player.unmute': 'Attiva audio',
};

export const en = {
  // Page shell (index.html)
  'app.docTitle': 'rms · Live stream dashboard',
  'app.skip': 'Skip to content',
  'nav.brandLabel': 'rms, live stream list',
  'nav.label': 'Main navigation',
  'nav.broadcasts': 'Live streams',
  'nav.settings': 'Settings',
  'logout.title': 'Sign out of the dashboard',
  'logout.label': 'Sign out',
  'conn.connecting': 'Connecting…',
  'conn.live': 'Real time',
  'conn.polling': 'Reconnecting…',
  'conn.liveTitle': 'Real-time updates active',
  'conn.pollingTitle': 'Real-time updates unavailable: refreshing every 10 seconds',
  'boot.unreachable': 'Server unreachable',

  // Common words and actions
  'common.loading': 'Loading…',
  'common.cancel': 'Cancel',
  'common.confirm': 'Confirm',
  'common.close': 'Close',
  'common.save': 'Save',
  'common.saveText': 'Save text',
  'common.retry': 'Retry',
  'common.copy': 'Copy',
  'common.no': 'No',
  'common.rec': 'REC',
  'common.total': 'Total',
  'common.status': 'Status',
  'common.viewers': 'Viewers',
  'common.protocol': 'Protocol',
  'common.broadcast': 'Live stream',
  'common.bytesIn': 'Data received',
  'common.liveNow': 'Live streams on air',
  'action.pause': 'Pause',
  'action.resume': 'Resume',
  'action.edit': 'Edit',
  'action.delete': 'Delete',
  'action.deleteVideo': 'Delete video',
  'action.play': 'Play',
  'action.download': 'Download',
  'action.regenerate': 'Regenerate',
  'action.disconnect': 'Disconnect encoder',
  'action.recordNow': 'Record now',
  'action.stopRecording': 'Stop recording',

  // Broadcast status badges
  'status.live': 'Live',
  'status.paused': 'Paused',
  'status.offline': 'Offline',

  // Time and durations
  'time.since': 'for {duration}',
  'duration.days': '{d} d {h} h',
  'duration.hours': '{h} h {m} min',
  'duration.minutes': '{m} min {s} s',
  'duration.seconds': '{s} s',

  // Error messages
  'error.unknown': 'Unknown error.',
  'error.aborted': 'Operation cancelled.',
  'error.unexpected': 'Unexpected error: {message}',
  'error.network': 'Cannot reach the server. Check your connection.',
  'error.sessionExpired': 'Session expired: please sign in again.',
  'error.notImplemented': 'This feature is not available yet in this server version.',
  'error.server': 'Server error ({status}). Please try again shortly.',
  'error.request': 'Request failed ({status}): {message}',
  'error.broadcastNotFound': 'Live stream not found.',
  'error.alreadyExists': 'A live stream with this name already exists.',
  'error.titleRequired': 'The title is required.',
  'error.invalidName': "Invalid name: use only lowercase letters, digits, '-' and '_' (max 64 characters).",
  'error.nameImmutable': 'The name of a live stream cannot be changed.',
  'error.tooLong': 'Text too long.',
  'error.alreadyRecording': 'Recording is already in progress.',
  'error.notRecording': 'No recording in progress.',
  'error.notLive': 'The live stream is not active right now.',
  'error.fileTooLarge': 'The file exceeds the maximum allowed size.',
  'error.noFfmpeg': 'ffmpeg is not available on the server: upload an MP4 file (H.264/AAC).',
  'error.unsupportedType': 'Unsupported format: upload an MP4, MOV, M4V, WebM, MKV or AVI video.',
  'error.cannotProcess': 'Cannot process the video: the file is damaged or has no valid video track.',
  'error.timedOut': 'Video processing took too long and was stopped.',
  'error.emptyFile': 'The uploaded file is empty.',
  'error.noFreeName': 'Could not generate a free name: please enter one.',
  'error.invalidLanguage': 'Unsupported language.',

  // Toasts, dialogs, copy fields (core.js)
  'toast.close': 'Dismiss notification',
  'toast.copied': 'Copied to the clipboard.',
  'toast.copyFailed': 'Copy failed: select the text and press Ctrl+C.',
  'toast.copyFailedShort': 'Copy failed.',
  'field.showKey': 'Show key',
  'field.hideKey': 'Hide key',
  'field.copyLabel': 'Copy {label}',

  // Upload widget (waiting videos)
  'upload.defaultLabel': 'Upload a video',
  'upload.noFile': 'No file selected',
  'upload.choose': 'Choose file…',
  'upload.send': 'Upload',
  'upload.progressLabel': 'Upload progress',
  'upload.maxSize': 'Maximum size: {mb} MB.',
  'upload.limitsFfmpeg': 'Formats: MP4, MOV, M4V, WebM, MKV, AVI. {max} The video is converted to H.264/AAC MP4 and a preview image is created.',
  'upload.limitsNoFfmpeg': 'ffmpeg is not available on the server: upload an MP4 file (H.264/AAC), it will be used as is. {max}',
  'upload.unsupported': 'Unsupported format: choose a video file.',
  'upload.mp4Only': 'Without ffmpeg on the server only MP4 files (H.264/AAC) can be uploaded.',
  'upload.tooLarge': 'The file is too large ({size}): the maximum is {max} MB.',
  'upload.progressPct': 'Uploading: {pct}%',
  'upload.progress': 'Uploading: {pct}% ({loaded} of {total})',
  'upload.converting': 'File sent. Converting the video…',
  'upload.saving': 'File sent. Saving…',
  'upload.done': 'Waiting video updated.',
  'upload.toastDone': 'Waiting video uploaded.',

  // Broadcast flags (create and edit dialogs)
  'flags.legend': 'Options',
  'flags.record': 'Record automatically',
  'flags.recordHint': 'Recording starts by itself when the signal from OBS arrives.',
  'flags.flv': 'HTTP-FLV and WebSocket-FLV',
  'flags.flvHint': "1–2 s latency: the player's default choice on desktop and Android.",
  'flags.hls': 'LL-HLS',
  'flags.hlsHint': 'Required for iPhone/iPad and Safari; CDN friendly.',
  'flags.dash': 'LL-DASH',
  'flags.dashHint': 'Standard alternative for MSE-based players.',
  'flags.webrtc': 'WebRTC (WHEP)',
  'flags.webrtcHint': "Sub-second latency; the server's WebRTC UDP port must be reachable.",

  // Login
  'login.docTitle': 'Sign in · rms',
  'login.title': 'Live stream dashboard',
  'login.lead': 'Sign in to manage live streams, OBS settings and embed code.',
  'login.tokenLabel': 'Admin token',
  'login.tokenHint': "You can find it in the server's data/admin_token file (or in the log at first start).",
  'login.submit': 'Sign in',
  'login.emptyToken': 'Enter the token.',
  'login.invalidToken': 'Invalid token.',

  // Broadcast list
  'list.docTitle': 'Live streams · rms',
  'list.title': 'Live streams',
  'list.new': 'New live stream',
  'list.emptyTitle': 'No live streams yet',
  'list.emptyText': 'Create your first live stream: you will get the settings for OBS and the code to embed in your website.',
  'list.broadcasts.one': '{n} live stream',
  'list.broadcasts.other': '{n} live streams',
  'list.onAir': '{n} on air',
  'list.viewers.one': '{n} viewer',
  'list.viewers.other': '{n} viewers',
  'card.recTitle': 'Recording in progress',
  'card.viewersLabel.one': '{n} viewer',
  'card.viewersLabel.other': '{n} viewers',
  'card.embed': 'Embed',
  'card.embedTitle': 'Copy the iframe code to paste into your website',
  'card.embedLabel': 'Copy the embed code of {title}',
  'card.open': 'Open',
  'card.openLabel': 'Open {title}',
  'card.signalFor': 'on air for {duration}',
  'card.waitingSignal': 'Waiting for the signal from OBS',
  'card.pauseLabel': 'Pause {title}',
  'card.resumeLabel': 'Resume {title}',
  'toast.iframeCopied': 'Iframe code copied to the clipboard.',
  'toast.resumed': 'Live stream resumed.',
  'toast.paused': 'Live stream paused: players show the waiting video.',
  'toast.streamLive': '“{title}” is receiving the signal and is live.',
  'toast.streamOffline': '“{title}”: the signal was lost.',

  // New broadcast dialog
  'create.title': 'New live stream',
  'create.titleLabel': 'Title',
  'create.titlePlaceholder': 'E.g. Sunday service',
  'create.nameLabel': 'Name in the URL',
  'create.nameHint': 'Generated from the title; used in every link (lowercase letters, digits, “-” and “_”). It cannot be changed later.',
  'create.descLabel': 'Description (optional)',
  'create.waitingLabel': 'Waiting text (optional)',
  'create.waitingHint': 'Shown by the player before the start and during pauses. If left empty, the global default text is used (Settings).',
  'create.defaultPlaceholder': 'Default: {text}',
  'create.playerLink': 'Player link: ',
  'create.invalidName': 'Invalid name: use only lowercase letters, digits, “-” and “_” (max 64 characters).',
  'create.submit': 'Create live stream',
  'toast.created': 'Live stream “{title}” created.',

  // Settings page
  'settings.docTitle': 'Settings · rms',
  'settings.title': 'Settings',
  'settings.lead': 'Defaults for every live stream and information about the server.',
  'settings.languageSection': 'Language',
  'settings.language': 'Interface language',
  'settings.languageNote': 'Applies to the dashboard and to the texts of the embedded player (waiting screen, pause, buttons). A single embed can force the language with the ?lang=it or ?lang=en parameter.',
  'settings.languageSave': 'Save language',
  'settings.languageSaved': 'Language saved.',
  'lang.name.it': 'Italiano',
  'lang.name.en': 'English',
  'settings.waitingSection': 'Default waiting video',
  'settings.waitingLead': 'Played in a loop by the players of live streams without their own waiting video, while they are offline or paused.',
  'settings.uploadLabel': 'Upload the default video',
  'settings.textLabel': 'Default waiting text',
  'settings.textHint': 'Used by live streams without their own waiting text. Leave empty to restore “{text}”.',
  'settings.textSaved': 'Default text saved.',
  'settings.noVideo': 'No default video',
  'settings.noVideoHint': 'Live streams without their own video show the title and the waiting text on an animated background.',
  'settings.videoHint': 'Used by every live stream without its own waiting video.',
  'settings.deleteVideoTitle': 'Delete the default video?',
  'settings.deleteVideoText': 'Live streams without their own video will show the waiting text.',
  'settings.videoDeleted': 'Default video deleted.',
  'settings.overview': 'Overview',
  'settings.totalViewers': 'Total viewers',
  'settings.noBroadcasts': 'No live streams.',
  'settings.system': 'System',
  'system.ffmpegMissingTitle': 'ffmpeg is not available on the server. ',
  'system.ffmpegMissingText': 'The waiting video must be an MP4 file (H.264/AAC) and is used as is (without a preview image); recordings stay in FLV format. Install ffmpeg or set its path in [ffmpeg] path.',
  'system.version': 'Version',
  'system.uptime': 'Running for',
  'system.ffmpeg': 'ffmpeg',
  'system.available': 'available',
  'system.unavailable': 'not available',
  'system.ffmpegPath': 'ffmpeg path',
  'system.rtmpBind': 'RTMP listening on',
  'system.httpBind': 'HTTP listening on',
  'system.publicUrl': 'Public URL',
  'system.publicUrlDerived': 'derived from the request (Host)',
  'system.webrtc': 'WebRTC (WHEP)',
  'system.iceAddresses': 'Advertised ICE addresses',
  'system.rtcDisabled': 'disabled',
  'system.rtcUnavailableReason': 'not available: {error}',
  'system.rtcStatus': 'UDP port {port} · {sessions} sessions',
  'system.rtcVideoOnly': 'video only (ffmpeg missing)',

  // Broadcast detail: header and navigation
  'detail.notFound': 'Live stream not found',
  'detail.notFoundText': 'There is no live stream called “{name}” (it may have been deleted).',
  'detail.back': 'Back to live streams',
  'detail.viewersTitle': 'Connected viewers',
  'detail.viewersLabel.one': '{n} connected viewer',
  'detail.viewersLabel.other': '{n} connected viewers',
  'detail.openPlayer': 'Open player',
  'detail.sections': 'Live stream sections',
  'detail.nav.preview': 'Preview',
  'detail.nav.obs': 'OBS',
  'detail.nav.links': 'Links and embed',
  'detail.nav.metrics': 'Metrics',
  'detail.nav.recordings': 'Recordings',
  'detail.nav.waiting': 'Waiting video',
  'detail.deletedElsewhere': 'This live stream has been deleted.',

  // Broadcast detail: status panel
  'state.since': 'Live since',
  'state.ip': 'Encoder IP',
  'state.video': 'Video',
  'state.audio': 'Audio',
  'state.bitrate': 'Incoming bitrate',
  'state.recording': 'Recording',
  'state.pausedHint': ' The signal is arriving but players show the waiting video.',
  'state.measuring': 'measuring…',
  'state.measured': '{rate} (measured)',
  'state.recSince': 'In progress since {time}',
  'state.resumeTitle': 'Show the live stream to players again',
  'state.pauseTitle': 'Show the waiting video to players without stopping OBS',
  'state.recDisabledTitle': 'Available when the live stream receives the signal',
  'state.byProtocol': 'Viewers by protocol',
  'state.sessions': 'Connected sessions ',
  'state.colIp': 'IP',
  'state.colClient': 'Client',
  'state.colConnected': 'Connected',
  'state.noSessions': 'No viewers connected.',
  'state.disconnectTitle': 'Disconnect the encoder?',
  'state.disconnectText': 'OBS will be disconnected from the server. If it is set to reconnect, it will start again by itself after a few seconds: to really stop it, regenerate the key or pause the live stream.',
  'state.disconnectOk': 'Disconnect',
  'state.disconnected': 'Encoder disconnected.',
  'state.deleteTitle': 'Delete the live stream?',
  'state.deleteText': 'The live stream “{title}” will be permanently deleted together with its waiting video. A connected encoder is disconnected; recordings remain available.',
  'state.deleteOk': 'Delete live stream',
  'state.deleted': 'Live stream deleted.',
  'state.resumed': 'Live stream resumed: players go back to the live stream.',
  'state.recStopped': 'Recording stopped.',
  'state.recStarted': 'Recording started.',

  // Broadcast detail: live preview
  'preview.title': 'Live preview',
  'preview.reload': 'Reload preview',
  'preview.turnOff': 'Turn off',
  'preview.turnOn': 'Turn on',
  'preview.off': 'Preview turned off.',
  'preview.frameTitle': 'Live stream preview',
  'preview.hint': 'This is the embed player as viewers see it (muted). The preview counts as one viewer.',

  // Broadcast detail: OBS settings
  'obs.title': 'OBS settings',
  'obs.server': 'Server',
  'obs.serverHint': 'In OBS: Settings › Stream › Service “Custom”.',
  'obs.key': 'Stream key',
  'obs.keyHint': 'Secret: anyone who knows it can broadcast on this live stream.',
  'obs.regenTitle': 'Generate a new stream key',
  'obs.regenConfirmTitle': 'Regenerate the stream key?',
  'obs.regenConfirmText': 'The current key will no longer be accepted: you will have to paste the new one into OBS. An encoder that is already connected stays connected until it disconnects.',
  'obs.regenConfirmOk': 'Regenerate key',
  'obs.regenerated': 'New key generated: update it in OBS.',
  'obs.serverNote': 'The server accepts any application name as long as the key is correct. H.264 + AAC (recommended) are supported and, for HTTP-FLV only, HEVC/AV1 with Enhanced RTMP.',
  'obs.recoTitle': 'Recommended settings for low latency',
  'obs.encoder': 'Encoder',
  'obs.keyframeNote': 'In OBS the default keyframe interval is “0” (automatic, up to 8 s): set it explicitly, otherwise LL-HLS and LL-DASH will have high latency.',
  'obs.gopWarnTitle': 'Keyframe interval too long: {secs} s. ',
  'obs.gopWarnText': 'In OBS set “Keyframe Interval” to 2 s (or 1 s): with long GOPs LL-HLS and LL-DASH have high latency and start slowly.',
  'obs.outputMode': 'Output mode',
  'obs.outputModeValue': 'Advanced (Settings › Output)',
  'obs.rateControl': 'Rate control',
  'obs.bitrate': 'Bitrate',
  'obs.bitrateValue': '2500–6000 kbps, depending on your upload bandwidth (leave headroom)',
  'obs.keyframe': 'Keyframe interval',
  'obs.keyframeValue': '2 s (1 s for minimum latency)',
  'obs.cpuPreset': 'CPU usage preset',
  'obs.cpuPresetValue': 'veryfast (or faster)',
  'obs.profile': 'Profile',
  'obs.profileValue': 'main or high',
  'obs.tune': 'Tune',
  'obs.x264Options': 'x264 options',
  'obs.preset': 'Preset',
  'obs.presetValue': 'P4 (balanced) or faster',
  'obs.tuning': 'Tuning',
  'obs.tuningValue': 'Low latency or Ultra low latency',
  'obs.multipass': 'Multipass',
  'obs.off': 'Disabled',
  'obs.maxBframes': 'Max B-frames',
  'obs.lookahead': 'Look-ahead',
  'obs.audio': 'Audio',
  'obs.audioValue': 'AAC 128–160 kbps, 48 kHz',
  'obs.video': 'Video',
  'obs.videoValue': '720p or 1080p, constant frame rate (30 or 60)',
  'obs.streamDelay': 'Stream delay',
  'obs.streamDelayValue': 'Disabled (Settings › Advanced)',

  // Broadcast detail: links and embed
  'links.embedPage': 'Embed page',
  'links.playerPage': 'Player page',
  'links.disabled': 'disabled',
  'links.optsLegend': 'Embed options',
  'links.autoplay': 'Autoplay',
  'links.muted': 'Muted at start',
  'links.controls': 'Player controls',
  'links.source': 'Source',
  'links.sourceAuto': 'Automatic (recommended)',
  'links.lang': 'Player language',
  'links.langDefault': 'Default',
  'links.langIt': 'Italian',
  'links.langEn': 'English',
  'links.autoplayHint': 'Browsers block autoplay with sound: keep “Muted at start” on, viewers can turn the sound on with one tap.',
  'links.snippetHlsjs': 'Example with hls.js (LL-HLS)',
  'links.snippetMpegtsjs': 'Example with mpegts.js (HTTP-FLV)',
  'links.snippetDashjs': 'Example with dash.js (LL-DASH)',
  'links.snippetWhep': 'WebRTC example (WHEP, no libraries)',
  'links.copyCode': 'Copy code',
  'links.iframeLabel': 'Iframe code to paste into your website',
  'links.copyIframe': 'Copy iframe',
  'links.jsTitle': 'Integration with a JavaScript player',

  // Broadcast detail: recordings
  'rec.statusRecording': 'Recording',
  'rec.statusProcessing': 'Processing',
  'rec.statusReady': 'Ready',
  'rec.statusFailed': 'Error',
  'rec.refresh': 'Refresh recordings',
  'rec.deleteTitle': 'Delete the recording?',
  'rec.deleteText': 'The recording of {date} ({size}) will be deleted from the disk.',
  'rec.deleted': 'Recording deleted.',
  'rec.none': 'No recordings',
  'rec.noneHint': 'Use “Record now” during the live stream, or turn on automatic recording in “Edit”.',
  'rec.delete': 'Delete recording',
  'rec.colStart': 'Start',
  'rec.colDuration': 'Duration',
  'rec.colSize': 'Size',
  'rec.colFormat': 'Format',
  'rec.colActions': 'Actions',
  'rec.flvHint': 'FLV recordings (no ffmpeg on the server) cannot be played in the browser: download them and open them with VLC.',
  'rec.unavailable': 'Recordings not available yet',
  'rec.unavailableText': 'This server version does not manage the recording archive yet.',
  'rec.loadError': 'Cannot load the recordings',

  // Broadcast detail: waiting video
  'waiting.uploadLabel': 'Upload a new waiting video',
  'waiting.textLabel': 'Waiting text',
  'waiting.textHint': 'Shown when there is no waiting video. If left empty, the global default text is used.',
  'waiting.textSaved': 'Waiting text saved.',
  'waiting.none': 'No waiting video',
  'waiting.noneHint': 'The player shows the title and the waiting text on an animated background. You can upload a video here or set a default one for every live stream in “Settings”.',
  'waiting.own': 'Video specific to this live stream.',
  'waiting.usingDefault': 'No specific video: the global default video is in use (editable in “Settings”).',
  'waiting.deleteTitle': 'Delete the waiting video?',
  'waiting.deleteText': 'The file will be deleted. If one is set, players will use the global default video.',
  'waiting.deleted': 'Waiting video deleted.',
  'waiting.lead': 'The player loops it, muted, while the live stream is offline or paused, and switches to the live stream by itself when the signal arrives.',
  'waiting.builtin': 'The live stream will start shortly',

  // Broadcast detail: edit dialog
  'edit.title': 'Edit live stream',
  'edit.description': 'Description',
  'edit.nameNote': 'Name (cannot be changed): ',
  'edit.saved': 'Changes saved.',

  // Broadcast detail: metrics
  'metrics.range1h': '1h',
  'metrics.range6h': '6h',
  'metrics.range24h': '24h',
  'metrics.range7d': '7d',
  'metrics.rangeCustom': 'Custom',
  'metrics.rangeLabel': 'Time range',
  'metrics.from': 'From',
  'metrics.to': 'To',
  'metrics.apply': 'Apply',
  'metrics.refresh': 'Refresh metrics',
  'metrics.now': 'Viewers now',
  'metrics.peak': 'Peak',
  'metrics.sessions': 'Total sessions',
  'metrics.avgWatch': 'Average watch time',
  'metrics.chartLabel': 'Chart of viewers over time',
  'metrics.empty': 'No viewers in the selected period.',
  'metrics.showTable': 'Show the data as a table',
  'metrics.time': 'Time',
  'metrics.legend': 'Series shown in the chart',
  'metrics.rangeError': 'The start must be before the end.',
  'metrics.unavailable': 'Metrics not available yet',
  'metrics.unavailableText': 'This server version does not record the viewer history yet. The real-time count is in the “Status” section.',
  'metrics.loadError': 'Cannot load the metrics',
  'metrics.peakAt': 'at {when}',
  'metrics.liveSince': 'live since {when}',
  'metrics.libError': 'Cannot load the chart library',
  'metrics.libErrorText': 'Check the Internet connection (Chart.js is loaded from cdnjs).',
  'metrics.noData': 'No data.',

  // Embedded player (embed.html, player.js)
  'player.defaultTitle': 'Live stream',
  'player.docTitle': '{title} · Live stream',
  'player.waiting': 'Waiting for the live stream',
  'player.paused': 'Live stream paused',
  'player.playing': 'Live stream playing',
  'player.notFound': 'Live stream not found',
  'player.notFoundText': 'Check the link: this live stream does not exist or has been deleted.',
  'player.loading': 'Loading the live stream',
  'player.live': 'LIVE',
  'player.latencyTitle': 'Estimated delay behind live',
  'player.play': 'Play',
  'player.goLive': 'Go to live',
  'player.unmute': 'Unmute',
};

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

const DICTS = { it, en };
const LOCALES = { it: 'it-IT', en: 'en-US' };
const has = (obj, key) => Object.prototype.hasOwnProperty.call(obj, key);

let current = 'it';
const listeners = new Set();
const warned = new Set();

/** `"it"` / `"en"` (case-insensitive), else null. */
export function normalizeLanguage(value) {
  const v = String(value == null ? '' : value).trim().toLowerCase();
  return has(DICTS, v) ? v : null;
}

export function getLanguage() { return current; }

/** BCP 47 locale of the current language for Intl (`it-IT` / `en-US`). */
export function getLocale() { return LOCALES[current]; }

/**
 * Switch language (unknown values mean Italian); updates `<html lang>`. Listeners run
 * only on a real change. Returns true when the language changed.
 */
export function setLanguage(lang) {
  const next = normalizeLanguage(lang) || 'it';
  if (typeof document !== 'undefined' && document.documentElement) document.documentElement.lang = next;
  if (next === current) return false;
  current = next;
  for (const fn of [...listeners]) {
    try { fn(next); } catch (e) { console.error(e); }
  }
  return true;
}

/** Call `fn(lang)` after every language change. Returns an unsubscribe function. */
export function onLanguageChange(fn) {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

function interpolate(text, params) {
  return text.replace(/\{(\w+)\}/g, (m, k) => (params[k] != null ? String(params[k]) : m));
}

/** Text of `key` in the current language (Italian fallback, then the key itself). */
export function t(key, params) {
  let text = has(DICTS[current], key) ? DICTS[current][key] : undefined;
  if (text === undefined) {
    if (!warned.has(current + ':' + key)) {
      warned.add(current + ':' + key);
      console.warn(`[i18n] missing ${current} text for "${key}"`);
    }
    text = has(it, key) ? it[key] : key;
  }
  return params ? interpolate(text, params) : text;
}

/** Plural: `${key}.one` or `${key}.other` according to `n` in the current locale. */
export function tn(key, n, params) {
  let form = 'other';
  try { form = new Intl.PluralRules(getLocale()).select(Number(n)) === 'one' ? 'one' : 'other'; } catch (_) { /* other */ }
  return t(`${key}.${form}`, { n, ...(params || {}) });
}

/** Whether `text` is the built-in waiting text of any language (server default). */
export function isBuiltinWaitingText(text) {
  const v = String(text == null ? '' : text).trim();
  return Object.values(DICTS).some((d) => d['waiting.builtin'] === v);
}

const DOM_ATTRS = [['data-i18n-title', 'title'], ['data-i18n-aria-label', 'aria-label'], ['data-i18n-placeholder', 'placeholder']];

/** Resolve data-i18n (text) and data-i18n-{title,aria-label,placeholder} under `root`. */
export function translateDom(root = document) {
  for (const el of root.querySelectorAll('[data-i18n]')) el.textContent = t(el.getAttribute('data-i18n'));
  for (const [data, attr] of DOM_ATTRS) {
    for (const el of root.querySelectorAll(`[${data}]`)) el.setAttribute(attr, t(el.getAttribute(data)));
  }
}

// ---------------------------------------------------------------------------
// Formatting (Intl, current locale)
// ---------------------------------------------------------------------------

const formatters = new Map();
function formatter(kind, opts) {
  const key = `${kind}|${current}|${JSON.stringify(opts || {})}`;
  let f = formatters.get(key);
  if (!f) {
    f = kind === 'n' ? new Intl.NumberFormat(getLocale(), opts) : new Intl.DateTimeFormat(getLocale(), opts);
    formatters.set(key, f);
  }
  return f;
}

/** Number in the current locale ('—' when not a finite number). */
export function formatNumber(n, opts = { maximumFractionDigits: 0 }) {
  if (n == null || n === '' || !Number.isFinite(Number(n))) return '—';
  return formatter('n', opts).format(Number(n));
}

/** Date/time (Date, timestamp or ISO string) in the current locale ('—' when invalid). */
export function formatDate(value, opts = { dateStyle: 'short', timeStyle: 'medium' }) {
  if (value == null || value === '') return '—';
  const d = value instanceof Date ? value : new Date(value);
  return Number.isNaN(d.getTime()) ? '—' : formatter('d', opts).format(d);
}

/** Decimal byte size: "1,4 GB" / "1.4 GB". */
export function formatBytes(n) {
  if (n == null || !Number.isFinite(Number(n))) return '—';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let v = Number(n);
  let i = 0;
  while (v >= 1000 && i < units.length - 1) { v /= 1000; i++; }
  return `${formatNumber(v, { maximumFractionDigits: i === 0 ? 0 : 1 })} ${units[i]}`;
}

/** Duration in seconds: "12 min 05 s", "1 h 02 min", "2 g 3 h" / "2 d 3 h". */
export function formatDuration(secs) {
  if (secs == null || !Number.isFinite(Number(secs))) return '—';
  let s = Math.max(0, Math.round(Number(secs)));
  const d = Math.floor(s / 86400); s -= d * 86400;
  const hh = Math.floor(s / 3600); s -= hh * 3600;
  const mm = Math.floor(s / 60); s -= mm * 60;
  const pad = (v) => String(v).padStart(2, '0');
  if (d) return t('duration.days', { d, h: hh });
  if (hh) return t('duration.hours', { h: hh, m: pad(mm) });
  if (mm) return t('duration.minutes', { m: mm, s: pad(s) });
  return t('duration.seconds', { s });
}

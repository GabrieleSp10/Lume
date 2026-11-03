# Lume

Lettore musicale per Windows che riproduce la tua musica da **Spotify** o da **YouTube**, con testi sincronizzati, vista immersiva, coda, temi e playlist importate.

> Progetto personale, non affiliato a Spotify né a YouTube/Google.

## Installazione

1. Apri la pagina **Releases** di questo repository e scarica `Lume-Setup-<versione>.exe`.
2. Avvialo: Lume si installa per il tuo utente (non servono permessi di amministratore) e compare nel menu Start.
3. Al primo avvio scegli la lingua e da dove ascoltare.

Per disinstallarlo: **Impostazioni di Windows › App › App installate › Lume**. Le impostazioni e la libreria restano sul computer finché non le cancelli.

Requisiti: Windows 10 o 11 a 64 bit, con WebView2 (già presente in Windows 11).

## Ascoltare da Spotify

Serve **Spotify Premium** e una tua app Spotify gratuita (ogni persona usa la propria):

1. Vai su <https://developer.spotify.com/dashboard> e crea un'app, attivando **Web API** e **Web Playback SDK**.
2. Come **Redirect URI** inserisci `http://127.0.0.1:8898/`.
3. Copia il **Client ID** e incollalo in Lume (primo avvio o Impostazioni › Avanzate), poi premi **Accedi**.

## Ascoltare da YouTube

Non serve installare niente a mano: quando scegli YouTube, Lume scarica da solo [yt-dlp](https://github.com/yt-dlp/yt-dlp) e [Deno](https://deno.com/) (circa 115 MB, dalle loro pagine ufficiali su GitHub, controllandone l'integrità) e tiene yt-dlp aggiornato.

Poi premi **Collega account YouTube** e accedi con il tuo account Google: evita i blocchi «non sei un bot».

L'accesso a YouTube resta salvato solo sul tuo computer; «Scollega» lo cancella. Usare yt-dlp va contro le condizioni d'uso di YouTube: fallo a tuo rischio.

## Le tue playlist

- **Nuova playlist** (pagina Playlist): scegli un nome e, se vuoi, un'immagine; senza immagine Lume usa le copertine dei primi 4 brani.
- Dal menu **…** di ogni brano (o con il tasto destro) lo aggiungi alla coda o a una playlist, lo togli dalla playlist che stai guardando o, se hai attivato l'ascolto offline, lo scarichi.
- **Crea copia locale** su una playlist di Spotify: Lume cerca gli stessi brani su YouTube e crea una tua playlist con quelli. Da Spotify prende solo titoli e artisti, nient'altro.

## Ascolto offline

È **disattivato** finché non lo attivi tu in **Impostazioni › Ascolto offline**. Poi puoi scaricare un brano dal suo menu **…** o un'intera playlist con **Scarica**.

- Si scaricano **solo brani di YouTube**, mai da Spotify, e restano dentro Lume (`%LOCALAPPDATA%\Lume\data\offline`), che li riproduce anche senza connessione.
- **Qualità**: «Alta» (la migliore di YouTube, circa 4 MB a brano) o «Risparmio» (circa la metà).
- Lume scarica un brano alla volta, con pause tra l'uno e l'altro; con un account YouTube collegato puoi attivare **Download veloce** (3 alla volta). Se YouTube chiede di confermare che non sei un bot, si ferma, riprende dopo un quarto d'ora e torna a un brano alla volta.
- **Le condizioni d'uso di YouTube non consentono di scaricare i contenuti**, salvo dove lo offre YouTube stesso. Se attivi l'ascolto offline lo fai sotto la tua responsabilità, e solo per uso personale.

## In sottofondo

Con **Impostazioni › Generali › Continua quando chiudi** attivo, chiudendo la finestra la musica continua e Lume resta nell'area di notifica: un clic sull'icona la riapre, il tasto destro offre «Riproduci o pausa» ed «Esci». È disattivato all'inizio.

## Se qualcosa non va

- **VPN o filtri DNS** (es. Norton VPN) possono impedire di raggiungere Spotify, i testi o YouTube: prova a disattivarli o a escludere Lume.
- **«Testo senza tempi»**: il sito dei testi sincronizzati è occupato; Lume riprova da solo dopo due minuti.

## Per sviluppatori

Rust (stabile) e Windows. Avvio: `cargo run --release -p app_ui --bin lume`. Installatore: `cargo build --release -p app_ui --bin lume`, poi `ISCC.exe installer\lume.iss` (Inno Setup 6).

- `crates/core` – configurazione, accesso e API di Spotify, limitatore delle richieste, cache, libreria locale, testi
- `crates/player` – motori di riproduzione (Spotify Web Playback SDK, YouTube tramite yt-dlp, librespot sperimentale), server locale
- `crates/ui` – interfaccia in [Slint](https://slint.dev)

Componenti di terze parti: [hls.js](https://github.com/video-dev/hls.js) (Apache-2.0, licenza in `crates/player/src/vendor/`), Slint e le librerie elencate in `Cargo.lock` con le rispettive licenze.

## Avvertenze

- Lume è un progetto gratuito e senza scopo di lucro. Non è affiliato né approvato da Spotify, YouTube o Google; i loro nomi servono solo a dire con quali servizi funziona.
- Lume non contiene né distribuisce musica e non ha server: tutto avviene sul computer di chi lo usa, con i suoi account. yt-dlp e Deno si scaricano dalle loro pagine ufficiali solo se scegli YouTube.
- Ognuno è responsabile di come lo usa e del rispetto delle condizioni dei servizi a cui si collega.
- Se ritieni che questo progetto violi un tuo diritto, apri una segnalazione (issue) su GitHub: verrà esaminata subito.

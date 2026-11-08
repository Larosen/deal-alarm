# 🔔 Deal-Alarm – expert & Euronics

Überwacht Geräte zyklisch (standardmäßig etwa alle 2 Stunden) bei **expert** und **Euronics**
und schickt per [ntfy](https://ntfy.sh) einen **Dealalarm**, sobald ein Gerät zum vorher festgelegten
Wunschpreis (oder günstiger) zu haben ist. Bedient wird alles über eine Weboberfläche.

Die expert-Abfrage basiert auf dem
[expert-checker reborn](https://github.com/DJGorex/expert-checker) (Credits: MyDealz @Barney & @Gorex)
und durchsucht auf Wunsch alle expert-Filialen inkl. Direktabzug/Aktionspreis und Versandkosten.

## Funktionen

- **Ein Artikel, beide Shops:** Suchbegriff, EAN oder eine Produkt-URL von expert, Euronics oder media@home
  eingeben – der Deal-Alarm ordnet den Artikel über die EAN automatisch beiden Shops zu und prüft sie zusammen
- Watchlist mit Wunschpreis pro Gerät, pausierbar, „Jetzt prüfen“ pro Gerät oder für alle
- **expert**: Produkt-URL, Webcode oder Suchbegriff (mit Suchauswahl in der Oberfläche)
  - **jede Filiale wird einzeln abgefragt** – jede Niederlassung hat ihre eigenen Preise
  - führt expert denselben Artikel unter mehreren Artikelnummern (Webcodes), werden alle geprüft –
    deren Preise unterscheiden sich teils deutlich
  - Vergleichstabelle „Alle Filialpreise“ mit Preis, Versand, Gesamtpreis, Bestand und Prüfzeitpunkt
  - optional Ausstellungsstücke und nur lokal verfügbare Angebote im Umkreis deiner PLZ
- **Euronics & media@home**: Onlineshop und **jeder Markt einzeln**
  - Produkt-URL (Onlineshop oder ein Markt) oder Suche über die EAN (wird automatisch übernommen)
  - **Händlerverzeichnis mit allen 361 Shops** (300 euronics.de, 61 media@home) aus der Euronics-Händlersuche,
    wöchentlich aktualisiert; optional nur Märkte im Umkreis deiner PLZ
  - eigene Märkte zusätzlich eintragbar, z. B. `https://www.euronics.de/hamburg-buddenhagen`
  - eigenes Rate-Limit pro Domain (euronics.de und mediaathome.de)
- Zeitplan mit einstellbarem Intervall (Standard 120 Min.) und Zufallsabweichung (Standard ± 10 Min.)
- ntfy mit eigenem Server, Topic, optionalem Access-Token und Priorität; Testnachricht-Button
- Kein Benachrichtigungs-Spam: Pro Gerät und Shop gibt es einen Alarm, wenn der Wunschpreis erreicht wird.
  Ein weiterer kommt erst, wenn der Preis nochmals fällt oder zwischenzeitlich wieder über dem Wunschpreis lag.
- Optionaler Passwortschutz (HTTP Basic Auth) über `UI_PASSWORD`

## Filial-Scanner und Rate-Limit

Jede Filiale wird einzeln abgefragt. Damit auch strenge Rate-Limits nicht zum Problem werden:

- **Adaptives Tempo pro Shop:** Anfragen werden gleichmäßig verteilt (einstellbar in Anfragen/Minute).
  Bei Drosselung (HTTP 429/503, bei Euronics auch WAF-403) halbiert sich das Tempo, `Retry-After`
  wird beachtet und es gibt eine wachsende Pause. Nach 25 erfolgreichen Anfragen steigt das Tempo wieder.
- **Gedächtnis pro Filiale:** Der letzte Preis jeder Filiale wird gespeichert. Bricht ein Scan wegen
  Drosselung ab, geht nichts verloren – der nächste Zyklus macht weiter.
- **Rotierendes Budget:** Optional nur *N* Filialen pro Zyklus. Bis zur Hälfte des Budgets geht an die
  bisher günstigsten Filialen (damit ein Deal auf aktuellen Daten beruht), der Rest rotiert durch
  ungeprüfte und am längsten nicht geprüfte Filialen. So sind über mehrere Zyklen alle Filialen abgedeckt.
- **Gültigkeit:** Filialpreise, die älter als die eingestellte Zeit sind (Standard 24 h), zählen nicht
  mehr für den Dealalarm und werden in der Tabelle als „veraltet“ markiert.

Bei expert dauert ein voller Scan aller ~350 Filialen mit Standardeinstellungen etwa eine Minute pro
Artikelnummer (z. B. ~5 Minuten für einen Artikel, den expert unter 4 Webcodes führt).

## Start mit Docker (empfohlen)

```bash
git clone https://github.com/Larosen/deal-alarm.git
cd deal-alarm
docker compose up -d --build
```

Danach <http://localhost:8080> öffnen, unter **Einstellungen** das ntfy-Topic eintragen,
„Testnachricht senden“ klicken und Geräte hinzufügen. Die Daten liegen in `./data/state.json`.

## Start auf Unraid

Das Image wird bei jedem Push auf `main` von GitHub Actions gebaut und als
`ghcr.io/larosen/deal-alarm:latest` veröffentlicht. Da das Repository privat ist, muss Unraid
einmal bei der GitHub Container Registry angemeldet sein (Terminal, Token mit `read:packages`):

```bash
docker login ghcr.io -u <github-benutzer>
```

Template einrichten:

1. Unraid-Terminal öffnen und den Inhalt von [`unraid/deal-alarm.xml`](unraid/deal-alarm.xml) speichern als
   `/boot/config/plugins/dockerMan/templates-user/my-deal-alarm.xml`.
2. **Docker → Add Container → Template:** „deal-alarm“ auswählen.
3. Port (Standard 8080), Datenpfad (Standard `/mnt/user/appdata/deal-alarm`) und optional ein Passwort
   prüfen, dann **Apply**.
4. Über **WebUI** öffnen und unter **Einstellungen** ntfy-Topic und PLZ eintragen.

Updates: In der Docker-Übersicht „Check for Updates“ – Unraid zieht dann das neue `latest`-Image.

## Start ohne Docker

```bash
python3 -m venv .venv
.venv/bin/pip install -r requirements.txt
.venv/bin/python -m dealalarm.app
```

| Umgebungsvariable | Standard | Bedeutung |
|---|---|---|
| `PORT` | `8080` | Port der Weboberfläche |
| `HOST` | `0.0.0.0` | Bind-Adresse |
| `DATA_DIR` | `./data` | Ablage für Einstellungen, Watchlist und Ergebnisse |
| `UI_PASSWORD` | – | Aktiviert Basic Auth (Benutzer `UI_USER`, Standard `admin`) |
| `LOG_LEVEL` | `INFO` | Log-Level |

## ntfy einrichten

1. ntfy-App (Android/iOS) installieren oder <https://ntfy.sh/app> öffnen.
2. Ein schwer zu erratendes Topic abonnieren, z. B. `dealalarm-7f3k2q`.
3. Dasselbe Topic in den Einstellungen eintragen. Für einen eigenen Server mit Zugriffsschutz
   Server-URL und Access-Token angeben.

Der Dealalarm enthält Gerät, Preis, Wunschpreis und Filiale. Ein Tipp auf die Nachricht öffnet das Angebot.

## Hinweise

- **Euronics** sperrt Anfragen aus Rechenzentren (Cloud-Server, VPS) häufig mit HTTP 403. Der Deal-Alarm
  sollte deshalb zuhause laufen (z. B. Raspberry Pi, NAS, Heimserver). Ist Euronics nicht erreichbar,
  wird das am Gerät angezeigt, ohne dass die expert-Prüfung darunter leidet.
- Der Euronics-Preis wird aus den strukturierten Produktdaten der Seite (schema.org/JSON-LD bzw.
  Meta-Tags) gelesen. Liefert die EAN-Suche kein eindeutiges Produkt, bitte die direkte Produkt-URL eintragen.
- **Euronics-Märkte:** Jeder Markt hat einen eigenen Shop. Die URL eines Produkts im Markt ist
  `<markt-url>/<produktpfad>`, der Produktpfad ist überall gleich und endet mit der EAN. Führt ein Markt das
  Produkt nicht (404 oder Weiterleitung aus dem Markt heraus), gilt es dort als nicht verfügbar.
  Händler mit mehreren Filialen (z. B. Buddenhagen) haben meist einen gemeinsamen Shop – ein Eintrag genügt.
- Bei Euronics werden keine Versandkosten ausgelesen; verglichen wird der Artikelpreis.
- **Händlerverzeichnis:** Die Euronics-Händlersuche lädt ihre Märkte über den Kartendienst Woosmap. Diese
  Daten sammelt das offene Projekt [All The Places](https://www.alltheplaces.xyz) (Spider `euronics_de`,
  Lizenz CC0). Der Deal-Alarm lädt sie wöchentlich von dort; ist das nicht möglich, wird die mitgelieferte
  Momentaufnahme `dealalarm/shops/euronics_markets.json` verwendet.
- **Dauer:** Bei 6 Anfragen/Minute dauert ein vollständiger Durchlauf durch alle ~300 euronics.de-Shops
  ca. 50 Minuten pro Gerät (media@home läuft parallel). Für mehrere Geräte den Umkreis begrenzen oder
  „Märkte pro Zyklus“ setzen – dann rotiert der Scan über mehrere Zyklen durch alle Märkte.
- Die Euronics-Suche über die EAN (wenn keine Euronics-URL hinterlegt ist) nutzt `euronics.de/search?sSearch=<EAN>`.
  Findet sie nichts, bitte einmal die Euronics-Produkt-URL einfügen.
- Eine Prüfung über alle expert-Filialen dauert ca. eine Minute pro Artikelnummer. Bitte das Intervall nicht zu
  klein wählen (Minimum 15 Minuten), damit die Shops nicht unnötig belastet werden.

## Entwicklung

```bash
.venv/bin/pip install -r requirements-dev.txt
.venv/bin/python -m pytest
```

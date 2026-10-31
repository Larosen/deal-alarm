# 🔔 Deal-Alarm – expert & Euronics

Überwacht Geräte zyklisch (standardmäßig etwa alle 2 Stunden) bei **expert** und **Euronics**
und schickt per [ntfy](https://ntfy.sh) einen **Dealalarm**, sobald ein Gerät zum vorher festgelegten
Wunschpreis (oder günstiger) zu haben ist. Bedient wird alles über eine Weboberfläche.

Die expert-Abfrage basiert auf dem
[expert-checker reborn](https://github.com/DJGorex/expert-checker) (Credits: MyDealz @Barney & @Gorex)
und durchsucht auf Wunsch alle expert-Filialen inkl. Direktabzug/Aktionspreis und Versandkosten.

## Funktionen

- Watchlist mit Wunschpreis pro Gerät, pausierbar, „Jetzt prüfen“ pro Gerät oder für alle
- **expert**: Produkt-URL, Webcode oder Suchbegriff (mit Suchauswahl in der Oberfläche)
  - **jede Filiale wird einzeln abgefragt** – jede Niederlassung hat ihre eigenen Preise
  - Vergleichstabelle „Alle Filialpreise“ mit Preis, Versand, Gesamtpreis, Bestand und Prüfzeitpunkt
  - optional Ausstellungsstücke und nur lokal verfügbare Angebote im Umkreis deiner PLZ
- **Euronics**: Produkt-URL oder Suche über die EAN (die EAN wird automatisch von expert übernommen)
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

Bei expert dauert ein voller Scan aller ~350 Filialen mit Standardeinstellungen etwa eine Minute.

## Start mit Docker (empfohlen)

```bash
git clone https://github.com/Larosen/deal-alarm.git
cd deal-alarm
docker compose up -d --build
```

Danach <http://localhost:8080> öffnen, unter **Einstellungen** das ntfy-Topic eintragen,
„Testnachricht senden“ klicken und Geräte hinzufügen. Die Daten liegen in `./data/state.json`.

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
- **Euronics-Marktpreise:** Derzeit wird bei Euronics nur der Onlinepreis abgefragt. Der Filial-Scanner
  inkl. Rate-Limit ist vorbereitet; es fehlt noch, wie euronics.de den Preis eines bestimmten Markts
  ausliefert – dafür wird ein Mitschnitt der Browser-Anfrage beim Wechsel des Markts benötigt.
- Eine Prüfung über alle expert-Filialen dauert ca. eine Minute pro Gerät. Bitte das Intervall nicht zu
  klein wählen (Minimum 15 Minuten), damit die Shops nicht unnötig belastet werden.

## Entwicklung

```bash
.venv/bin/pip install -r requirements-dev.txt
.venv/bin/python -m pytest
```

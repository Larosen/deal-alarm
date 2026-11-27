# Echte Shop-Seiten als Testdaten

Hier liegen echte Antworten der Shops (gespeichert mit `DEBUG_CAPTURE=1`, siehe README).
`cargo test` prüft jede Datei automatisch gegen die Auswertung – ändert ein Shop seinen
Seitenaufbau, legt man die neue Seite dazu und sieht sofort, was nicht mehr passt.

Aufbau: `fixtures/<art>/<name>.html` plus `fixtures/<art>/<name>.json` mit dem erwarteten Ergebnis.

| Ordner              | Auswertung                    | Erwartung (`.json`)                                   |
|---------------------|-------------------------------|-------------------------------------------------------|
| `product/`          | Produktseite Euronics/idealo  | `{"title": "...", "price": 219.0, "ean": "..."}`      |
| `amazon-offers/`    | Amazon „Alle Angebote“        | `{"offers": 8, "cheapest": 249.0}`                    |
| `amazon-search/`    | Amazon-Suche                  | `{"first_asin": "B0...", "results": 16}`              |
| `idealo-search/`    | idealo-Suche                  | `{"first_id": "205412345", "results": 20}`            |

Felder, die in der `.json` fehlen, werden nicht geprüft. Persönliche Daten (Cookies,
Namen) vor dem Einchecken entfernen.

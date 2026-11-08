"""Versand von Benachrichtigungen über ntfy (https://ntfy.sh)."""

import logging

import requests

log = logging.getLogger(__name__)


def send(settings, title, message, click=None, tags=None, priority=None):
    """Sendet eine Nachricht und liefert die Nachrichten-ID, die der ntfy-Server vergeben hat."""
    server = (settings.get("ntfy_server") or "https://ntfy.sh").rstrip("/")
    topic = (settings.get("ntfy_topic") or "").strip()
    if not topic:
        raise ValueError("Kein ntfy-Topic konfiguriert")
    payload = {
        "topic": topic,
        "title": title,
        "message": message,
        "priority": int(priority or settings.get("ntfy_priority") or 4),
        "tags": tags or [],
    }
    if click:
        payload["click"] = click
        payload["actions"] = [{"action": "view", "label": "Zum Angebot", "url": click}]
    headers = {}
    if settings.get("ntfy_token"):
        headers["Authorization"] = f"Bearer {settings['ntfy_token']}"
    # JSON-Publishing an die Server-Root erlaubt Umlaute in Titel und Text
    response = requests.post(server, json=payload, headers=headers, timeout=20)
    if response.status_code in (401, 403):
        raise PermissionError(f"ntfy lehnt ab (HTTP {response.status_code}) – Access-Token oder Schreibrecht auf "
                              f"Topic „{topic}“ prüfen")
    response.raise_for_status()
    # Nur eine echte ntfy-Antwort enthält eine Nachrichten-ID. Ein 200 von etwas anderem
    # (z. B. Reverse-Proxy, falsche Server-URL) würde sonst als Erfolg durchgehen.
    try:
        data = response.json()
    except ValueError:
        data = None
    if not isinstance(data, dict) or not data.get("id"):
        raise ValueError(f"Antwort von {server} ist keine ntfy-Antwort (HTTP {response.status_code}, "
                         f"{response.headers.get('Content-Type', '?')}) – Server-URL prüfen")
    if data.get("topic") and data["topic"] != topic:
        raise ValueError(f"ntfy hat die Nachricht unter Topic „{data['topic']}“ statt „{topic}“ angenommen")
    log.info("ntfy: Nachricht %s an %s/%s angenommen", data["id"], server, topic)
    return data["id"]

"""Versand von Benachrichtigungen über ntfy (https://ntfy.sh)."""

import requests


def send(settings, title, message, click=None, tags=None, priority=None):
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
    response.raise_for_status()

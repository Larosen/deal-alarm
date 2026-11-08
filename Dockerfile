FROM python:3.12-slim

LABEL org.opencontainers.image.title="Deal-Alarm" \
      org.opencontainers.image.description="Dealalarm für expert & Euronics: Filialpreise prüfen, Benachrichtigung per ntfy" \
      org.opencontainers.image.source="https://github.com/Larosen/deal-alarm"

ENV PYTHONUNBUFFERED=1 \
    DATA_DIR=/data \
    PORT=8080 \
    TZ=Europe/Berlin

# Zeitzonendaten, sonst greift TZ nicht und alle Zeiten laufen in UTC
RUN apt-get update \
    && apt-get install -y --no-install-recommends tzdata \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt
COPY dealalarm ./dealalarm

VOLUME /data
EXPOSE 8080
HEALTHCHECK --interval=60s --timeout=5s --start-period=20s \
    CMD python -c "import os, urllib.request; urllib.request.urlopen(f'http://127.0.0.1:{os.environ.get(\"PORT\", \"8080\")}/healthz', timeout=4)"
CMD ["python", "-m", "dealalarm.app"]

FROM python:3.15-slim

ENV PYTHONUNBUFFERED=1 \
    DATA_DIR=/data \
    PORT=8080 \
    TZ=Europe/Berlin

WORKDIR /app
COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt
COPY dealalarm ./dealalarm

VOLUME /data
EXPOSE 8080
CMD ["python", "-m", "dealalarm.app"]

"""Adaptives Rate-Limit pro Shop.

Jeder Shop bekommt einen Limiter, der Anfragen gleichmäßig verteilt. Meldet der Shop
eine Drosselung (429, 503 oder ein WAF-403), wird das Tempo halbiert und eine Pause
eingelegt (Retry-After wird beachtet). Nach einer Serie erfolgreicher Anfragen steigt das
Tempo langsam wieder bis zum Maximum. Häufen sich Drosselungen, bricht der Scan ab und
setzt im nächsten Zyklus an derselben Stelle fort.
"""

import threading
import time

THROTTLE_STATUS = {429, 503}


class RateLimited(Exception):
    """Der Shop drosselt zu stark – Scan abbrechen und später fortsetzen."""


class AdaptiveLimiter:
    def __init__(self, name, per_minute, min_per_minute=2, max_strikes=5, max_pause=300):
        self.name = name
        self.max_strikes = max_strikes
        self.max_pause = max_pause
        self.min_rate = min_per_minute
        self._lock = threading.Lock()
        self._next_slot = 0.0
        self._pause_until = 0.0
        self._streak = 0
        self._strikes = 0
        self.requests = 0
        self.throttled_count = 0
        self.configure(per_minute)

    def configure(self, per_minute):
        per_minute = max(self.min_rate, float(per_minute))
        with self._lock:
            if getattr(self, "max_rate", None) != per_minute:
                self.max_rate = per_minute
                self.rate = per_minute

    def start_scan(self):
        with self._lock:
            self._strikes = 0

    def wait(self):
        """Blockiert bis zum nächsten freien Slot."""
        with self._lock:
            if self._strikes >= self.max_strikes:
                raise RateLimited(f"{self.name} drosselt die Anfragen")
            now = time.monotonic()
            start = max(now, self._next_slot, self._pause_until)
            self._next_slot = start + 60.0 / self.rate
        if start > now:
            time.sleep(start - now)

    def ok(self):
        with self._lock:
            self.requests += 1
            self._strikes = 0
            self._streak += 1
            if self._streak >= 25 and self.rate < self.max_rate:
                self.rate = min(self.max_rate, self.rate * 1.15)
                self._streak = 0

    def throttled(self, retry_after=None):
        with self._lock:
            self.requests += 1
            self.throttled_count += 1
            self._strikes += 1
            self._streak = 0
            self.rate = max(self.min_rate, self.rate / 2)
            pause = retry_after if retry_after else 15 * 2 ** (self._strikes - 1)
            self._pause_until = time.monotonic() + min(self.max_pause, pause)

    def snapshot(self):
        with self._lock:
            return {
                "rate_per_min": round(self.rate, 1),
                "max_rate_per_min": self.max_rate,
                "requests": self.requests,
                "throttled": self.throttled_count,
                "paused": self._pause_until > time.monotonic(),
            }


def _retry_after(response):
    value = response.headers.get("Retry-After")
    try:
        return float(value) if value else None
    except ValueError:
        return None


def request(session, limiter, method, url, throttle_status=THROTTLE_STATUS, attempts=4, **kwargs):
    """HTTP-Anfrage unter Kontrolle des Limiters. Liefert die Antwort oder wirft RateLimited."""
    kwargs.setdefault("timeout", 20)
    for _ in range(attempts):
        limiter.wait()
        try:
            response = session.request(method, url, **kwargs)
        except Exception:
            limiter.throttled()
            continue
        if response.status_code in throttle_status:
            limiter.throttled(_retry_after(response))
            continue
        limiter.ok()
        return response
    raise RateLimited(f"{limiter.name}: keine Antwort nach {attempts} Versuchen")


_limiters = {}
_registry_lock = threading.Lock()


def get_limiter(name, per_minute, **kwargs):
    with _registry_lock:
        limiter = _limiters.get(name)
        if limiter is None:
            limiter = _limiters[name] = AdaptiveLimiter(name, per_minute, **kwargs)
        else:
            limiter.configure(per_minute)
        return limiter


def snapshots():
    with _registry_lock:
        return {name: limiter.snapshot() for name, limiter in _limiters.items()}

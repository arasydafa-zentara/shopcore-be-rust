import hashlib
import hmac
import json
import os
import random
import threading
import time
import uuid

import requests
from flask import Flask, jsonify, request

app = Flask(__name__)

CHARGES = []
SEEN_KEYS = {}
SECRET = os.environ.get("BANK_SECRET", "bank-shared-secret-2019")
APP_URL = os.environ.get("APP_URL", "http://localhost:8000")
SETTINGS = {
    "latency_ms": int(os.environ.get("BANK_LATENCY_MS", "0")),
    "drop_rate": float(os.environ.get("BANK_DROP_RATE", "0")),
}


def settle(order_id, bank_ref):
    time.sleep(0.2)
    body = json.dumps({"order_id": order_id, "bank_ref": bank_ref, "status": "settled"}).encode()
    sig = hmac.new(SECRET.encode(), body, hashlib.sha256).hexdigest()
    try:
        requests.post(
            APP_URL + "/api/webhooks/bank",
            data=body,
            headers={"X-Signature": sig, "Content-Type": "application/json"},
            timeout=5,
        )
    except Exception:
        pass


@app.route("/charge", methods=["POST"])
def charge():
    data = request.get_json(silent=True) or {}
    if SETTINGS["latency_ms"]:
        time.sleep(SETTINGS["latency_ms"] / 1000.0)
    key = request.headers.get("Idempotency-Key") or data.get("idempotency_key")
    if key and key in SEEN_KEYS:
        return jsonify(bank_ref=SEEN_KEYS[key], status="ok")
    ref = "BK-" + uuid.uuid4().hex[:12]
    if key:
        SEEN_KEYS[key] = ref
    CHARGES.append({"ref": ref, "order_id": data.get("order_id"), "amount": data.get("amount")})
    if random.random() < SETTINGS["drop_rate"]:
        return jsonify(error="gateway timeout"), 503
    threading.Thread(target=settle, args=(data.get("order_id"), ref), daemon=True).start()
    return jsonify(bank_ref=ref, status="ok")


@app.route("/stats", methods=["GET"])
def stats():
    return jsonify(charges=len(CHARGES), items=CHARGES[-100000:])


@app.route("/config", methods=["GET", "POST"])
def config():
    if request.method == "POST":
        data = request.get_json(silent=True) or {}
        if "drop_rate" in data:
            SETTINGS["drop_rate"] = float(data["drop_rate"])
        if "latency_ms" in data:
            SETTINGS["latency_ms"] = int(data["latency_ms"])
    return jsonify(SETTINGS)


@app.route("/reset", methods=["POST"])
def reset():
    CHARGES.clear()
    SEEN_KEYS.clear()
    return jsonify(ok=True)


if __name__ == "__main__":
    app.run(host="0.0.0.0", port=5001, threaded=True)

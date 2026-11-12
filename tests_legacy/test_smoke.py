import hashlib
import hmac
import json
import os
import time
import uuid

import requests

BASE = os.environ.get("BASE_URL", "http://localhost:8000")
WIDE = {"from": "2000-01-01", "to": "2999-12-31"}


def api(method, path, token=None, **kw):
    headers = kw.pop("headers", {})
    if token:
        headers["Authorization"] = "Bearer " + token
    return requests.request(method, BASE + path, headers=headers, **kw)


def new_user(prefix="u"):
    name = "%s_%s" % (prefix, uuid.uuid4().hex[:8])
    r = api("POST", "/api/register", json={"username": name, "email": name + "@example.com", "password": "pw12345", "name": "Test " + name})
    assert r.status_code == 201, r.text
    uid = r.json()["id"]
    r = api("POST", "/api/login", json={"username": name, "password": "pw12345"})
    assert r.status_code == 200, r.text
    return {"id": uid, "username": name, "token": r.json()["token"]}


def admin_token():
    r = api("POST", "/api/login", json={"username": "admin", "password": "admin123"})
    assert r.status_code == 200, r.text
    return r.json()["token"]


def first_product():
    r = api("GET", "/api/products?sort=id&limit=10")
    assert r.status_code == 200
    return r.json()[0]


def test_health():
    r = api("GET", "/health")
    assert r.status_code == 200 and r.json()["status"] == "ok"


def test_register_login_me_update():
    u = new_user()
    r = api("GET", "/api/users/me", u["token"])
    assert r.status_code == 200
    assert r.json()["username"] == u["username"]
    assert r.json()["role"] == "user"
    r = api("PATCH", "/api/users/me", u["token"], json={"name": "Renamed"})
    assert r.status_code == 200 and r.json()["name"] == "Renamed"
    assert api("GET", "/api/users/me").status_code == 401
    assert api("POST", "/api/login", json={"username": u["username"], "password": "wrong"}).status_code == 401
    assert api("POST", "/api/logout", u["token"]).status_code == 200


def test_register_rejects_duplicates_and_missing():
    u = new_user()
    r = api("POST", "/api/register", json={"username": u["username"], "email": "x_" + u["username"] + "@example.com", "password": "pw"})
    assert r.status_code == 409
    assert api("POST", "/api/register", json={"username": "nopass"}).status_code == 400


def test_products_list_sort_search_get():
    r = api("GET", "/api/products?sort=price&limit=5")
    assert r.status_code == 200
    prices = [p["price"] for p in r.json()]
    assert len(prices) == 5 and prices == sorted(prices)
    r = api("GET", "/api/products?q=keyboard")
    assert r.status_code == 200 and any("Keyboard" in p["name"] for p in r.json())
    p = first_product()
    assert api("GET", "/api/products/%d" % p["id"]).json()["sku"] == p["sku"]
    assert api("GET", "/api/products/999999").status_code == 404


def test_admin_product_create_update_and_forbidden_for_user():
    adm = admin_token()
    sku = "T-" + uuid.uuid4().hex[:6]
    r = api("POST", "/api/products", adm, json={"sku": sku, "name": "Test Widget", "price": 12.5, "stock": 50})
    assert r.status_code == 201
    pid = r.json()["id"]
    r = api("PUT", "/api/products/%d" % pid, adm, json={"price": 15.0})
    assert r.status_code == 200 and r.json()["price"] == 15.0
    u = new_user()
    assert api("POST", "/api/products", u["token"], json={"sku": "x", "name": "x", "price": 1, "stock": 1}).status_code == 403


def test_order_lifecycle_wallet_pay_and_invoice():
    u = new_user()
    p = first_product()
    r = api("POST", "/api/orders", u["token"], json={"items": [{"product_id": p["id"], "quantity": 2}]})
    assert r.status_code == 201, r.text
    order = r.json()
    assert order["status"] == "pending"
    assert order["total"] == round(p["price"] * 2, 2)
    assert len(order["items"]) == 1 and order["items"][0]["quantity"] == 2

    r = api("GET", "/api/orders/%d" % order["id"], u["token"])
    assert r.status_code == 200 and r.json()["id"] == order["id"]
    r = api("GET", "/api/orders", u["token"])
    assert r.status_code == 200 and any(o["id"] == order["id"] for o in r.json())
    r = api("GET", "/api/orders/%d/invoice" % order["id"], u["token"])
    assert r.status_code == 200 and r.json()["total"] == order["total"]

    r = api("POST", "/api/orders/%d/pay" % order["id"], u["token"], json={"method": "wallet"})
    assert r.status_code == 402
    api("POST", "/api/wallet/topup", u["token"], json={"amount": 1000})
    r = api("POST", "/api/orders/%d/pay" % order["id"], u["token"], json={"method": "wallet"})
    assert r.status_code == 200 and r.json()["status"] == "paid"
    bal = api("GET", "/api/wallet", u["token"]).json()["balance"]
    assert abs(bal - (1000 - order["total"])) < 0.01
    assert api("POST", "/api/orders/%d/pay" % order["id"], u["token"], json={"method": "wallet"}).status_code == 409


def test_order_bank_payment_and_settlement_webhook():
    u = new_user()
    p = first_product()
    order = api("POST", "/api/orders", u["token"], json={"items": [{"product_id": p["id"], "quantity": 1}]}).json()
    r = api("POST", "/api/orders/%d/pay" % order["id"], u["token"], json={"method": "bank"})
    assert r.status_code == 200, r.text
    assert r.json()["bank_ref"].startswith("BK-")
    time.sleep(0.6)
    assert api("GET", "/api/orders/%d" % order["id"], u["token"]).json()["status"] == "paid"


def test_cancel_restocks_and_refunds_wallet():
    u = new_user()
    p = first_product()
    before = api("GET", "/api/products/%d" % p["id"]).json()["stock"]
    order = api("POST", "/api/orders", u["token"], json={"items": [{"product_id": p["id"], "quantity": 3}]}).json()
    assert api("GET", "/api/products/%d" % p["id"]).json()["stock"] == before - 3
    api("POST", "/api/wallet/topup", u["token"], json={"amount": 500})
    api("POST", "/api/orders/%d/pay" % order["id"], u["token"], json={"method": "wallet"})
    r = api("POST", "/api/orders/%d/cancel" % order["id"], u["token"])
    assert r.status_code == 200 and r.json()["status"] == "cancelled"
    assert api("GET", "/api/products/%d" % p["id"]).json()["stock"] == before
    assert abs(api("GET", "/api/wallet", u["token"]).json()["balance"] - 500) < 0.01
    assert api("POST", "/api/orders/%d/cancel" % order["id"], u["token"]).status_code == 409


def test_refund_paid_order():
    u = new_user()
    p = first_product()
    order = api("POST", "/api/orders", u["token"], json={"items": [{"product_id": p["id"], "quantity": 1}]}).json()
    assert api("POST", "/api/orders/%d/refund" % order["id"], u["token"]).status_code == 409
    api("POST", "/api/wallet/topup", u["token"], json={"amount": 500})
    api("POST", "/api/orders/%d/pay" % order["id"], u["token"], json={"method": "wallet"})
    r = api("POST", "/api/orders/%d/refund" % order["id"], u["token"], json={})
    assert r.status_code == 200 and r.json()["status"] == "refunded"
    assert abs(api("GET", "/api/wallet", u["token"]).json()["balance"] - 500) < 0.01


def test_coupon_discount_and_single_use():
    u = new_user()
    p = first_product()
    r = api("POST", "/api/orders", u["token"], json={"items": [{"product_id": p["id"], "quantity": 2}], "coupon": "SAVE10"})
    assert r.status_code == 201, r.text
    assert r.json()["total"] == round(p["price"] * 2 * 0.9, 2)
    assert api("POST", "/api/orders", u["token"], json={"items": [{"product_id": p["id"], "quantity": 1}], "coupon": "NOPE"}).status_code == 400


def test_order_validation_errors():
    u = new_user()
    assert api("POST", "/api/orders", u["token"], json={"items": []}).status_code == 400
    assert api("GET", "/api/orders/999999", u["token"]).status_code == 404
    assert api("POST", "/api/orders", None, json={"items": [{"product_id": 1, "quantity": 1}]}).status_code == 401


def test_wallet_topup_and_transfer():
    a, b = new_user("a"), new_user("b")
    assert api("POST", "/api/wallet/topup", a["token"], json={"amount": 100}).json()["balance"] == 100
    r = api("POST", "/api/wallet/transfer", a["token"], json={"to_user_id": b["id"], "amount": 40})
    assert r.status_code == 200 and r.json()["balance"] == 60
    assert api("GET", "/api/wallet", b["token"]).json()["balance"] == 40
    assert api("POST", "/api/wallet/transfer", a["token"], json={"to_user_id": b["id"], "amount": 1000}).status_code == 402
    assert api("POST", "/api/wallet/transfer", a["token"], json={"to_user_id": 99999999, "amount": 1}).status_code == 404


def test_admin_users_roles_and_order_status():
    adm = admin_token()
    u = new_user()
    r = api("GET", "/api/admin/users", adm)
    assert r.status_code == 200 and any(x["id"] == u["id"] for x in r.json())
    assert api("GET", "/api/admin/users", u["token"]).status_code == 403
    r = api("POST", "/api/admin/users/%d/role" % u["id"], adm, json={"role": "admin"})
    assert r.status_code == 200 and r.json()["role"] == "admin"
    p = first_product()
    order = api("POST", "/api/orders", u["token"], json={"items": [{"product_id": p["id"], "quantity": 1}]}).json()
    r = api("POST", "/api/admin/orders/%d/status" % order["id"], adm, json={"status": "shipped"})
    assert r.status_code == 200 and r.json()["status"] == "shipped"


def test_sales_report_and_export_download():
    adm = admin_token()
    u = new_user()
    p = first_product()
    order = api("POST", "/api/orders", u["token"], json={"items": [{"product_id": p["id"], "quantity": 1}]}).json()
    api("POST", "/api/wallet/topup", u["token"], json={"amount": 500})
    api("POST", "/api/orders/%d/pay" % order["id"], u["token"], json={"method": "wallet"})

    r = api("GET", "/api/reports/sales", adm, params=dict(WIDE, group_by="status", nonce=uuid.uuid4().hex))
    assert r.status_code == 200
    body = r.json()
    assert any(b["bucket"] == "paid" for b in body["buckets"])
    assert len(body["top_products"]) >= 1
    assert api("GET", "/api/reports/sales", u["token"], params=WIDE).status_code == 403

    fname = "smoke_%s.csv" % uuid.uuid4().hex[:6]
    r = api("POST", "/api/reports/export", adm, json={"filename": fname})
    assert r.status_code == 200 and r.json()["file"] == fname + ".gz"
    r = api("GET", "/api/reports/download", adm, params={"file": fname + ".gz"})
    assert r.status_code == 200 and r.content[:2] == b"\x1f\x8b"
    assert api("GET", "/api/reports/download", adm, params={"file": "missing.gz"}).status_code == 404


def test_bank_webhook_with_valid_signature():
    u = new_user()
    p = first_product()
    order = api("POST", "/api/orders", u["token"], json={"items": [{"product_id": p["id"], "quantity": 1}]}).json()
    body = json.dumps({"order_id": order["id"], "bank_ref": "BK-test", "status": "settled"}).encode()
    sig = hmac.new(b"bank-shared-secret-2019", body, hashlib.sha256).hexdigest()
    r = api("POST", "/api/webhooks/bank", data=body, headers={"X-Signature": sig, "Content-Type": "application/json"})
    assert r.status_code == 200
    assert api("GET", "/api/orders/%d" % order["id"], u["token"]).json()["status"] == "paid"
    r = api("POST", "/api/webhooks/bank", data=body, headers={"X-Signature": "deadbeef", "Content-Type": "application/json"})
    assert r.status_code == 401

#!/usr/bin/env python3
"""Differential check: replay the same scenario against the legacy Flask service and the Rust service and
print every difference in status code or (normalised) JSON body.

    python tools/differential.py http://localhost:18000 http://localhost:8000

Ids, tokens, timestamps and bank references legitimately differ and are normalised away. Differences that
remain are either deliberate (see "Behavior that deliberately differs" in the README) or bugs.
"""
import hashlib, hmac, json, sys, uuid

import requests

VOLATILE = {"token", "created_at", "id", "order_no", "user_id", "order_id", "bank_ref", "username", "email", "name", "trace"}
SECRET = b"bank-shared-secret-2019"


def norm(v, key=None):
    if isinstance(v, dict):
        return {k: norm(x, k) for k, x in sorted(v.items())}
    if isinstance(v, list):
        return [norm(x) for x in v]
    if key in VOLATILE:
        return "<%s>" % type(v).__name__
    if isinstance(v, float) and v == int(v):
        return int(v)
    return v


class Client:
    def __init__(self, base):
        self.base = base
        self.log = []

    def call(self, label, method, path, tok=None, **kw):
        h = kw.pop("headers", {})
        if tok:
            h["Authorization"] = "Bearer " + tok
        try:
            r = requests.request(method, self.base + path, headers=h, timeout=30, **kw)
            try:
                body = norm(r.json())
            except ValueError:
                body = "<non-json %d bytes ct=%s>" % (len(r.content), r.headers.get("content-type", "").split(";")[0])
            self.log.append((label, r.status_code, body))
            return r
        except Exception as e:  # noqa
            self.log.append((label, "EXC", str(e)[:80]))
            class R:  # minimal stand-in
                status_code = 0
                def json(self): return {}
                content = b""
            return R()


def scenario(c):
    tag = uuid.uuid4().hex[:8]
    reg = lambda n: c.call("register " + n, "POST", "/api/register", json={"username": n + tag, "email": n + tag + "@x.com", "password": "pw1"})
    def login(n, pw="pw1"):
        return c.call("login " + n, "POST", "/api/login", json={"username": n + tag, "password": pw})
    reg("alice"); reg("bob")
    tok = lambda n: login(n).json().get("token")
    ta, tb = tok("alice"), tok("bob")
    adm = c.call("admin login", "POST", "/api/login", json={"username": "admin", "password": "admin123"}).json().get("token")
    bid = c.call("alice id", "GET", "/api/users/me", ta).json().get("id")
    bob_id = c.call("bob id", "GET", "/api/users/me", tb).json().get("id")

    reg("alice"); c.call("register missing fields", "POST", "/api/register", json={"username": "x"})
    c.call("register blank password", "POST", "/api/register", json={"username": "z" + tag, "email": "z@z", "password": ""})
    login("alice", "bad"); c.call("login no body", "POST", "/api/login")
    c.call("me no token", "GET", "/api/users/me"); c.call("me bad token", "GET", "/api/users/me", "nope")
    c.call("patch name", "PATCH", "/api/users/me", ta, json={"name": "Alice A"})
    c.call("logout", "POST", "/api/logout", tok("bob")); 

    for q in ["", "?sort=price&limit=3", "?sort=name&limit=2&page=2", "?q=key", "?limit=0", "?limit=-1", "?page=0", "?sort=bogus", "?limit=abc"]:
        c.call("products " + q, "GET", "/api/products" + q)
    c.call("product 1", "GET", "/api/products/1"); c.call("product 99999", "GET", "/api/products/99999")
    c.call("product create (user)", "POST", "/api/products", ta, json={"sku": "x", "name": "x", "price": 1, "stock": 1})
    p = c.call("product create (admin)", "POST", "/api/products", adm, json={"sku": "T" + tag, "name": "Widget " + tag, "price": 12.5, "stock": 5}).json()
    pid = p.get("id")
    c.call("product update", "PUT", "/api/products/%s" % pid, adm, json={"price": "15.25"})
    c.call("product update 404", "PUT", "/api/products/999999", adm, json={"price": 1})

    items = lambda q, i=None: {"items": [{"product_id": i or pid, "quantity": q}]}
    o1 = c.call("order ok", "POST", "/api/orders", ta, json=items(2)).json().get("id")
    c.call("order coupon", "POST", "/api/orders", ta, json=dict(items(1), coupon="SAVE10"))
    c.call("order bad coupon", "POST", "/api/orders", ta, json=dict(items(1), coupon="NOPE"))
    c.call("order empty", "POST", "/api/orders", ta, json={"items": []})
    c.call("order no auth", "POST", "/api/orders", json=items(1))
    c.call("order unknown product", "POST", "/api/orders", ta, json=items(1, 999999))
    c.call("order too much stock", "POST", "/api/orders", ta, json=items(1000))
    c.call("order string qty", "POST", "/api/orders", ta, json=items("2"))
    c.call("order zero qty", "POST", "/api/orders", ta, json=items(0))
    c.call("orders list", "GET", "/api/orders", ta); c.call("orders limit=1", "GET", "/api/orders?limit=1&page=2", ta)
    c.call("order get", "GET", "/api/orders/%s" % o1, ta); c.call("order get 404", "GET", "/api/orders/999999", ta)
    c.call("invoice", "GET", "/api/orders/%s/invoice" % o1, ta)
    c.call("pay insufficient", "POST", "/api/orders/%s/pay" % o1, ta, json={"method": "wallet"})
    c.call("pay unknown method", "POST", "/api/orders/%s/pay" % o1, ta, json={"method": "cash"})
    c.call("topup", "POST", "/api/wallet/topup", ta, json={"amount": 500})
    c.call("topup string", "POST", "/api/wallet/topup", ta, json={"amount": "10.5"})
    c.call("wallet", "GET", "/api/wallet", ta)
    c.call("pay wallet", "POST", "/api/orders/%s/pay" % o1, ta, json={"method": "wallet"})
    c.call("pay again", "POST", "/api/orders/%s/pay" % o1, ta, json={"method": "wallet"})
    c.call("pay default method (no body)", "POST", "/api/orders/%s/pay" % o1, ta)
    c.call("pay other user's order", "POST", "/api/orders/%s/pay" % o1, tb, json={})
    o2 = c.call("order 2", "POST", "/api/orders", ta, json=items(1)).json().get("id")
    r = c.call("pay bank", "POST", "/api/orders/%s/pay" % o2, ta, json={"method": "bank"})
    import time; time.sleep(0.7)
    c.call("order after bank", "GET", "/api/orders/%s" % o2, ta)
    c.call("transfer", "POST", "/api/wallet/transfer", ta, json={"to_user_id": bob_id, "amount": 40})
    c.call("transfer too much", "POST", "/api/wallet/transfer", ta, json={"to_user_id": bob_id, "amount": 1e6})
    c.call("transfer 404", "POST", "/api/wallet/transfer", ta, json={"to_user_id": 99999999, "amount": 1})
    c.call("refund pending", "POST", "/api/orders/%s/refund" % c.call("o3", "POST", "/api/orders", ta, json=items(1)).json().get("id"), ta, json={})
    c.call("refund partial", "POST", "/api/orders/%s/refund" % o1, ta, json={"amount": 10})
    c.call("refund rest", "POST", "/api/orders/%s/refund" % o1, ta, json={})
    c.call("refund 404", "POST", "/api/orders/999999/refund", ta, json={})
    c.call("cancel paid", "POST", "/api/orders/%s/cancel" % o2, ta)
    c.call("cancel again", "POST", "/api/orders/%s/cancel" % o2, ta)
    c.call("cancel 404", "POST", "/api/orders/999999/cancel", ta)
    c.call("wallet after", "GET", "/api/wallet", ta)

    c.call("admin users (user)", "GET", "/api/admin/users", ta)
    c.call("admin users", "GET", "/api/admin/users", adm)
    c.call("role bad", "POST", "/api/admin/users/%s/role" % bob_id, adm, json={"role": "superuser"})
    c.call("role admin", "POST", "/api/admin/users/%s/role" % bob_id, adm, json={"role": "admin"})
    c.call("role 404", "POST", "/api/admin/users/999999/role", adm, json={"role": "user"})
    c.call("order status", "POST", "/api/admin/orders/%s/status" % o1, adm, json={"status": "shipped"})
    c.call("order status bad", "POST", "/api/admin/orders/%s/status" % o1, adm, json={"status": "weird"})
    c.call("order status 404", "POST", "/api/admin/orders/999999/status", adm, json={"status": "paid"})

    W = {"from": "2000-01-01", "to": "2999-12-31"}
    for g in ["day", "status", "user"]:
        d = c.call("report " + g, "GET", "/api/reports/sales", adm, params=dict(W, group_by=g, nonce=uuid.uuid4().hex)).json()
        if isinstance(d, dict):
            d["buckets"] = len(d.get("buckets", [])) > 0
    c.call("report bogus group", "GET", "/api/reports/sales", adm, params=dict(W, group_by="bogus"))
    c.call("report bad date", "GET", "/api/reports/sales", adm, params={"from": "garbage"})
    c.call("report user", "GET", "/api/reports/sales", ta, params=W)
    f = "d_%s.csv" % tag
    c.call("export", "POST", "/api/reports/export", adm, json={"filename": f})
    r = c.call("download", "GET", "/api/reports/download", adm, params={"file": f + ".gz"})
    c.log[-1] = (c.log[-1][0], r.status_code, "gz=%s" % (r.content[:2] == b"\x1f\x8b"))
    c.call("download missing", "GET", "/api/reports/download", adm, params={"file": "missing.gz"})

    body = json.dumps({"order_id": o1, "bank_ref": "BK-x", "status": "settled"}).encode()
    sig = hmac.new(SECRET, body, hashlib.sha256).hexdigest()
    hd = {"Content-Type": "application/json"}
    c.call("webhook ok", "POST", "/api/webhooks/bank", data=body, headers=dict(hd, **{"X-Signature": sig}))
    c.call("webhook bad sig", "POST", "/api/webhooks/bank", data=body, headers=dict(hd, **{"X-Signature": "00"}))
    c.call("webhook unknown order", "POST", "/api/webhooks/bank", data=json.dumps({"order_id": 99999999, "status": "settled"}).encode(),
           headers=dict(hd, **{"X-Signature": hmac.new(SECRET, json.dumps({"order_id": 99999999, "status": "settled"}).encode(), hashlib.sha256).hexdigest()}))
    c.call("health", "GET", "/health"); c.call("unknown route", "GET", "/api/nope"); c.call("wrong method", "DELETE", "/api/products/1")


def main():
    a, b = Client(sys.argv[1]), Client(sys.argv[2])
    scenario(a); scenario(b)
    same = 0
    for (la, sa, ba), (lb, sb, bb) in zip(a.log, b.log):
        if sa == sb and ba == bb:
            same += 1
            continue
        print("DIFF  %s\n   legacy: %s %s\n   rust:   %s %s" % (la, sa, json.dumps(ba)[:230], sb, json.dumps(bb)[:230]))
    print("\n%d identical, %d different, of %d steps" % (same, len(a.log) - same, len(a.log)))


if __name__ == "__main__":
    main()

# ShopCore (Rust)

Order, wallet and reporting backend, migrated from the legacy Flask service to Rust
(axum + sqlx + tokio). Same HTTP API (`docs/openapi.yaml`), same maintenance commands, same bank
settlement webhook, same database tables.

## Build and run

```
docker compose up --build        # db (postgres:16) + bank simulator + app on :8000
```

Starting the app creates the tables, applies the additive migrations and seeds the admin account,
two coupons and ten products (all idempotent). No manual steps.

## Test

```
cargo test                       # unit tests (offline) + end-to-end tests
docker compose up -d --build     # ...start the stack first, otherwise the end-to-end tests are skipped
BASE_URL=http://localhost:8000 pytest -q tests_legacy   # the original legacy smoke tests, unchanged
```

## Configuration (environment)

| Variable | Default | Meaning |
|---|---|---|
| `DATABASE_URL` | `postgresql://shop:shop@localhost:5432/shop` | Postgres |
| `BANK_URL` | `http://localhost:5001` | bank simulator |
| `BANK_SECRET` | `bank-shared-secret-2019` | HMAC key of the settlement webhook |
| `ADMIN_USER` / `ADMIN_PASSWORD` | `admin` / `admin123` | seeded admin (change outside of testing) |
| `EXPORT_DIR` | `/tmp/exports` | where report exports are written |
| `PORT` | `8000` | listen port |
| `DB_MAX_CONNECTIONS` | `20` | pool size |
| `NOTIFY_BLOCK_PRIVATE` | `false` | `true` refuses `notify_url` targets on private/loopback addresses (set it in production) |

A warning is logged at start if the default admin password, bank secret or open notify policy are in use.

## Maintenance commands

Same commands as `manage.py`, available three ways inside the container:

```
docker compose exec app manage init-db | seed | purge-sessions
docker compose exec app manage import-products <csv>
docker compose exec app manage export-sales <file>
docker compose exec app python manage.py seed     # launcher, same thing
docker compose exec app shopcore manage seed
```

(`manage.py` is a small launcher that execs the Rust binary; the service itself contains no Python.)

## Database

The six legacy tables are unchanged. Additive changes, applied automatically at start (`migrations/`):
`sessions` table, `order_no_seq` sequence, and indexes / unique constraints on `users.username`,
`users.email`, `orders.order_no` plus lookup indexes. The index step is best effort: on a database that
already contains duplicates the unique index is skipped with a warning instead of stopping the service.
The legacy service can still run against the migrated database.

## Behavior that deliberately differs from the legacy service

Security (all were reachable by any caller):
- Admin routes without a token returned data; now `401` (valid non-admin: `403`).
- The bank webhook accepted unsigned calls; the signature is now required and compared in constant time.
- SQL injection via `sort`, `group_by`, `PATCH /users/me` keys and `import-products`; identifiers are allowlisted,
  values are bound parameters. `PATCH /users/me` only changes `name` and `email` (it let callers set `role` or the balance).
- Path traversal on report download, and shell injection through the export filename (`gzip` via shell). Filenames must be
  plain names; gzip is done in-process.
- Any logged-in user could read, cancel or refund any order and read its invoice. Now only the owner or an admin;
  everybody else gets `404`.
- `/api/admin/users` returned password hashes; removed. Errors no longer include stack traces.
- Passwords: new hashes are argon2id; legacy MD5 hashes still verify and are upgraded on the next login. Failed logins are throttled.
- Sessions are random 256-bit tokens stored hashed in the database: `logout` and `purge-sessions` really revoke them,
  and they survive a restart (30 day lifetime).
- The in-memory audit log that kept every request body (including passwords) forever is gone.

Money and consistency (all concurrency bugs in the legacy read-modify-write code):
- Order creation, stock reservation, coupon use, wallet payment/transfer/top-up, cancel and refund run in transactions with
  guarded updates: no overselling, no double spend, a failed order leaves no orphan row or leaked stock, a single-use coupon is
  consumed once, `order_no` comes from a sequence.
- `amount` values must be positive finite numbers (negative / NaN top-ups and transfers used to mint money); no self-transfer;
  order quantities must be >= 1. All answered with `400`.
- Refund: at most `total - refunded` in total, may be done in several parts (default amount = what is left), never on a pending or
  cancelled order; invalid amount `400`, nothing left `409`.
- Bank payment: one charge per order. The charge is retried on failure but every attempt carries the same
  `Idempotency-Key`, so the bank records it once (the legacy retries created duplicate charges). The call has a timeout and is
  made outside any database transaction, after the order has been claimed by a pending payment row. If the settlement webhook arrives
  first the payment is stored as settled.

Spec conformance:
- Invalid `sort`, `group_by`, admin `role` and order `status` values are `400` (legacy: SQL error `500` or silently accepted).
- Sales report is no longer cached forever (it returned stale numbers) and is computed with two SQL aggregates instead of
  N+1 loops. Order lists load items with one query. `page < 1` is treated as page 1.
- All errors, including unknown routes (404) and wrong methods (405), are `{"error": ...}`.
- The webhook's "settled" event also sends the `paid` notify_url callback when it is what moves the order to paid.
- `import-products` understands quoted CSV, skips SKUs already present, and never builds SQL from file contents.

`notify_url` callbacks: http/https only, no redirects, no embedded credentials, 5 s timeout, link-local addresses (cloud metadata)
always refused, the connection is pinned to the addresses that were validated. Private/loopback targets are refused only when
`NOTIFY_BLOCK_PRIVATE=true` (default off to stay compatible with the legacy behavior and local receivers). Events of one order are
delivered in order (pending, then paid). E-mails were never delivered in the legacy service (an unread in-memory list); they are logged only.

## Operational notes

- **Offline build.** All crates are vendored in `vendor/` (filtered to Linux x86_64 / aarch64 with `cargo vendor-filterer`) and the
  Docker build runs `cargo build --offline --locked`; the runtime image (`python:3.13-slim`) needs no package install. Verified with
  `docker build --network none --no-cache .`. To refresh: `cargo vendor-filterer --platform=x86_64-unknown-linux-gnu
  --platform=aarch64-unknown-linux-gnu --exclude-crate-path '*#tests' --exclude-crate-path '*#benches' vendor`.
- **One TLS backend.** rustls with the `ring` provider for both Postgres (`sslmode=require` works) and outgoing HTTPS.
- **Payment states.** `payments.status` is `pending` (bank charge in flight), `captured`, `settled`, or `failed`. While a payment is
  pending the order cannot be cancelled, moved by an admin or paid a second time (`409 payment in progress`), so a customer is never
  charged for an order that no longer exists. A pending row older than two minutes (a crashed request) is retired automatically.
- **Login throttle.** 10 failed attempts for a username within 60 s are answered with `429` until the window passes
  (table `login_failures`; a successful login clears it). Other usernames are unaffected.
- **CSV exports** prefix text cells that start with `= + - @` with an apostrophe, so spreadsheets do not evaluate them.
- **Timestamps** keep the legacy rendering (`Thu, 12 Nov 2026 10:00:00 GMT`; order times are Jakarta wall-clock values).
- **`manage.py`** is a tiny launcher that execs the Rust binary, so `python manage.py seed`, `./manage.py seed`, `manage seed` and
  `shopcore manage seed` all work.
- **Differential check.** `tools/differential.py <legacy-url> <rust-url>` replays one 93-step scenario against both services and lists
  every difference. Against the legacy service on fresh databases 76 steps are identical (payments, webhook, cancel, coupons, invoices,
  exports, ...); the other 17 are exactly the deliberate changes listed above or their consequences (for example the legacy service
  left three orphan `pending` orders behind after rejected requests, and refunded more than the order total).

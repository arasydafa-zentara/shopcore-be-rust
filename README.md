# ShopCore

Order, wallet and reporting backend, migrated from the legacy Flask service to Rust.
It keeps the public contract of the original: the HTTP API (`docs/openapi.yaml`), the maintenance
commands, the bank settlement webhook and the database tables.

- [Quick start](#quick-start) · [Configuration](#configuration) · [Maintenance commands](#maintenance-commands) · [Testing](#testing)
- [Design](#design) · [Deliberate differences from the legacy service](#deliberate-differences-from-the-legacy-service) · [Kept as it was](#kept-as-it-was)

## Quick start

```
docker compose up --build        # postgres:16 + bank simulator + the service on :8000
```

Starting the service creates the tables, applies the additive migrations and seeds the admin account,
two coupons and ten products. All of it is idempotent; there are no manual steps. The image builds
without network access (all dependencies are vendored in `vendor/`).

## Configuration

Environment variables; the defaults match the legacy service and the provided compose file.

| Variable | Default | Meaning |
|---|---|---|
| `DATABASE_URL` | `postgresql://shop:shop@localhost:5432/shop` | Postgres (`sslmode=require` is supported) |
| `BANK_URL` | `http://localhost:5001` | bank simulator |
| `BANK_SECRET` | `bank-shared-secret-2019` | HMAC key of the settlement webhook |
| `ADMIN_USER` / `ADMIN_PASSWORD` | `admin` / `admin123` | seeded admin account (change outside of testing) |
| `EXPORT_DIR` | `/tmp/exports` | where report exports are written |
| `PORT` | `8000` | listen port |
| `DB_MAX_CONNECTIONS` | `20` | connection pool size |
| `UPGRADE_LEGACY_HASHES` | `true` | re-hash legacy MD5 passwords with argon2id at login; `false` keeps the legacy service able to authenticate them (rollback path) |
| `NOTIFY_BLOCK_PRIVATE` | `false` | `true` refuses `notify_url` targets on private/loopback addresses; set it in production |

A warning is logged at start when the default admin password, the default bank secret or the open
`notify_url` policy is in use.

## Maintenance commands

The commands of the legacy `manage.py`, reachable in four equivalent ways inside the container:

```
docker compose exec app manage init-db | seed | purge-sessions
docker compose exec app manage import-products <csv>
docker compose exec app manage export-sales <file>
docker compose exec app python manage.py seed      # launcher that execs the binary
docker compose exec app shopcore manage seed
```

## Testing

```
docker compose up -d --build                             # the end-to-end tests need the stack
cargo test                                               # unit tests + end-to-end tests
BASE_URL=http://localhost:8000 pytest -q tests_legacy    # the original smoke tests, unchanged
python tools/differential.py <legacy-url> <this-url>     # replays 93 steps against both services
```

The end-to-end tests are skipped, with a message, when no service answers. They cover the contract
and the failure modes: concurrent orders on the last unit of stock, double spending a wallet,
cancelling while a bank payment is in flight, refund limits, injection and traversal attempts,
malformed bodies, login throttling and export hardening. CI (`.github/workflows/ci.yml`) repeats the
offline image build and the full run on an x86_64 runner.

The differential run against the legacy service on fresh databases gives 76 identical steps (payments,
webhook, cancel, coupons, invoices, exports, ...). The other 17 are exactly the deliberate changes below
or their consequences; for example the legacy service left three orphan `pending` orders behind after
rejected requests and refunded more than an order's total.

## Design

**Guiding rule.** The contract in `docs/openapi.yaml` wins over incidental legacy behavior. Where the
legacy code contradicted its own spec, or was unsafe, the service follows the spec and the change is
listed below. Where the spec is silent, the legacy behavior is kept.

**One transaction per business operation.** The legacy code committed after every statement on a single
shared connection, so every read-modify-write could interleave. Here each operation (order creation,
payment, transfer, cancel, refund, webhook) runs in one transaction and uses guarded updates
(`UPDATE ... WHERE stock >= n`, `... WHERE balance >= amount`) instead of check-then-write. Row locks
are always taken in the same order (orders, then products by id, then users by id), so concurrent
operations cannot deadlock.

**Money is derived from what was received.** Refunds and cancellations pay out `paid_amount` minus what
was already refunded, never `total` or whatever `status` says. Changing a status therefore cannot
create or lose money: a cancelled order is final, a paid order cannot return to `pending`, and an admin
setting `cancelled` runs the same bookkeeping as a customer cancel. Amounts are written rounded to two
decimals in SQL; the schema keeps its `DOUBLE PRECISION` columns and the same JSON number types.

**Bank payments.** A payment first claims the order with a `pending` payment row, then charges the bank
outside any transaction, then records the result and marks the order paid.
- The charge is retried on failure but every attempt carries the same `Idempotency-Key`, so the bank
  records one charge however many attempts were needed.
- While the row is pending the order cannot be cancelled, moved by an admin or paid again
  (`409 payment in progress`), so nobody is charged for an order that no longer exists. A pending row
  older than two minutes (a crashed request) is retired automatically.
- The flow runs in a task detached from the request: a client that disconnects mid-payment cannot leave
  a charge unrecorded.
- The settlement webhook can arrive before the payment is recorded (the bank settles about 0.2 s after
  the charge); the payment is then stored as `settled` and `paid` is announced once.
- `payments.status` is `pending`, `captured`, `settled`, `failed` (or `orphaned`, logged loudly, if it
  ever happens that a charge succeeded for an order that is not payable).

**Authentication.** Sessions are random 256-bit tokens stored hashed in a `sessions` table, so `logout`
and `manage purge-sessions` really revoke them and sessions survive restarts (30 days). New passwords use
argon2id; legacy MD5 hashes still verify and are re-hashed at login. Failed logins are throttled:
10 per username and client address, or 60 per address across usernames, within 60 s answer `429`
(table `login_failures`; a success clears it). Counting per address keeps a stranger from locking a
user out. Password hashing runs on a bounded pool, so a burst of logins queues instead of exhausting memory.

**Notifications.** `notify_url` callbacks are delivered by sharded workers, so the events of one order
stay in order (pending, then paid). Only http/https, no redirects, no embedded credentials, hard time
limits, and the connection is pinned to the addresses that were validated (no second DNS lookup, so no
rebinding). Link-local and cloud-metadata addresses, including IPv4-mapped IPv6 forms, are always
refused; private and loopback targets are refused when `NOTIFY_BLOCK_PRIVATE=true`. E-mails were never
delivered by the legacy service (an unread in-memory list), so they are only logged.

**Database.** The six legacy tables are unchanged. Additive migrations in `migrations/`, applied at
start under an advisory lock: `sessions` and `login_failures` tables, the `order_no_seq` sequence, and
indexes plus unique constraints on `users.username`, `users.email` and `orders.order_no`. The index step
is best effort: on a database that already contains duplicates the unique index is skipped with a warning
and the service still starts. Checked against a populated legacy database (MD5 users, duplicate usernames
and order numbers): it starts, authenticates legacy users and continues the order numbering. Data stays
readable by the legacy service; only the password hashes of accounts that logged in here are in the new
format, so set `UPGRADE_LEGACY_HASHES=false` while a rollback must stay possible.

**Build.** The dependencies are vendored (Linux x86_64 and aarch64) and the image is built with
`cargo build --offline --locked`; the runtime image needs no package installation and runs as a non-root
user with a built-in healthcheck. Verified with `docker build --network none --no-cache .`. To refresh
the vendor directory: `cargo vendor-filterer --platform=x86_64-unknown-linux-gnu
--platform=aarch64-unknown-linux-gnu --exclude-crate-path '*#tests' --exclude-crate-path '*#benches' vendor`.

## Deliberate differences from the legacy service

### Security
- Admin routes without a token returned data; now `401` (a valid non-admin token: `403`).
- The bank webhook accepted unsigned calls; the signature is required and compared in constant time.
- SQL injection through `sort`, `group_by`, the keys of `PATCH /users/me` and `import-products`:
  identifiers are allowlisted and values are bound parameters. `PATCH /users/me` only changes `name`
  and `email` (it let callers set their own `role` or balance).
- Path traversal on report download and shell injection through the export filename (`gzip` ran in a
  shell): file names must be plain names, compression is done in-process.
- Any logged-in user could read, cancel or refund any order and read its invoice; now only the owner or
  an admin can, everybody else gets `404`.
- `/api/admin/users` returned password hashes, and errors carried stack traces; both removed.
- The in-memory audit log that kept every request body, passwords included, forever is gone.

### Money and consistency
- No overselling, no double spend, a single-use coupon is consumed once, `order_no` comes from a sequence,
  and a rejected order leaves no orphan row or leaked stock.
- Amounts must be positive finite numbers and quantities at least 1 (negative or NaN top-ups and
  transfers used to mint money); no transfer to oneself. All answered with `400`.
- Refunds: at most what was received minus what was refunded, in as many parts as wanted (the default
  amount is what is left), never on a pending or cancelled order. Invalid amount `400`, nothing left `409`.
  A cancel after a partial refund returns only the remainder.
- Admin status changes cannot move money or stock behind the ledger's back (see Design).
- One bank charge per order (the legacy retries created duplicate charges), recorded even if the client
  disconnects.
- Product edits are a single `UPDATE`, so a concurrent order's stock change is never overwritten. Prices
  must be finite and non-negative, stock a non-negative 32-bit integer; ids outside the 32-bit range are
  "not found" instead of wrapping onto another product.

### Contract and input handling
- Invalid `sort`, `group_by`, admin `role` and order `status` values are `400` (legacy: SQL error `500`,
  or silently accepted). `POST /admin/users/{id}/role` requires a `role`.
- A body declared as JSON that cannot be parsed is `400`. The legacy code treated it as `{}`, so a
  truncated refund request refunded the whole order.
- All errors, including unknown routes (`404`) and wrong methods (`405`), are `{"error": ...}`.
- The sales report is no longer cached forever (it returned stale numbers) and uses two SQL aggregates
  instead of nested loops; order lists load their items with one query.
- `page < 1` is page 1 (legacy: `500`). A page holds at most 10 000 rows (`limit=-1` still means "all",
  up to that cap), an order at most 100 items, and `%` / `_` in a product search are literal characters.
- The webhook's `settled` event also sends the `paid` callback when it is what moves the order to paid.
- `import-products` understands quoted CSV, skips SKUs that already exist and never builds SQL from file contents.
- CSV exports prefix text cells that start with `= + - @` with an apostrophe so spreadsheets do not evaluate them.

## Kept as it was

- Timestamps are rendered as the legacy service did (`Thu, 12 Nov 2026 10:00:00 GMT`; order times are
  Jakarta wall-clock values), and totals are computed and rounded with the same arithmetic.
- Lenient input coercion (`"12abc"` as an integer, numeric strings as amounts) and the defaults of the
  legacy endpoints where the spec is silent.
- Public status codes and response shapes, the seeded data, and the environment variable names.

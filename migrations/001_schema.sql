-- Legacy tables, unchanged (CREATE IF NOT EXISTS so an existing production DB is left alone).
CREATE TABLE IF NOT EXISTS users (
    id SERIAL PRIMARY KEY,
    username TEXT,
    email TEXT,
    name TEXT,
    password_hash TEXT,
    role TEXT DEFAULT 'user',
    wallet_balance DOUBLE PRECISION DEFAULT 0,
    created_at TIMESTAMP DEFAULT now()
);
CREATE TABLE IF NOT EXISTS products (
    id SERIAL PRIMARY KEY,
    sku TEXT,
    name TEXT,
    price DOUBLE PRECISION,
    stock INTEGER,
    created_at TIMESTAMP DEFAULT now()
);
CREATE TABLE IF NOT EXISTS coupons (
    id SERIAL PRIMARY KEY,
    code TEXT,
    percent INTEGER,
    expires TEXT,
    single_use BOOLEAN DEFAULT FALSE,
    used_count INTEGER DEFAULT 0
);
CREATE TABLE IF NOT EXISTS orders (
    id SERIAL PRIMARY KEY,
    order_no INTEGER,
    user_id INTEGER,
    status TEXT,
    total DOUBLE PRECISION,
    paid_amount DOUBLE PRECISION DEFAULT 0,
    refunded DOUBLE PRECISION DEFAULT 0,
    coupon TEXT,
    notify_url TEXT,
    created_at TIMESTAMP
);
CREATE TABLE IF NOT EXISTS order_items (
    id SERIAL PRIMARY KEY,
    order_id INTEGER,
    product_id INTEGER,
    quantity INTEGER,
    price DOUBLE PRECISION
);
CREATE TABLE IF NOT EXISTS payments (
    id SERIAL PRIMARY KEY,
    order_id INTEGER,
    amount DOUBLE PRECISION,
    method TEXT,
    bank_ref TEXT,
    status TEXT,
    created_at TIMESTAMP DEFAULT now()
);

-- New (additive): persistent sessions, so logout and `manage purge-sessions` really work.
CREATE TABLE IF NOT EXISTS sessions (
    token_hash TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- New (additive): failed login attempts, for the brute-force limiter.
CREATE TABLE IF NOT EXISTS login_failures (
    id BIGSERIAL PRIMARY KEY,
    username TEXT NOT NULL,
    failed_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- New (additive): race-free order numbers, continuing after the highest existing one.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_class WHERE relkind = 'S' AND relname = 'order_no_seq') THEN
        CREATE SEQUENCE order_no_seq;
        PERFORM setval('order_no_seq', GREATEST(COALESCE((SELECT MAX(order_no) FROM orders), 1000), 1000));
    END IF;
END
$$;

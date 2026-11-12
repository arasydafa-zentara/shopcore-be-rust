-- Best effort: each statement runs on its own and a failure (for example duplicate rows already in
-- a legacy database) is logged as a warning instead of stopping the service.
CREATE UNIQUE INDEX IF NOT EXISTS users_username_uq ON users (username);
CREATE UNIQUE INDEX IF NOT EXISTS users_email_uq ON users (email);
CREATE UNIQUE INDEX IF NOT EXISTS orders_order_no_uq ON orders (order_no);
CREATE INDEX IF NOT EXISTS orders_user_id_idx ON orders (user_id, id);
CREATE INDEX IF NOT EXISTS orders_created_at_idx ON orders (created_at);
CREATE INDEX IF NOT EXISTS order_items_order_id_idx ON order_items (order_id);
CREATE INDEX IF NOT EXISTS payments_order_id_idx ON payments (order_id);
CREATE INDEX IF NOT EXISTS products_sku_idx ON products (sku);
CREATE INDEX IF NOT EXISTS sessions_user_id_idx ON sessions (user_id);
CREATE INDEX IF NOT EXISTS login_failures_idx ON login_failures (ip, username, failed_at);
CREATE INDEX IF NOT EXISTS payments_pending_idx ON payments (order_id) WHERE status = 'pending'

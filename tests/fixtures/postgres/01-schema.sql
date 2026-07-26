-- PostgreSQL fixture.
--
-- Mirrors the MySQL fixture where it makes sense, plus PostgreSQL-specific
-- hazards: a non-public schema, a SECURITY DEFINER function, an enum type,
-- a bytea column with invalid-UTF8 bytes, a deferrable foreign-key cycle,
-- reserved-word and unicode identifiers.

\connect fixture

CREATE SCHEMA IF NOT EXISTS reporting;

CREATE TYPE order_status AS ENUM ('pending', 'paid', 'shipped', 'cancelled');

CREATE TABLE users (
    id           BIGSERIAL PRIMARY KEY,
    email        TEXT        NOT NULL UNIQUE,
    display_name TEXT        NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE user_profiles (
    user_id BIGINT PRIMARY KEY REFERENCES users (id),
    bio     TEXT,
    avatar  BYTEA
);

CREATE TABLE roles (
    id   SERIAL PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);

CREATE TABLE role_user (
    role_id INT    NOT NULL REFERENCES roles (id),
    user_id BIGINT NOT NULL REFERENCES users (id),
    PRIMARY KEY (role_id, user_id)
);

CREATE TABLE categories (
    id        SERIAL PRIMARY KEY,
    parent_id INT REFERENCES categories (id),
    name      TEXT NOT NULL
);

CREATE TABLE products (
    id          BIGSERIAL PRIMARY KEY,
    sku         TEXT NOT NULL UNIQUE,
    name        TEXT NOT NULL,
    category_id INT REFERENCES categories (id),
    price_cents INT  NOT NULL DEFAULT 0,
    thumbnail   BYTEA
);

CREATE TABLE orders (
    id          BIGSERIAL PRIMARY KEY,
    user_id     BIGINT       NOT NULL REFERENCES users (id),
    total_cents INT          NOT NULL DEFAULT 0,
    status      order_status NOT NULL DEFAULT 'pending',
    placed_at   TIMESTAMPTZ  NOT NULL DEFAULT now()
);

CREATE TABLE order_items (
    id         BIGSERIAL PRIMARY KEY,
    order_id   BIGINT NOT NULL REFERENCES orders (id),
    product_id BIGINT NOT NULL REFERENCES products (id),
    quantity   INT    NOT NULL DEFAULT 1
);

-- Deferrable cycle: employees.department_id <-> departments.head_id.
CREATE TABLE employees (
    id            SERIAL PRIMARY KEY,
    name          TEXT NOT NULL,
    department_id INT
);

CREATE TABLE departments (
    id      SERIAL PRIMARY KEY,
    name    TEXT NOT NULL,
    head_id INT REFERENCES employees (id) DEFERRABLE INITIALLY DEFERRED
);

ALTER TABLE employees
    ADD CONSTRAINT fk_emp_dept FOREIGN KEY (department_id)
    REFERENCES departments (id) DEFERRABLE INITIALLY DEFERRED;

-- Reserved words as identifiers.
CREATE TABLE "order" (
    id     SERIAL PRIMARY KEY,
    "key"  TEXT NOT NULL,
    "from" TEXT
);

CREATE TABLE "select" (
    id    SERIAL PRIMARY KEY,
    "all" TEXT
);

-- Non-ASCII identifiers.
CREATE TABLE "日本語テーブル" (
    id     SERIAL PRIMARY KEY,
    "名前" TEXT NOT NULL,
    "説明" TEXT
);

CREATE TABLE "naïve_café" (
    id    SERIAL PRIMARY KEY,
    ville TEXT NOT NULL
);

CREATE TABLE attachments (
    id       BIGSERIAL PRIMARY KEY,
    filename TEXT  NOT NULL,
    payload  BYTEA NOT NULL,
    checksum BYTEA
);

CREATE TABLE audit_log (
    id         BIGSERIAL PRIMARY KEY,
    actor      TEXT        NOT NULL,
    action     TEXT        NOT NULL,
    payload    JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE sessions (
    id         UUID PRIMARY KEY,
    user_id    BIGINT REFERENCES users (id),
    expires_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE shipments (
    id       BIGSERIAL PRIMARY KEY,
    order_id BIGINT NOT NULL REFERENCES orders (id),
    carrier  TEXT   NOT NULL,
    tracking TEXT
);

CREATE TABLE payments (
    id           BIGSERIAL PRIMARY KEY,
    order_id     BIGINT NOT NULL REFERENCES orders (id),
    amount_cents INT    NOT NULL,
    method       TEXT   NOT NULL
);

-- Table in a non-public schema: a dump restricted to `public` must not include
-- it, and a full dump must.
CREATE TABLE reporting.daily_totals (
    day         DATE PRIMARY KEY,
    order_count INT  NOT NULL DEFAULT 0,
    cents       BIGINT NOT NULL DEFAULT 0
);

-- ── Seed data ───────────────────────────────────────────────────────────

INSERT INTO users (email, display_name) VALUES
    ('ada@example.com',   'Ada Lovelace'),
    ('alan@example.com',  'Alan Turing'),
    ('grace@example.com', 'Grace Hopper');

INSERT INTO roles (name) VALUES ('admin'), ('editor'), ('viewer');
INSERT INTO role_user (role_id, user_id) VALUES (1, 1), (2, 2), (3, 3);

INSERT INTO categories (parent_id, name) VALUES (NULL, 'Root'), (1, 'Widgets');

INSERT INTO products (sku, name, category_id, price_cents, thumbnail) VALUES
    ('SKU-001', 'Widget',     2, 1999, '\x89504e470d0a1a0a'::bytea),
    ('SKU-002', 'Gadget',     2, 4999, '\xffd8ffe000104a464946'::bytea),
    ('SKU-003', 'Café Crème', 2,  350, NULL);

INSERT INTO orders (user_id, total_cents, status) VALUES
    (1, 1999, 'paid'), (2, 4999, 'pending');

INSERT INTO order_items (order_id, product_id, quantity) VALUES (1, 1, 1), (2, 2, 1);

BEGIN;
SET CONSTRAINTS ALL DEFERRED;
INSERT INTO departments (id, name, head_id) VALUES (1, 'Engineering', 1);
INSERT INTO employees (id, name, department_id) VALUES (1, 'Ada', 1), (2, 'Alan', 1);
COMMIT;

SELECT setval('departments_id_seq', 1, true);
SELECT setval('employees_id_seq', 2, true);

INSERT INTO "order" ("key", "from") VALUES ('reserved', 'words');
INSERT INTO "select" ("all") VALUES ('keyword');

INSERT INTO "日本語テーブル" ("名前", "説明") VALUES ('テスト', 'ユニコード識別子');
INSERT INTO "naïve_café" (ville) VALUES ('Zürich'), ('São Paulo');

INSERT INTO attachments (filename, payload, checksum) VALUES
    ('binary.bin', '\xdeadbeef00ff00ffc3289f'::bytea, decode(repeat('ab', 32), 'hex')),
    ('empty.bin',  '\x'::bytea, NULL);

INSERT INTO audit_log (actor, action, payload) VALUES
    ('system', 'noted that DEFINER=`root`@`localhost` was set', NULL),
    ('system', 'quote test: it''s fine', '{"k":"v"}'::jsonb);

INSERT INTO settings (key, value) VALUES
    ('definer_note', 'DEFINER=`root`@`localhost`'),
    ('greeting', 'hello');

INSERT INTO reporting.daily_totals (day, order_count, cents)
VALUES (DATE '2026-01-01', 2, 6998);

-- ── Views and functions ─────────────────────────────────────────────────

CREATE VIEW active_users AS
    SELECT id, email, display_name FROM users;

CREATE VIEW order_totals AS
    SELECT o.id AS order_id, u.email, o.total_cents
    FROM orders o JOIN users u ON u.id = o.user_id;

CREATE FUNCTION order_item_count(p_order_id BIGINT)
RETURNS INT
LANGUAGE sql
STABLE
AS $$ SELECT COUNT(*)::INT FROM order_items WHERE order_id = p_order_id $$;

-- SECURITY DEFINER: the PostgreSQL analogue of the MySQL DEFINER hazard.
-- Restoring this as a non-superuser without --no-owner tends to fail.
CREATE FUNCTION recalc_order_total(p_order_id BIGINT)
RETURNS VOID
LANGUAGE plpgsql
SECURITY DEFINER
AS $$
BEGIN
    UPDATE orders o
       SET total_cents = COALESCE((
           SELECT SUM(p.price_cents * i.quantity)
             FROM order_items i
             JOIN products p ON p.id = i.product_id
            WHERE i.order_id = o.id
       ), 0)
     WHERE o.id = p_order_id;
END;
$$;

CREATE FUNCTION trg_orders_audit() RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO audit_log (actor, action)
    VALUES ('trigger', 'order ' || NEW.id || ' created');
    RETURN NEW;
END;
$$;

CREATE TRIGGER orders_audit
AFTER INSERT ON orders
FOR EACH ROW EXECUTE FUNCTION trg_orders_audit();

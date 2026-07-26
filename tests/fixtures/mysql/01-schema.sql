-- MySQL fixture.
--
-- Deliberately contains the things that break naive dump/restore tooling:
-- a routine and a view carrying DEFINER clauses, a trigger, BLOB columns,
-- a foreign-key cycle, a reserved-word table name, a unicode table name,
-- a MyISAM table (not covered by --single-transaction), and row data that
-- contains the literal text "DEFINER=" to catch over-eager stripping.

-- The entrypoint sources this file with the client's default charset, which is
-- not utf8mb4 on every image. Without this line the non-ASCII identifiers below
-- are read as latin1 and stored double-encoded ("naïve_café" becomes
-- "naÃ¯ve_cafÃ©"), which a client doing the right thing then cannot find.
SET NAMES utf8mb4;

USE fixture;

SET FOREIGN_KEY_CHECKS = 0;

CREATE TABLE users (
    id            BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
    email         VARCHAR(255)    NOT NULL UNIQUE,
    display_name  VARCHAR(255)    NOT NULL,
    created_at    DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

CREATE TABLE user_profiles (
    user_id  BIGINT UNSIGNED NOT NULL PRIMARY KEY,
    bio      TEXT,
    avatar   MEDIUMBLOB,
    CONSTRAINT fk_profile_user FOREIGN KEY (user_id) REFERENCES users (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

CREATE TABLE roles (
    id    INT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    name  VARCHAR(64) NOT NULL UNIQUE
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

CREATE TABLE role_user (
    role_id  INT             NOT NULL,
    user_id  BIGINT UNSIGNED NOT NULL,
    PRIMARY KEY (role_id, user_id),
    CONSTRAINT fk_ru_role FOREIGN KEY (role_id) REFERENCES roles (id),
    CONSTRAINT fk_ru_user FOREIGN KEY (user_id) REFERENCES users (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

CREATE TABLE categories (
    id         INT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    parent_id  INT NULL,
    name       VARCHAR(128) NOT NULL,
    CONSTRAINT fk_cat_parent FOREIGN KEY (parent_id) REFERENCES categories (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

CREATE TABLE products (
    id          BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
    sku         VARCHAR(64)     NOT NULL UNIQUE,
    name        VARCHAR(255)    NOT NULL,
    category_id INT             NULL,
    price_cents INT             NOT NULL DEFAULT 0,
    thumbnail   BLOB,
    CONSTRAINT fk_product_category FOREIGN KEY (category_id) REFERENCES categories (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

CREATE TABLE orders (
    id          BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
    user_id     BIGINT UNSIGNED NOT NULL,
    total_cents INT             NOT NULL DEFAULT 0,
    status      VARCHAR(32)     NOT NULL DEFAULT 'pending',
    placed_at   DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT fk_order_user FOREIGN KEY (user_id) REFERENCES users (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

CREATE TABLE order_items (
    id          BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
    order_id    BIGINT UNSIGNED NOT NULL,
    product_id  BIGINT UNSIGNED NOT NULL,
    quantity    INT             NOT NULL DEFAULT 1,
    CONSTRAINT fk_item_order FOREIGN KEY (order_id) REFERENCES orders (id),
    CONSTRAINT fk_item_product FOREIGN KEY (product_id) REFERENCES products (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

-- Foreign-key cycle: each table references the other. A restore that does not
-- disable FK checks will fail here.
CREATE TABLE employees (
    id            INT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    name          VARCHAR(128) NOT NULL,
    department_id INT NULL
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

CREATE TABLE departments (
    id       INT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    name     VARCHAR(128) NOT NULL,
    head_id  INT NULL,
    CONSTRAINT fk_dept_head FOREIGN KEY (head_id) REFERENCES employees (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

ALTER TABLE employees
    ADD CONSTRAINT fk_emp_dept FOREIGN KEY (department_id) REFERENCES departments (id);

-- Reserved word as a table name.
CREATE TABLE `order` (
    id     INT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    `key`  VARCHAR(64) NOT NULL,
    `from` VARCHAR(64) NULL
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

-- Non-ASCII table and column identifiers.
CREATE TABLE `日本語テーブル` (
    id       INT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    `名前`    VARCHAR(128) NOT NULL,
    `説明`    TEXT
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

CREATE TABLE `naïve_café` (
    id    INT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    ville VARCHAR(128) NOT NULL
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

-- Binary payloads, including bytes that are invalid UTF-8. Without --hex-blob
-- these corrupt in transit.
CREATE TABLE attachments (
    id        BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
    filename  VARCHAR(255) NOT NULL,
    payload   LONGBLOB     NOT NULL,
    checksum  BINARY(32)   NULL
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

-- MyISAM: --single-transaction does not apply, so the UI must warn.
CREATE TABLE legacy_stats (
    id     INT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    metric VARCHAR(64) NOT NULL,
    value  BIGINT      NOT NULL DEFAULT 0
) ENGINE = MyISAM DEFAULT CHARSET = utf8mb4;

CREATE TABLE audit_log (
    id         BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
    actor      VARCHAR(255) NOT NULL,
    action     VARCHAR(255) NOT NULL,
    payload    JSON         NULL,
    created_at DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

CREATE TABLE sessions (
    id         CHAR(36)     NOT NULL PRIMARY KEY,
    user_id    BIGINT UNSIGNED NULL,
    expires_at DATETIME     NOT NULL
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

CREATE TABLE settings (
    `key`   VARCHAR(128) NOT NULL PRIMARY KEY,
    `value` TEXT         NOT NULL
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

CREATE TABLE shipments (
    id       BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
    order_id BIGINT UNSIGNED NOT NULL,
    carrier  VARCHAR(64)     NOT NULL,
    tracking VARCHAR(128)    NULL,
    CONSTRAINT fk_ship_order FOREIGN KEY (order_id) REFERENCES orders (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

CREATE TABLE payments (
    id          BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
    order_id    BIGINT UNSIGNED NOT NULL,
    amount_cents INT            NOT NULL,
    method      VARCHAR(32)     NOT NULL,
    CONSTRAINT fk_pay_order FOREIGN KEY (order_id) REFERENCES orders (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

SET FOREIGN_KEY_CHECKS = 1;

-- ── Seed data ───────────────────────────────────────────────────────────

INSERT INTO users (email, display_name) VALUES
    ('ada@example.com',  'Ada Lovelace'),
    ('alan@example.com', 'Alan Turing'),
    ('grace@example.com','Grace Hopper');

INSERT INTO roles (name) VALUES ('admin'), ('editor'), ('viewer');

INSERT INTO role_user (role_id, user_id) VALUES (1, 1), (2, 2), (3, 3);

INSERT INTO categories (parent_id, name) VALUES (NULL, 'Root'), (1, 'Widgets');

INSERT INTO products (sku, name, category_id, price_cents, thumbnail) VALUES
    ('SKU-001', 'Widget',       2, 1999, UNHEX('89504E470D0A1A0A0000000D49484452')),
    ('SKU-002', 'Gadget',       2, 4999, UNHEX('FFD8FFE000104A46494600010100')),
    ('SKU-003', 'Café Crème',   2,  350, NULL);

INSERT INTO orders (user_id, total_cents, status) VALUES
    (1, 1999, 'paid'), (2, 4999, 'pending');

INSERT INTO order_items (order_id, product_id, quantity) VALUES
    (1, 1, 1), (2, 2, 1);

INSERT INTO departments (name, head_id) VALUES ('Engineering', NULL);
INSERT INTO employees (name, department_id) VALUES ('Ada', 1), ('Alan', 1);
UPDATE departments SET head_id = 1 WHERE id = 1;

INSERT INTO `order` (`key`, `from`) VALUES ('reserved', 'words');

INSERT INTO `日本語テーブル` (`名前`, `説明`) VALUES
    ('テスト', 'ユニコードのテーブル名と列名');

INSERT INTO `naïve_café` (ville) VALUES ('Zürich'), ('São Paulo');

-- Invalid UTF-8 byte sequences: these must survive a round trip.
INSERT INTO attachments (filename, payload, checksum) VALUES
    ('binary.bin', UNHEX('DEADBEEF00FF00FFC3289F'), UNHEX(REPEAT('AB', 32))),
    ('empty.bin',  '', NULL);

INSERT INTO legacy_stats (metric, value) VALUES ('visits', 42);

-- Row data containing the literal text a naive DEFINER filter would corrupt.
INSERT INTO audit_log (actor, action, payload) VALUES
    ('system', 'noted that DEFINER=`root`@`localhost` was set', NULL),
    ('system', 'quote test: it''s fine', JSON_OBJECT('k', 'v'));

INSERT INTO settings (`key`, `value`) VALUES
    ('definer_note', 'DEFINER=`root`@`localhost`'),
    ('greeting', 'hello');

-- ── Routines, views and triggers (all carry DEFINER) ────────────────────

CREATE DEFINER = `root`@`%` VIEW active_users AS
    SELECT id, email, display_name FROM users WHERE id IS NOT NULL;

CREATE DEFINER = `root`@`%` VIEW order_totals AS
    SELECT o.id AS order_id, u.email, o.total_cents
    FROM orders o JOIN users u ON u.id = o.user_id;

DELIMITER //

CREATE DEFINER = `root`@`%` PROCEDURE recalc_order_total(IN p_order_id BIGINT UNSIGNED)
BEGIN
    UPDATE orders o
       SET o.total_cents = (
           SELECT COALESCE(SUM(p.price_cents * i.quantity), 0)
             FROM order_items i
             JOIN products p ON p.id = i.product_id
            WHERE i.order_id = o.id
       )
     WHERE o.id = p_order_id;
END //

CREATE DEFINER = `root`@`%` FUNCTION order_item_count(p_order_id BIGINT UNSIGNED)
RETURNS INT
DETERMINISTIC
READS SQL DATA
BEGIN
    DECLARE n INT;
    SELECT COUNT(*) INTO n FROM order_items WHERE order_id = p_order_id;
    RETURN n;
END //

CREATE DEFINER = `root`@`%` TRIGGER trg_orders_audit
AFTER INSERT ON orders
FOR EACH ROW
BEGIN
    INSERT INTO audit_log (actor, action)
    VALUES ('trigger', CONCAT('order ', NEW.id, ' created'));
END //

DELIMITER ;

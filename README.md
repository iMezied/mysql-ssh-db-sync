# db-migrate

> MySQL cross-server backup and restore via SSH tunnels and a local Docker container.
> Designed for production databases: selective schema-only or schema+data backup per table, with full progress tracking.

```
┌─────────────────────┐        SSH Tunnel         ┌──────────────────────┐
│   Source Server A   │ ────────────────────────► │   Your Mac (Docker)  │
│   (Germany)         │                           │   MySQL 8 Container  │
└─────────────────────┘                           └──────────┬───────────┘
                                                             │
                                                     SSH Tunnel
                                                             │
                                                  ┌──────────▼───────────┐
                                                  │  Destination Server B │
                                                  │  (Malaysia)           │
                                                  └──────────────────────┘
```

---

## Table of Contents

- [Overview](#overview)
- [How It Works](#how-it-works)
- [Requirements](#requirements)
- [Project Structure](#project-structure)
- [Installation](#installation)
- [Configuration](#configuration)
  - [Environment Variables (.env)](#environment-variables-env)
  - [Tables Configuration (tables.conf)](#tables-configuration-tablesconf)
- [Usage](#usage)
  - [Full Pipeline](#full-pipeline)
  - [Backup Only](#backup-only)
  - [Restore Only](#restore-only)
  - [Dry Run](#dry-run)
- [What Gets Backed Up](#what-gets-backed-up)
- [Output & Progress](#output--progress)
- [Backup File Naming](#backup-file-naming)
- [Destination Database Naming](#destination-database-naming)
- [Security](#security)
- [Troubleshooting](#troubleshooting)
- [Git Setup](#git-setup)

---

## Overview

`db-migrate` is a Bash automation script that connects to two remote MySQL servers via SSH tunnels, backs up a production database with granular table-level control, and restores it to a destination server — all from your local Mac without installing MySQL directly on your machine.

**Key capabilities:**

- Schema-only backup for all tables in the database
- Schema + data backup for a selected list of tables (defined in a config file)
- Everything packed into a single compressed `.sql.gz` file
- Live progress bars, spinners, and size monitors throughout every step
- Per-step timing with a full summary table at the end
- Safe for production: uses `--single-transaction`, no table locks on InnoDB
- Strips `DEFINER` clauses automatically — no `SUPER` privilege required on destination
- Each restore creates a uniquely named database — source is never touched

---

## How It Works

The script runs through up to 8 sequential steps depending on the mode:

| Step | Name | Description |
|------|------|-------------|
| **0** | Pre-flight Checks | Validates Docker, SSH keys, disk space, tables config |
| **1** | SSH Tunnel → Source | Opens an SSH tunnel to Server A on a local port |
| **2** | Schema Dump | Dumps all table schemas (no data) via `mysqldump --no-data` |
| **3** | Data Dump | Dumps data for selected tables via `mysqldump --no-create-info` |
| **4** | Compress | Gzips the combined `.sql` into a single `.sql.gz` file, closes Source tunnel |
| **5** | SSH Tunnel → Destination | Opens an SSH tunnel to Server B on a separate local port |
| **6** | Create Database | Creates a new database on Server B with a timestamped name |
| **7** | Restore | Streams the backup file into the new database via `pv` or plain pipe |
| **8** | Verification | Queries the restored DB for table count, row stats, and top 5 largest tables |

The two-pass dump strategy (schema first, data second into the same file) allows you to control exactly which tables carry data, while ensuring all 250+ table structures are always present on the destination.

---

## Requirements

| Requirement | Notes |
|---|---|
| **macOS** | Tested on macOS Ventura / Sonoma |
| **Docker Desktop** | Must be running with a MySQL 8 container active |
| **SSH access** | Key-based SSH access to both servers |
| **bash 3.2+** | Ships with macOS — no upgrade needed |
| **`pv`** *(optional)* | Enables byte-level progress bar during restore — `brew install pv` |
| **`bc`** *(optional)* | Used for size formatting — usually pre-installed on macOS |

### Setting Up the Local MySQL 8 Docker Container

If you don't already have a MySQL 8 container running:

```bash
docker run \
  --name mysql8 \
  -e MYSQL_ROOT_PASSWORD=your_password \
  -p 3306:3306 \
  -d mysql:8
```

Verify it is running:

```bash
docker ps --filter name=mysql8
```

---

## Project Structure

```
db-migrate/
├── db_migrate.sh               # Main entry point — run this
├── .env                        # Your configuration (git-ignored)
├── .env.example                # Configuration template (safe to commit)
├── .gitignore                  # Excludes .env, tables.conf, backups, logs
├── README.md                   # This file
├── config/
│   ├── tables.conf             # Your table list (git-ignored)
│   └── tables.conf.example     # Table list template (safe to commit)
└── scripts/
    ├── ui.sh                   # Terminal UI: colors, progress bars, spinners, timers
    └── validate.sh             # Config and environment validation logic
```

---

## Installation

### 1. Clone the repository

```bash
git clone https://github.com/yourname/db-migrate.git
cd db-migrate
```

### 2. Make the script executable

```bash
chmod +x db_migrate.sh
```

### 3. Create your environment file

```bash
cp .env.example .env
```

Edit `.env` and fill in your server details — see [Configuration](#configuration) below.

### 4. Create your tables configuration

```bash
cp config/tables.conf.example config/tables.conf
```

Edit `config/tables.conf` and add the table names that should be backed up with data — one table per line.

### 5. Install optional dependencies

```bash
# Strongly recommended — enables byte-level progress bar during restore
brew install pv
```

### 6. Validate your setup

```bash
./db_migrate.sh --dry-run
```

This checks your entire configuration without making any changes to either server.

---

## Configuration

### Environment Variables (`.env`)

```dotenv
# ── Local Docker ─────────────────────────────────────────────────
# Name of your running MySQL 8 Docker container
DOCKER_CONTAINER=mysql8

# ── Backup Storage ───────────────────────────────────────────────
# Where .sql.gz backup files are saved on your Mac
# The ~ shorthand is supported
BACKUP_DIR=~/mysql_backups

# ── Source Server A ──────────────────────────────────────────────
SRC_SSH_USER=ubuntu
SRC_SSH_HOST=1.2.3.4
SRC_SSH_PORT=22
SRC_SSH_KEY=~/.ssh/id_rsa

SRC_DB_HOST=127.0.0.1          # MySQL host as seen from Server A (usually 127.0.0.1)
SRC_DB_PORT=3306
SRC_DB_USER=root
SRC_DB_PASS=your_source_password
SRC_DB_NAME=your_database_name

SRC_LOCAL_PORT=13306            # Free local port to bind the SSH tunnel

# ── Destination Server B ─────────────────────────────────────────
DST_SSH_USER=ubuntu
DST_SSH_HOST=5.6.7.8
DST_SSH_PORT=22
DST_SSH_KEY=~/.ssh/id_rsa

DST_DB_HOST=127.0.0.1
DST_DB_PORT=3306
DST_DB_USER=root
DST_DB_PASS=your_destination_password

DST_LOCAL_PORT=13307            # Must be different from SRC_LOCAL_PORT

# ── Destination Database Naming ───────────────────────────────────
# Final DB name will be: DB_PREFIX + _ + YYYYMMDD_HHmmss
# Example: restore_20250315_143022
DB_PREFIX=restore

# ── Tables Configuration ──────────────────────────────────────────
# Path to the file listing tables that need schema + data backup
# Relative paths are resolved from the project root
TABLES_FILE=./config/tables.conf

# ── Dump Options ──────────────────────────────────────────────────
DUMP_ROUTINES=true              # Include stored procedures and functions
DUMP_TRIGGERS=true              # Include triggers
DUMP_EVENTS=true                # Include scheduled events
COMPRESS_BACKUP=true            # Gzip the output file
```

### Tables Configuration (`config/tables.conf`)

One table name per line. Lines starting with `#` are treated as comments and ignored. Blank lines are ignored.

```
# Orders
orders
order_items
order_status_history

# Users
users
user_profiles
roles
permissions
```

**Every table listed here** → backed up with **schema + data**

**Every other table in the database** → backed up with **schema only** (structure preserved, no rows)

There is no limit on the number of tables. The file supports hundreds of entries organized into any comment-based groupings you prefer.

---

## Usage

### Full Pipeline

Runs all 8 steps: connects to Source, dumps schema and data, compresses, connects to Destination, creates DB, restores, verifies.

```bash
./db_migrate.sh
```

### Backup Only

Connects to Source, runs the full dump and compression, saves the `.sql.gz` file locally. Stops before touching the Destination server. Useful for scheduled backups or when you want to review the file before restoring.

```bash
./db_migrate.sh --backup
```

### Restore Only

Skips the dump entirely. Presents a numbered list of `.sql.gz` files available in your `BACKUP_DIR`, lets you choose one, then connects to Destination and restores it into a new database.

```bash
./db_migrate.sh --restore
```

Example prompt:

```
  Available backups:
  ───────────────────────────────────────────────────────
  [ 1]  mydb_20250315_143022.sql.gz          142 MB   2025-03-15 14:30
  [ 2]  mydb_20250314_090011.sql.gz          139 MB   2025-03-14 09:00
  [ 3]  mydb_20250313_021500.sql.gz          138 MB   2025-03-13 02:15
  ───────────────────────────────────────────────────────

  Select backup [1-3]:
```

### Dry Run

Validates your entire configuration without opening any tunnels or touching any database. Checks Docker, SSH keys, disk space, tables config, and all required `.env` values.

```bash
./db_migrate.sh --dry-run
```

---

## What Gets Backed Up

| Tables | Schema | Data |
|--------|--------|------|
| All tables in the source database | ✅ Always | ❌ |
| Tables listed in `tables.conf` | ✅ Always | ✅ |

The result is a single `.sql` file (then compressed to `.sql.gz`) that contains:

1. `CREATE TABLE` statements for every table in the database
2. `INSERT` statements only for tables listed in `tables.conf`
3. Stored routines, triggers, and events (if enabled in `.env`)

All `DEFINER=` clauses are automatically stripped from the dump, making it safe to restore on any server without requiring `SUPER` privileges.

---

## Output & Progress

The script provides live feedback at every stage:

**Schema dump** — live file size monitor showing bytes written and elapsed time as the dump runs in the background

**Data dump** — animated progress bar per table showing:
  - Visual fill bar (25 segments)
  - Percentage complete
  - Current / total table count
  - Current table name
  - Elapsed time and ETA

**Compression** — spinner with size while gzip runs, then shows original size → compressed size → reduction percentage

**Restore with `pv` installed** — byte-level progress bar showing:
  - Bytes transferred
  - Transfer throughput
  - Elapsed time
  - ETA to completion

**Restore without `pv`** — animated spinner showing file size and elapsed time

**Verification** — after restore, queries the destination database and reports:
  - Total table count
  - Tables with data vs schema-only
  - DB size on disk
  - Top 5 largest tables (by size) with row counts

**Summary** — step-by-step timing table showing duration per step and total elapsed time

---

## Backup File Naming

Backup files are saved to `BACKUP_DIR` with the following naming convention:

```
{SOURCE_DB_NAME}_{YYYYMMDD}_{HHmmss}.sql.gz
```

Example:
```
~/mysql_backups/baredex_app_20250315_143022.sql.gz
```

Backup files are never deleted automatically. Manage retention manually or add a cron-based cleanup as needed.

---

## Destination Database Naming

Each restore creates a brand-new database on the Destination server. The name is auto-generated using your configured prefix and a timestamp:

```
{DB_PREFIX}_{YYYYMMDD}_{HHmmss}
```

Example with `DB_PREFIX=restore`:
```
restore_20250315_143022
```

This means every run is non-destructive — no existing database on the destination is ever modified or dropped.

---

## Security

| Concern | How it is handled |
|---|---|
| **Passwords in `.env`** | `.env` is git-ignored and never committed |
| **Table names in `tables.conf`** | `tables.conf` is git-ignored and never committed |
| **SSH connections** | Key-based only — no password authentication |
| **Data in transit** | All MySQL traffic travels inside the SSH tunnel (encrypted) |
| **Source database safety** | `--single-transaction` — no locks, no writes to source |
| **Destination safety** | Always creates a new uniquely named DB — never overwrites |
| **SUPER privilege error** | `DEFINER=` clauses stripped automatically from dump |
| **Backup files** | Stored locally on your Mac only — not uploaded anywhere |

---

## Troubleshooting

### `zsh: permission denied: ./db_migrate.sh`

```bash
chmod +x db_migrate.sh
```

To persist the permission in Git so it survives clones:

```bash
git update-index --chmod=+x db_migrate.sh
git commit -m "fix: set executable bit on db_migrate.sh"
```

---

### `mapfile: command not found`

Your system is using macOS's default bash 3.2. The script uses `while read` loops for compatibility, but if you see this in another context:

```bash
# Check your bash version
bash --version

# Install bash 5 via Homebrew (does not replace system bash)
brew install bash
```

---

### `Cannot reach Server A on port 13306 after 15 attempts`

- Confirm the SSH host and user are correct in `.env`
- Test your SSH key manually: `ssh -i ~/.ssh/id_rsa ubuntu@your-server`
- Check that MySQL is actually running on the source server
- Confirm `SRC_LOCAL_PORT` is not already in use: `lsof -i :13306`
- Try increasing the sleep before `wait_for_tunnel` in `step_open_source_tunnel` if the server is slow

---

### `ERROR 1227 (42000): Access denied; you need SUPER privilege`

This is caused by `DEFINER=` clauses in the dump. The script strips these automatically in Step 2. If you are using an older backup file that was generated without the strip:

```bash
# Strip DEFINER from an existing backup file
gunzip -c old_backup.sql.gz \
  | sed 's/DEFINER=[^ ]* / /g' \
  | gzip > fixed_backup.sql.gz
```

---

### `No .sql.gz files found` during `--restore`

Check that `BACKUP_DIR` in your `.env` points to the correct directory and that backup files exist there:

```bash
ls -lh ~/mysql_backups/
```

---

### Docker container not found

```bash
# List running containers
docker ps

# Start your MySQL 8 container if it is stopped
docker start mysql8
```

---

## Git Setup

Sensitive files are excluded from version control via `.gitignore`. Only templates and scripts are committed.

```
✅ Committed (safe)          ❌ Git-ignored (stays local)
─────────────────────────    ──────────────────────────────
db_migrate.sh                .env
scripts/ui.sh                config/tables.conf
scripts/validate.sh          *.sql
.env.example                 *.sql.gz
config/tables.conf.example   logs/
README.md
.gitignore
```

Initial repository setup:

```bash
git init
git add .
git update-index --chmod=+x db_migrate.sh
git commit -m "initial commit"
git remote add origin https://github.com/yourname/db-migrate.git
git push -u origin main
```

When setting up on a new machine:

```bash
git clone https://github.com/yourname/db-migrate.git
cd db-migrate
cp .env.example .env          # then edit with your values
cp config/tables.conf.example config/tables.conf   # then add your tables
./db_migrate.sh --dry-run
```

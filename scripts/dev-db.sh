#!/usr/bin/env bash
# Start a throwaway, user-owned PostgreSQL cluster for development and tests.
# Data and socket live under $HUB_PG_DIR (default: var/pg); nothing system-wide.
set -euo pipefail
dir="${HUB_PG_DIR:-$PWD/var/pg}"
port="${HUB_PG_PORT:-54371}"
if [ ! -f "$dir/PG_VERSION" ]; then
  mkdir -p "$dir"
  initdb -D "$dir" -U hub --auth=trust >/dev/null
fi
if ! pg_ctl -D "$dir" status >/dev/null 2>&1; then
  pg_ctl -D "$dir" -l "$dir/server.log" -o "-k $dir -p $port -c listen_addresses=''" start >/dev/null
fi
createdb -h "$dir" -p "$port" -U hub hub 2>/dev/null || true
echo "export DATABASE_URL='postgres://hub@localhost/hub?host=$dir&port=$port'"
echo "export HUB_TEST_DATABASE_URL='postgres://hub@localhost/hub?host=$dir&port=$port'"

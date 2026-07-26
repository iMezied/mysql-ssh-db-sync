#!/usr/bin/env bash
# Run the container-backed integration suites.
#
# WHY THIS EXISTS
#
# `cargo test --workspace` runs the integration binaries back to back in one
# process-per-binary sequence. When an SSH-heavy suite starts within roughly
# 30 seconds of the previous one finishing, about half of its SSH connects fail
# with `Disconnected` before authentication.
#
# What is established:
#   * Every suite passes reliably on its own, repeatedly (tunnel 12/12 across
#     four consecutive runs, introspect 15/15, profile_connect 4/4).
#   * A settle gap between suites clears it completely.
#   * `ssh(1)` opening 20 parallel sessions in the *same window* succeeds 20/20,
#     so the server is still accepting connections.
#   * sshd reports no lingering sessions and `0 of 200-400` startup slots used,
#     so it is not MaxStartups.
#   * No socket accumulation: exactly one TIME_WAIT against the published port.
#   * Closing the sqlx pools deterministically did not change it.
#
# The mechanism is still unidentified — it is tracked as an open issue rather
# than papered over silently. It has not been observed to affect the
# application, which opens one tunnel per job rather than a dozen at once, and
# the full profile → keychain → tunnel → introspect path passes end to end.
#
# Usage:
#   docker compose -f docker-compose.test.yml up -d --wait
#   tests/run-integration.sh

set -uo pipefail

SETTLE="${DBSYNC_TEST_SETTLE:-20}"
SUITES=(store introspect tunnel profile_connect)

export DBSYNC_REQUIRE_CONTAINERS=1

failures=0
first=1

for suite in "${SUITES[@]}"; do
    # Only pay the settle cost between suites, not before the first.
    if [ "$first" -eq 0 ]; then
        sleep "$SETTLE"
    fi
    first=0

    echo "── ${suite} ──────────────────────────────────────────────"
    if cargo test -p db-sync-engine --test "$suite"; then
        echo "   ${suite}: ok"
    else
        echo "   ${suite}: FAILED"
        failures=$((failures + 1))
    fi
    echo
done

if [ "$failures" -gt 0 ]; then
    echo "${failures} integration suite(s) failed"
    exit 1
fi

echo "all integration suites passed"

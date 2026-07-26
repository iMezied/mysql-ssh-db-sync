#!/usr/bin/with-contenv bash
# Raise sshd's concurrency limits for the test container.
#
# The image ships OpenSSH's defaults: MaxStartups 10:30:100 and MaxSessions 10.
# The integration suites open many tunnels at once (deliberately — concurrent
# tunnels are a feature), and past ten *unauthenticated* connections sshd starts
# dropping them at random. That surfaces as a bare "Disconnected" and looks
# exactly like a bug in the tunnel code.
#
# It also disables PerSourcePenalties (on by default since OpenSSH 9.8). That
# feature blocks a source address that produces authentication failures or
# half-finished handshakes, answering later connections with "Not allowed at
# this time." The tunnel suite deliberately generates exactly those events —
# wrong keys, refused host keys, unreachable hosts — so with penalties on it
# blocks itself, and unrelated tests that merely run afterwards fail.
#
# The client-side half of this was a real bug and is fixed separately: sessions
# now send SSH_MSG_DISCONNECT instead of vanishing, so ordinary use no longer
# accrues penalties. See `disconnect_politely` in engine/src/ssh.rs.
#
# These are properties of the test fixture, not of the product: a real bastion
# is configured by whoever runs it, and the app must not be designed around one
# container's defaults.

CONFIG=/config/sshd/sshd_config

if [ -f "$CONFIG" ]; then
    sed -i '/^MaxStartups/d;/^MaxSessions/d;/^PerSourcePenalties/d' "$CONFIG"
    {
        echo "MaxStartups 200:30:400"
        echo "MaxSessions 200"
        echo "PerSourcePenalties no"
    } >> "$CONFIG"
    echo "[dbsync-test] raised sshd limits and disabled PerSourcePenalties"
fi

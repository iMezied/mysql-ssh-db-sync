#!/usr/bin/with-contenv bash
# Raise sshd's concurrency limits for the test container.
#
# The image ships OpenSSH's defaults: MaxStartups 10:30:100 and MaxSessions 10.
# The integration suites open many tunnels at once (deliberately — concurrent
# tunnels are a feature), and past ten *unauthenticated* connections sshd starts
# dropping them at random. That surfaces as a bare "Disconnected" and looks
# exactly like a bug in the tunnel code.
#
# This is a property of the test fixture, not of the product: a real bastion is
# configured by whoever runs it, and the app must not be designed around one
# container's defaults.

CONFIG=/config/sshd/sshd_config

if [ -f "$CONFIG" ]; then
    sed -i '/^MaxStartups/d;/^MaxSessions/d' "$CONFIG"
    {
        echo "MaxStartups 200:30:400"
        echo "MaxSessions 200"
    } >> "$CONFIG"
    echo "[dbsync-test] raised sshd MaxStartups/MaxSessions for concurrent tunnels"
fi

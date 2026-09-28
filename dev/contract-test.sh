#!/usr/bin/env bash
# Phase 3a: isolated D-Bus contract test. Runs the backend on a private session bus with no
# Wayland (so nothing can be grabbed) and the contract client against it.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --quiet
cargo build --quiet --example contract
exec dbus-run-session -- bash -c '
  env -u WAYLAND_DISPLAY ./target/debug/xdg-desktop-portal-layercapture serve \
      --fake-zones 2880x1800 --trust-any-caller --dev-control > target/contract-backend.log 2>&1 &
  backend=$!
  for _ in $(seq 50); do
    busctl --user status org.freedesktop.impl.portal.desktop.layercapture >/dev/null 2>&1 && break
    sleep 0.1
  done
  status=0
  ./target/debug/examples/contract || status=$?
  kill -TERM $backend; wait $backend || true
  echo "--- backend log: target/contract-backend.log"
  exit $status
'

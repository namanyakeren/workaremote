#!/usr/bin/env bash
# Install agent Worka Remote di Linux (X11).
#
#   sudo ./linux-install.sh https://remote.perusahaan.com TOKEN-ENROLL [./worka-agent]
#
# Script menjalankan bagian root hanya untuk menyalin binary; konfigurasi agent
# tetap milik user yang memanggil (lewat sudo, pakai $SUDO_USER).
set -euo pipefail

SERVER="${1:?pakai: linux-install.sh SERVER TOKEN [binary]}"
TOKEN="${2:?pakai: linux-install.sh SERVER TOKEN [binary]}"
BIN="${3:-./worka-agent}"

if [[ $EUID -eq 0 && -n "${SUDO_USER:-}" ]]; then
  TARGET_USER="$SUDO_USER"
else
  TARGET_USER="$(id -un)"
fi

as_user() {
  if [[ $EUID -eq 0 && "$TARGET_USER" != "root" ]]; then
    sudo -u "$TARGET_USER" -- "$@"
  else
    "$@"
  fi
}

install -Dm755 "$BIN" /usr/local/bin/worka-agent
as_user /usr/local/bin/worka-agent enroll --server "$SERVER" --token "$TOKEN"

HOME_DIR="$(getent passwd "$TARGET_USER" | cut -d: -f6)"
UNIT_DIR="$HOME_DIR/.config/systemd/user"
mkdir -p "$UNIT_DIR"
chown -R "$TARGET_USER" "$HOME_DIR/.config"

cat > "$UNIT_DIR/worka-agent.service" <<'EOF'
[Unit]
Description=Worka Remote agent
After=graphical-session.target

[Service]
ExecStart=/usr/local/bin/worka-agent
Restart=always
RestartSec=5
Environment=DISPLAY=:0

[Install]
WantedBy=default.target
EOF
chown "$TARGET_USER" "$UNIT_DIR/worka-agent.service"

as_user systemctl --user daemon-reload || true
as_user systemctl --user enable --now worka-agent || {
  echo "systemd --user tidak tersedia; jalankan manual: worka-agent &" >&2
}

echo
echo "ID mesin ini:"
as_user /usr/local/bin/worka-agent id
echo
echo "Unattended access : worka-agent set-password"
echo "Mode konfirmasi   : worka-agent clear-password"
echo
echo "Prasyarat: sesi X11 (Wayland belum didukung). Cek dengan: echo \$XDG_SESSION_TYPE"

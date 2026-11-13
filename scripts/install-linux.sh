#!/usr/bin/env bash
# Instala Osky para el usuario actual (sin sudo) y lo deja como reproductor
# predeterminado de MP3, FLAC, WAV y AAC/M4A.
#   ./scripts/install-linux.sh            compila en release e instala
#   ./scripts/install-linux.sh --no-build instala el binario ya compilado
set -euo pipefail
cd "$(dirname "$0")/.."

APP_ID="osky-player"
BIN_SRC="src-tauri/target/release/osky-player"
BIN_DIR="$HOME/.local/bin"
APPS_DIR="$HOME/.local/share/applications"
ICON_DIR="$HOME/.local/share/icons/hicolor"
MIMES="audio/mpeg;audio/mp3;audio/x-mp3;audio/flac;audio/x-flac;audio/wav;audio/x-wav;audio/vnd.wave;audio/aac;audio/x-aac;audio/mp4;audio/x-m4a;"

if [[ "${1:-}" != "--no-build" ]]; then
  [[ -f "$HOME/.cargo/env" ]] && source "$HOME/.cargo/env"
  npm run build:bin
fi

install -Dm755 "$BIN_SRC" "$BIN_DIR/$APP_ID"
install -Dm644 src-tauri/icons/32x32.png      "$ICON_DIR/32x32/apps/$APP_ID.png"
install -Dm644 src-tauri/icons/128x128.png    "$ICON_DIR/128x128/apps/$APP_ID.png"
install -Dm644 src-tauri/icons/128x128@2x.png "$ICON_DIR/256x256/apps/$APP_ID.png"
install -Dm644 assets/icon.svg                "$ICON_DIR/scalable/apps/$APP_ID.svg"

mkdir -p "$APPS_DIR"
cat > "$APPS_DIR/$APP_ID.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Osky Audio Player
GenericName=Reproductor de audio para DJs
Comment=Escucha y clasifica tracks a toda velocidad
Exec="$BIN_DIR/$APP_ID" %f
Icon=$APP_ID
Terminal=false
Categories=AudioVideo;Audio;Player;
MimeType=$MIMES
StartupWMClass=osky-player
DESKTOP

update-desktop-database "$APPS_DIR" 2>/dev/null || true
gtk-update-icon-cache -q "$ICON_DIR" 2>/dev/null || true

IFS=';' read -ra list <<< "$MIMES"
for m in "${list[@]}"; do
  [[ -n "$m" ]] && xdg-mime default "$APP_ID.desktop" "$m" 2>/dev/null
done
command -v kbuildsycoca6 >/dev/null && kbuildsycoca6 --noincremental >/dev/null 2>&1 || true

echo "✓ Osky instalado en $BIN_DIR/$APP_ID y configurado como reproductor predeterminado."
echo "  flac -> $(xdg-mime query default audio/flac)"
echo "  mp3  -> $(xdg-mime query default audio/mpeg)"

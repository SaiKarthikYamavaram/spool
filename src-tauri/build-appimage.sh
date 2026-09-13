#!/usr/bin/env bash
# Build the AppImage, working around linuxdeploy breakage on rolling distros (Arch).
set -euo pipefail
cd "$(dirname "$0")/.."

# Compile in an untouched environment so cargo's pkg-config caches stay valid.
npx tauri build --no-bundle "$@"

# linuxdeploy's bundled strip can't read the .relr.dyn sections current toolchains emit.
export NO_STRIP=true

# gdk-pixbuf 2.44+ (glycin) dropped the loader dir its .pc still names; give the gtk plugin an empty one.
pixbuf_dir="$(pkg-config --variable=gdk_pixbuf_binarydir gdk-pixbuf-2.0)"
if [ ! -d "$pixbuf_dir" ]; then
    shim="$(mktemp -d)"
    trap 'rm -rf "$shim"' EXIT
    mkdir -p "$shim/pixbuf/loaders"
    sed "s|^gdk_pixbuf_binarydir=.*|gdk_pixbuf_binarydir=$shim/pixbuf|" \
        "$(pkg-config --variable=pcfiledir gdk-pixbuf-2.0)/gdk-pixbuf-2.0.pc" > "$shim/gdk-pixbuf-2.0.pc"
    export PKG_CONFIG_PATH="$shim${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
fi

npx tauri bundle --bundles appimage "$@"

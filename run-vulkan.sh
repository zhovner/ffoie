#!/bin/bash
# Run ffoie on the locally-built Mesa 26.1 PanVK Vulkan driver.
# Logs everything (env, vulkaninfo, game stdout+stderr) to a timestamped file
# under ./logs/ and a stable `logs/latest.log` symlink.

set -u
cd "$(dirname "$0")"

BIN=./target/release/ffoie
[ -x "$BIN" ] || { echo "binary not found at $BIN (run 'cargo build --release' first)"; exit 1; }

mkdir -p logs
STAMP=$(date +%Y%m%d-%H%M%S)
LOG="logs/vulkan-run-$STAMP.log"
ln -sf "vulkan-run-$STAMP.log" logs/latest.log

# Env that routes Vulkan to our /opt Mesa 26.1 PanVK build.
export VK_DRIVER_FILES=/opt/mesa-26.1/share/vulkan/icd.d/panfrost_icd.aarch64.json
export PAN_I_WANT_A_BROKEN_VULKAN_DRIVER=1

# Force wgpu to pick Vulkan; refuse silent GL fallback by also setting the
# adapter selection to high-power so we don't pick llvmpipe by accident.
export WGPU_BACKEND=vulkan
export WGPU_POWER_PREF=high

# PanVK reports conformanceVersion=0.0.0.0; wgpu hides non-conformant adapters
# unless this flag is set. Without it, wgpu silently falls back to GL.
export WGPU_ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER=1

# wgpu/Rust diagnostics — verbose enough to see backend negotiation + adapter
# picks, quiet enough to not drown the log in render-loop spam.
export RUST_LOG=info,wgpu=info,wgpu_core=info,wgpu_hal=debug,naga=warn
export RUST_BACKTRACE=1

# Loader-level Vulkan debug. 'warn' surfaces ICD-load failures without spam.
export VK_LOADER_DEBUG=warn

# Redirect everything from here on into the log file (tee so terminal still sees it).
exec > >(tee "$LOG") 2>&1

echo "=================================================================="
echo " ffoie Vulkan run @ $(date)"
echo " log:  $LOG"
echo "=================================================================="
echo ""

echo "----- env (filtered) -----"
env | grep -E 'VK_|WGPU_|PAN_|LIBGL_|EGL_VENDOR|RUST_|MESA_' | sort
echo ""

echo "----- pre-flight: vulkaninfo --summary -----"
vulkaninfo --summary 2>&1 | sed -n '/^Devices:/,/^$/p'
echo ""

echo "----- panvk ICD JSON sanity -----"
[ -r "$VK_DRIVER_FILES" ] && cat "$VK_DRIVER_FILES" || echo "ICD JSON NOT READABLE: $VK_DRIVER_FILES"
echo ""

echo "----- adapter selection hint (lsof on the game after spawn) -----"
echo "(use:  lsof -p \$(pgrep -x ffoie) | grep -E 'mesa|vulkan|dri/'  during runtime)"
echo ""

echo "----- launching $BIN -----"
echo ""
exec "$BIN" "$@"

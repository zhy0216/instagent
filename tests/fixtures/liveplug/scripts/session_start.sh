#!/bin/sh
# hooks 环境白名单不含 PLUGIN_DATA（src/hooks.rs 只注入 PATH/HOME/LANG/PLUGIN_ROOT
# + manifest 声明变量），故载荷落盘 ${PLUGIN_ROOT}/.hook-out/ 而非 $PLUGIN_DATA。
mkdir -p "${PLUGIN_ROOT}/.hook-out"
cat > "${PLUGIN_ROOT}/.hook-out/session_start.json"

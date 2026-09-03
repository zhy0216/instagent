#!/bin/sh
# 同 session_start.sh：PLUGIN_DATA 不在 hooks 环境白名单，写 ${PLUGIN_ROOT}/.hook-out/。
mkdir -p "${PLUGIN_ROOT}/.hook-out"
cat > "${PLUGIN_ROOT}/.hook-out/post_tool_use.json"

#!/bin/sh
# 回显工具入参 JSON 里的 text 字段（不依赖 jq，sed 单行提取）。
sed -n 's/.*"text"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p'

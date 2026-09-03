#!/bin/sh
# PreToolUse guard：stdin 载荷含 forbidden_marker 则 exit 2 阻止（stderr 给理由）。
if grep -q forbidden_marker; then
  echo "blocked by liveplug guard: forbidden_marker in tool input" >&2
  exit 2
fi
exit 0

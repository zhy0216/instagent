#!/usr/bin/env python3
"""goose 声明式 provider JSON -> bundled/dev.instagent/providers/*.json（第三版 §2.4）。

用法：python3 scripts/convert_providers.py OUT_DIR NAME [NAME...]
读 ~/yyds/goose/crates/goose-providers/src/declarative/definitions/*.json（45 个，
commit 4ad43df 只读参考），按 `name`（含去掉 `custom_` 前缀后的名字）挑选，
转换规则：base_url 去掉 `/chat/completions` 后缀（我们写到 `/v1`，引擎自己拼）、
删除 setup / setup_steps 向导段与本内核不用的字段、`custom_` 前缀剥掉、
engine=anthropic 直接报错退出（instagent 不支持 Anthropic，见 docs/adr/0001）。
产物写入 OUT_DIR/<name>.json（产物是提交物，脚本只是生成器）。
"""
import json
import os
import sys

SRC = os.path.expanduser(
    "~/yyds/goose/crates/goose-providers/src/declarative/definitions"
)
KEEP = ["name", "engine", "display_name", "description", "api_key_env",
        "base_url", "headers", "timeout_seconds", "models"]


def convert(d):
    if d.get("base_url", "").endswith("/chat/completions"):
        d["base_url"] = d["base_url"][: -len("/chat/completions")]
    if d.get("engine") == "anthropic":
        sys.exit(f"provider `{d.get('name')}` engine=anthropic，"
                 "instagent 不支持 Anthropic（docs/adr/0001-no-anthropic-support.md）")
    d["name"] = d["name"].removeprefix("custom_")
    d.setdefault("headers", {})
    d["models"] = [
        {k: m[k] for k in ("name", "context_limit", "max_tokens") if m.get(k) is not None}
        for m in d.get("models", [])
    ]
    return {k: d[k] for k in KEEP if d.get(k) is not None}


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    out_dir, wanted = sys.argv[1], set(sys.argv[2:])
    definitions = {}
    for path in sorted(os.listdir(SRC)):
        with open(os.path.join(SRC, path)) as f:
            d = json.load(f)
        definitions[d["name"]] = d
        definitions[d["name"].removeprefix("custom_")] = d
    missing = wanted - set(definitions)
    if missing:
        sys.exit(f"goose 声明式定义里没有这些 provider（改为手写）：{sorted(missing)}")
    os.makedirs(out_dir, exist_ok=True)
    for name in sorted(wanted):
        d = convert(definitions[name])
        target = os.path.join(out_dir, f"{d['name']}.json")
        with open(target, "w") as f:
            f.write(json.dumps(d, indent=2, ensure_ascii=False) + "\n")
        print(target)


if __name__ == "__main__":
    main()

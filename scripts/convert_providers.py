#!/usr/bin/env python3
"""goose 声明式 provider JSON -> bundled/dev.instagent/providers/*.json（第三版 §2.4；E5/S9）。

用法：
  python3 scripts/convert_providers.py --source DIR OUT_DIR NAME [NAME...]
  python3 scripts/convert_providers.py --fixture  OUT_DIR NAME [NAME...]

`--source DIR` 指向 goose `declarative/definitions/` 目录（不再硬编码
~/yyds/goose，CI 可直接 `--source tests/fixtures/provider_converter/definitions`）；
`--fixture` 是等价的便捷写法，使用仓库内最小 fixture。

转换规则：base_url 去掉 `/chat/completions` 后缀（我们写到 `/v1`，引擎自己拼）、
删除 setup / setup_steps 向导段与 `08` 的 ProviderDef/ModelDef 契约之外的字段
（计费、request_params、dynamic_models 等运行时不用的都剔除；display_name、
description、model max_tokens 是契约字段，保留）、`custom_` 前缀剥掉、
engine=anthropic 直接报错退出（instagent 不支持 Anthropic，见 docs/adr/0001）。
产物写入 OUT_DIR/<name>.json 后做 schema + round-trip 校验：回读文件、按
[`ProviderDef::validate`]（src/provider/mod.rs）同款规则检查，坏 source /
坏 schema 一律非零退出并指出文件与字段。产物是提交物，脚本只是生成器。
"""
import argparse
import json
import os
import sys

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FIXTURE_SRC = os.path.join(
    REPO_ROOT, "tests", "fixtures", "provider_converter", "definitions"
)
# src/provider/mod.rs ProviderDef 的字段契约（todo 08）；其余源字段全部剔除。
KEEP = ["name", "engine", "display_name", "description", "api_key_env",
        "base_url", "headers", "timeout_seconds", "models"]
# ModelDef 的字段契约。
MODEL_KEEP = ["name", "context_limit", "max_tokens"]


def fail(msg):
    sys.exit(f"convert_providers: {msg}")


def is_nonempty_str(v):
    return isinstance(v, str) and v.strip() != ""


def is_pos_int(v):
    return isinstance(v, int) and not isinstance(v, bool) and v >= 1


def convert(d, src_path):
    """goose 声明式定义 -> ProviderDef 契约形状（行为与旧版逐字段一致）。"""
    if not isinstance(d, dict):
        fail(f"{src_path}: provider 定义不是 JSON 对象")
    if "name" not in d or "engine" not in d:
        fail(f"{src_path}: 缺少 name/engine 字段")
    engine = d.get("engine")
    if engine == "anthropic":
        fail(f"provider `{d['name']}` engine=anthropic，"
             "instagent 不支持 Anthropic（docs/adr/0001-no-anthropic-support.md）")
    if engine != "openai":
        fail(f"{src_path}: 未知 engine `{engine}`（goose 源只有 openai/anthropic）")
    if d.get("base_url", "").endswith("/chat/completions"):
        d["base_url"] = d["base_url"][: -len("/chat/completions")]
    d["name"] = d["name"].removeprefix("custom_")
    d.setdefault("headers", {})
    if not isinstance(d.get("models", []), list):
        fail(f"{src_path}: models 不是数组")
    d["models"] = [
        {k: m[k] for k in MODEL_KEEP if m.get(k) is not None}
        for m in d.get("models", [])
    ]
    return {k: d[k] for k in KEEP if d.get(k) is not None}


def check(defn):
    """按 ProviderDef::validate（src/provider/mod.rs:385）同款规则校验产物。

    返回错误信息列表；空列表 = 通过。产物只可能是 engine=openai（proxy provider
    是手写提交物，不走本脚本）。
    """
    errs = []
    name = defn.get("name")
    if not is_nonempty_str(name) or "/" in name:
        fail(f"provider name `{name}` is empty or contains `/`")
    p = name
    extra = set(defn) - set(KEEP)
    if extra:
        errs.append(f"provider `{p}`: 契约外字段 {sorted(extra)}")
    for f in ("display_name", "description", "api_key_env"):
        v = defn.get(f)
        if v is not None and not is_nonempty_str(v):
            errs.append(f"provider `{p}`: field `{f}` must be non-empty when present")
    if defn.get("engine") not in ("openai", "proxy"):
        errs.append(f"provider `{p}`: field `engine` must be openai|proxy "
                    f"(got {defn.get('engine')!r})")
    base_url = defn.get("base_url")
    if base_url is not None and not is_nonempty_str(base_url):
        errs.append(f"provider `{p}`: field `base_url` must be non-empty when present")
    if defn.get("engine") == "openai" and not is_nonempty_str(base_url):
        errs.append(f"provider `{p}` (engine openai) is missing base_url")
    headers = defn.get("headers")
    if headers is not None:
        if not isinstance(headers, dict):
            errs.append(f"provider `{p}`: headers must be an object")
        else:
            for k, v in headers.items():
                if not is_nonempty_str(k):
                    errs.append(f"provider `{p}`: header name must be non-empty")
                if not is_nonempty_str(v):
                    errs.append(f"provider `{p}`: header `{k}` must have a "
                                "non-empty value")
    timeout = defn.get("timeout_seconds")
    if timeout is not None and not is_pos_int(timeout):
        errs.append(f"provider `{p}`: field `timeout_seconds` must be >= 1")
    models = defn.get("models", [])
    if not isinstance(models, list):
        errs.append(f"provider `{p}`: models must be an array")
        return errs
    seen = set()
    for m in models:
        if not isinstance(m, dict):
            errs.append(f"provider `{p}`: model entry must be an object")
            continue
        extra = set(m) - set(MODEL_KEEP)
        if extra:
            errs.append(f"provider `{p}`: model `{m.get('name')}` "
                        f"契约外字段 {sorted(extra)}")
        mn = m.get("name")
        if not is_nonempty_str(mn):
            errs.append(f"provider `{p}`: model name must be non-empty")
            continue
        if mn in seen:
            errs.append(f"provider `{p}`: model `{mn}` is declared twice")
        seen.add(mn)
        if m.get("context_limit") is not None and not is_pos_int(m["context_limit"]):
            errs.append(f"provider `{p}`: model `{mn}`: field `context_limit` "
                        "must be >= 1")
        if m.get("max_tokens") is not None and not is_pos_int(m["max_tokens"]):
            errs.append(f"provider `{p}`: model `{mn}`: field `max_tokens` "
                        "must be >= 1")
    return errs


def load_definitions(src_dir):
    definitions = {}
    for path in sorted(os.listdir(src_dir)):
        if not path.endswith(".json"):
            continue
        fp = os.path.join(src_dir, path)
        try:
            with open(fp) as f:
                d = json.load(f)
        except (OSError, ValueError) as e:
            fail(f"读取 {fp} 失败：{e}")
        if not isinstance(d, dict) or not is_nonempty_str(d.get("name")):
            fail(f"{fp}: 缺少非空 name 字段")
        if d["name"] in definitions:
            fail(f"provider `{d['name']}` 重复定义：{fp} 与 {definitions[d['name']][1]}")
        definitions[d["name"]] = (d, fp)
        definitions[d["name"].removeprefix("custom_")] = (d, fp)
    return definitions


def write_and_verify(defn, src_path, out_dir):
    out = convert(json.loads(json.dumps(defn)), src_path)
    errs = check(out)
    if errs:
        fail(f"{src_path}: 生成结果不符合 ProviderDef 契约：\n" + "\n".join(errs))
    target = os.path.join(out_dir, f"{out['name']}.json")
    with open(target, "w") as f:
        f.write(json.dumps(out, indent=2, ensure_ascii=False) + "\n")
    with open(target) as f:
        reloaded = json.load(f)
    if reloaded != out:
        fail(f"{target}: round-trip 校验失败（回读与生成不一致）")
    if check(reloaded):
        fail(f"{target}: 回读后契约校验失败")
    for f_ in ("display_name", "description", "api_key_env", "timeout_seconds"):
        if defn.get(f_) is not None and reloaded.get(f_) != defn[f_]:
            fail(f"{target}: 约定字段 `{f_}` 未原样保留")
    print(target)


def main():
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("--source", metavar="DIR",
                    help="goose declarative definitions 目录（~ 会展开）")
    ap.add_argument("--fixture", action="store_true",
                    help=f"使用仓库最小 fixture（等价 --source {FIXTURE_SRC}）")
    ap.add_argument("out_dir", metavar="OUT_DIR")
    ap.add_argument("names", nargs="+", metavar="NAME")
    a = ap.parse_args()
    if bool(a.source) == bool(a.fixture):
        ap.error("--source 与 --fixture 必须二选一")
    src_dir = os.path.expanduser(a.source) if a.source else FIXTURE_SRC
    if not os.path.isdir(src_dir):
        fail(f"source 目录不存在：{src_dir}")
    definitions = load_definitions(src_dir)
    missing = set(a.names) - set(definitions)
    if missing:
        fail(f"声明式定义里没有这些 provider（改为手写）：{sorted(missing)}")
    os.makedirs(a.out_dir, exist_ok=True)
    for name in sorted(set(a.names)):
        d, fp = definitions[name]
        write_and_verify(d, fp, a.out_dir)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""convert_providers.py 的 fixture/unit 测试（E5/S9）。

运行：python3 tests/test_convert_providers.py（或 pytest tests/test_convert_providers.py）
不依赖 ~/yyds/goose：全部用仓库 fixture 与临时目录合成的 source。
"""
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCRIPT = os.path.join(REPO_ROOT, "scripts", "convert_providers.py")
FIXTURE_SRC = os.path.join(
    REPO_ROOT, "tests", "fixtures", "provider_converter", "definitions"
)


def run(*args):
    return subprocess.run(
        [sys.executable, SCRIPT, *args], capture_output=True, text=True
    )


def write_source(tmp, files):
    src = os.path.join(tmp, "definitions")
    os.makedirs(src, exist_ok=True)
    for name, content in files.items():
        with open(os.path.join(src, name), "w") as f:
            if isinstance(content, str):
                f.write(content)
            else:
                json.dump(content, f)
    return src


class ConverterTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.out = os.path.join(self.tmp, "out")

    def convert(self, *args):
        r = run(*args, self.out, "custom_acme", "beta")
        self.assertEqual(r.returncode, 0, r.stderr)
        return r

    def load(self, name):
        with open(os.path.join(self.out, f"{name}.json")) as f:
            return json.load(f)

    def test_fixture_mode_generates_bundled_shape(self):
        r = self.convert("--fixture")
        self.assertEqual(
            r.stdout.split(),
            [os.path.join(self.out, n) for n in ("beta.json", "acme.json")],
        )
        acme = self.load("acme")
        # 约定字段保留（08 契约），base_url 剥后缀，custom_ 前缀剥掉。
        self.assertEqual(acme["name"], "acme")
        self.assertEqual(acme["display_name"], "Acme AI")
        self.assertEqual(acme["description"], "Acme hosted OpenAI-compatible endpoint")
        self.assertEqual(acme["api_key_env"], "ACME_API_KEY")
        self.assertEqual(acme["base_url"], "https://api.acme.test/v1")
        self.assertEqual(acme["timeout_seconds"], 60)
        self.assertEqual(acme["models"][0]["max_tokens"], 8192)
        self.assertNotIn("max_tokens", acme["models"][1])
        # 运行时不用的字段被剔除。
        for gone in ("setup", "setup_steps", "env_vars", "dynamic_models",
                     "requires_auth", "preserves_thinking", "supports_streaming",
                     "model_doc_link"):
            self.assertNotIn(gone, acme)
        self.assertNotIn("request_params", acme["models"][0])
        self.assertNotIn("input_token_cost", acme["models"][0])
        # headers 为 null 时整字段缺省（与旧行为一致，运行时段默认空 map）。
        self.assertNotIn("headers", self.load("beta"))

    def test_output_passes_runtime_contract_check(self):
        spec = importlib.util.spec_from_file_location("cp", SCRIPT)
        cp = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cp)
        self.convert("--fixture")
        for name in ("acme", "beta"):
            self.assertEqual(cp.check(self.load(name)), [], name)

    def test_explicit_source_flag_matches_fixture(self):
        self.convert("--fixture")
        other = os.path.join(self.tmp, "out2")
        r = run("--source", FIXTURE_SRC, other, "custom_acme", "beta")
        self.assertEqual(r.returncode, 0, r.stderr)
        for name in ("acme.json", "beta.json"):
            with open(os.path.join(self.out, name)) as a, \
                    open(os.path.join(other, name)) as b:
                self.assertEqual(a.read(), b.read(), name)

    def test_repeatable_runs(self):
        self.convert("--fixture")
        first = open(os.path.join(self.out, "acme.json")).read()
        self.convert("--fixture")
        self.assertEqual(open(os.path.join(self.out, "acme.json")).read(), first)

    def test_missing_source_dir_is_diagnosable(self):
        r = run("--source", os.path.join(self.tmp, "nope"), self.out, "acme")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("source 目录不存在", r.stderr)

    def test_corrupt_json_names_the_file(self):
        src = write_source(self.tmp, {"broken.json": "{not json"})
        r = run("--source", src, self.out, "acme")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("broken.json", r.stderr)

    def test_duplicate_definition_names_rejected(self):
        dup = {"name": "custom_acme", "engine": "openai",
               "base_url": "https://a.test/v1"}
        src = write_source(self.tmp, {
            "custom_acme.json": json.load(open(os.path.join(FIXTURE_SRC, "custom_acme.json"))),
            "acme_copy.json": dup,
        })
        r = run("--source", src, self.out, "acme")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("重复定义", r.stderr)

    def test_missing_requested_name_is_diagnosable(self):
        r = run("--fixture", self.out, "not-there")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("not-there", r.stderr)

    def test_anthropic_engine_rejected(self):
        src = write_source(self.tmp, {
            "x.json": {"name": "x", "engine": "anthropic", "base_url": "https://x.test"}
        })
        r = run("--source", src, self.out, "x")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("anthropic", r.stderr)

    def test_bad_schema_rejected_with_field_names(self):
        src = write_source(self.tmp, {
            "bad.json": {
                "name": "bad", "engine": "openai", "base_url": "https://b.test/v1",
                "display_name": "   ", "timeout_seconds": 0,
                "models": [{"name": "m", "context_limit": 0},
                           {"name": "m", "context_limit": None}],
            }
        })
        r = run("--source", src, self.out, "bad")
        self.assertNotEqual(r.returncode, 0)
        for needle in ("display_name", "timeout_seconds", "context_limit",
                       "declared twice"):
            self.assertIn(needle, r.stderr)

    def test_source_and_fixture_are_exclusive(self):
        r = run("--fixture", "--source", FIXTURE_SRC, self.out, "acme")
        self.assertNotEqual(r.returncode, 0)
        r = run(self.out, "acme")
        self.assertNotEqual(r.returncode, 0)


if __name__ == "__main__":
    unittest.main()

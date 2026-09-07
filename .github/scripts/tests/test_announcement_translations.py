import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

spec = importlib.util.spec_from_file_location(
    "validator", Path(__file__).resolve().parents[1] / "validate-announcement-translations.py")
validator = importlib.util.module_from_spec(spec)
spec.loader.exec_module(validator)


class AnnouncementCatalogTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.folder = self.root / validator.PREFIX
        (self.folder / "catalogs").mkdir(parents=True)
        self.write_catalog(1)

    def write_catalog(self, number, entries=None):
        value = {"schema_version": 1, "version": number, "locale": "zh-CN",
                 "entries": entries if entries is not None else [
                     {"field": "title", "source": "From the team", "translation": "团队寄语"}]}
        raw = (json.dumps(value, ensure_ascii=False, indent=2) + "\n").encode()
        (self.folder / "catalogs" / f"{number}.json").write_bytes(raw)
        self.write_manifest(number, hashlib.sha256(raw).hexdigest())
        return raw

    def write_manifest(self, number, digest):
        (self.folder / "manifest.json").write_bytes(json.dumps(
            {"schema_version": 1, "version": number, "sha256": digest}).encode())

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.root), *args],
                                       stderr=subprocess.PIPE).decode().strip()

    def baseline(self):
        self.git("init", "--quiet")
        self.git("-c", "core.autocrlf=false", "add", "community")
        self.git("-c", "user.name=Catalog Test", "-c", "user.email=test@example.invalid",
                 "-c", "core.hooksPath=/dev/null", "-c", "commit.gpgSign=false",
                 "commit", "--quiet", "-m", "catalog baseline")
        return self.git("rev-parse", "HEAD")

    def test_current_catalog_and_empty_replacement(self):
        self.assertEqual(validator.validate(self.root)["version"], 1)
        base = self.baseline()
        self.write_catalog(2, [])
        self.assertEqual(validator.validate(self.root, base)["version"], 2)

    def test_published_catalog_cannot_be_rewritten_even_with_matching_hash(self):
        base = self.baseline()
        self.write_catalog(1, [])
        with self.assertRaisesRegex(ValueError, "immutable"):
            validator.validate(self.root, base)

    def test_published_catalog_cannot_be_deleted(self):
        self.write_catalog(2)
        base = self.baseline()
        self.write_catalog(3)
        (self.folder / "catalogs/2.json").unlink()
        with self.assertRaisesRegex(ValueError, "immutable"):
            validator.validate(self.root, base)

    def test_digest_mismatch_and_old_manifest_are_rejected(self):
        self.write_manifest(1, "0" * 64)
        with self.assertRaisesRegex(ValueError, "digest mismatch"):
            validator.validate(self.root)
        self.write_catalog(2)
        self.write_catalog(1)
        with self.assertRaisesRegex(ValueError, "highest catalog"):
            validator.validate(self.root)

    def test_contract_rejects_duplicates_links_controls_and_bad_versions(self):
        entry = {"field": "title", "source": "Hello", "translation": "你好"}
        for entries in ([entry, entry], [{**entry, "field": "url"}],
                        [{**entry, "translation": "\x1b[2J"}], [{**entry, "source": ""}]):
            raw = self.write_catalog(1, entries)
            with self.assertRaises(ValueError):
                validator.catalog(raw, 1)
        for number in (True, 0, -1, 1.0, 2**64):
            with self.assertRaises(ValueError):
                validator.version(number)

    def test_newlines_are_message_only_and_fields_are_exact(self):
        entry = {"field": "message", "source": "a\nb", "translation": "甲\n乙"}
        validator.catalog(self.write_catalog(1, [entry]), 1)
        with self.assertRaises(ValueError):
            validator.catalog(self.write_catalog(1, [{**entry, "field": "title"}]), 1)
        raw = self.write_catalog(1)
        for invalid in (raw.replace(b'"locale":', b'"extra": 1, "locale":'),
                        raw.replace(b'"version": 1', b'"version": 1, "version": 1'),
                        raw + b" " * validator.MAX_CATALOG_BYTES, raw.replace(b"\n", b"\r\n")):
            with self.assertRaises(ValueError):
                validator.catalog(invalid, 1)


if __name__ == "__main__":
    unittest.main()

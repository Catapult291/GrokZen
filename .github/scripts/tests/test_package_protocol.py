import hashlib
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]
sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("protocol", SCRIPTS / "write-package-protocol.py")
protocol = importlib.util.module_from_spec(spec)
spec.loader.exec_module(protocol)


class PackageProtocolTests(unittest.TestCase):
    def test_all_platforms_keep_the_same_package_members(self):
        for target, (executable, installer) in protocol.PLATFORMS.items():
            with self.subTest(target=target), tempfile.TemporaryDirectory() as folder:
                package = Path(folder)
                for name in (executable, installer, "BUILD-INFO.txt"):
                    (package / name).write_text("Version: 1.0.16\n", encoding="utf-8")
                names = sorted(path.name for path in package.iterdir())
                protocol.append_protocol(package, "1.0.16", target)
                self.assertEqual(names, sorted(path.name for path in package.iterdir()))
                text = (package / "BUILD-INFO.txt").read_text(encoding="utf-8")
                self.assertTrue(text.startswith("Version: 1.0.16\n"))
                block = json.loads(text.split(protocol.BEGIN)[1].split(protocol.END)[0])
                self.assertEqual(block["platform"], target)
                self.assertEqual(block["executable"], executable)
                self.assertEqual(block["installer"], installer)
                # The old SHA256SUMS grammar hashes the extended metadata normally.
                lines = [f"{hashlib.sha256((package / name).read_bytes()).hexdigest()}  {name}" for name in names]
                self.assertEqual(len(lines), len(names))
                for line in lines:
                    digest, name = line.split("  ")
                    self.assertEqual(digest, hashlib.sha256((package / name).read_bytes()).hexdigest())
                with self.assertRaises(ValueError):
                    protocol.append_protocol(package, "1.0.16", target)

    def test_missing_and_mismatched_version_fail_before_writing(self):
        for text in ("no version\n", "Version: 1.0.13\n"):
            with tempfile.TemporaryDirectory() as folder:
                package = Path(folder)
                for name in ("grok-zh.exe", "Install-GrokZh.ps1", "BUILD-INFO.txt"):
                    (package / name).write_text(text, encoding="utf-8")
                with self.assertRaises(ValueError):
                    protocol.append_protocol(package, "1.0.16", "x86_64-pc-windows-gnu")
                self.assertEqual((package / "BUILD-INFO.txt").read_text(), text)


if __name__ == "__main__":
    unittest.main()

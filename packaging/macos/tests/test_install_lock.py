"""Exercise the installer's actual lock function on a native macOS runner."""

import fcntl
import os
from pathlib import Path
import selectors
import subprocess
import tempfile
import unittest


INSTALLER = Path(__file__).resolve().parents[1] / "Install-GrokZh.sh"
SOURCE = INSTALLER.read_text(encoding="utf-8")
PREFIX, SEPARATOR, _ = SOURCE.partition("\nusage() {")
assert SEPARATOR and "acquire_install_lock() {" in PREFIX
LOCK_SCRIPT = PREFIX + '\numask 077\nBIN_DIR=$1\nacquire_install_lock\n'
ENVIRONMENT = {"PATH": "/usr/bin:/bin:/usr/sbin:/sbin"}


class InstallLockTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="grok-zh-install-lock-")
        self.addCleanup(self.temporary.cleanup)
        self.bin_dir = Path(self.temporary.name) / "bin"
        self.bin_dir.mkdir(mode=0o700)
        self.lock_path = self.bin_dir / ".grok-zh-install.lock"

    def run_lock(self):
        return subprocess.run(
            ["/bin/sh", "-c", LOCK_SCRIPT, "lock-test", str(self.bin_dir)],
            capture_output=True,
            text=True,
            timeout=10,
            env=ENVIRONMENT,
        )

    def start_holder(self):
        process = subprocess.Popen(
            ["/bin/sh", "-c", LOCK_SCRIPT + "printf 'ready\\n'\nIFS= read -r release\n",
             "lock-test", str(self.bin_dir)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=ENVIRONMENT,
        )

        def stop():
            if process.poll() is None:
                process.kill()
            process.communicate(timeout=5)

        self.addCleanup(stop)
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            self.assertTrue(selector.select(timeout=10), "installer did not acquire its lock")
        self.assertEqual(process.stdout.readline(), b"ready\n")
        return process

    def test_lock_outlives_perl_child_and_blocks_native_flock(self):
        holder = self.start_holder()
        self.assertEqual(self.lock_path.stat().st_mode & 0o7777, 0o600)
        with self.lock_path.open("r+") as lock:
            with self.assertRaises(BlockingIOError):
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        self.assertNotEqual(self.run_lock().returncode, 0)
        holder.communicate(input=b"release\n", timeout=5)
        self.assertEqual(holder.returncode, 0)
        self.assertEqual(self.run_lock().returncode, 0)

    def test_native_updater_lock_blocks_installer(self):
        descriptor = os.open(self.lock_path, os.O_RDWR | os.O_CREAT, 0o600)
        with os.fdopen(descriptor, "r+") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.assertNotEqual(self.run_lock().returncode, 0)
        self.assertEqual(self.run_lock().returncode, 0)

    def test_killed_installer_releases_lock_without_deleting_file(self):
        holder = self.start_holder()
        identity = self.lock_path.stat().st_ino
        holder.kill()
        holder.communicate(timeout=5)
        self.assertEqual(self.lock_path.stat().st_ino, identity)
        self.assertEqual(self.run_lock().returncode, 0)

    def test_symlink_is_rejected_without_touching_target(self):
        target = self.bin_dir / "keep.txt"
        target.write_text("keep", encoding="utf-8")
        self.lock_path.symlink_to(target)
        self.assertNotEqual(self.run_lock().returncode, 0)
        self.assertEqual(target.read_text(encoding="utf-8"), "keep")
        self.assertTrue(self.lock_path.is_symlink())

    def test_insecure_existing_lock_is_rejected(self):
        self.lock_path.write_text("keep", encoding="utf-8")
        self.lock_path.chmod(0o644)
        self.assertNotEqual(self.run_lock().returncode, 0)
        self.assertEqual(self.lock_path.read_text(encoding="utf-8"), "keep")


if __name__ == "__main__":
    unittest.main()

import plistlib
from pathlib import Path, PurePosixPath
import tempfile
import unittest
from unittest.mock import patch

import service_rust as service


class RustServiceTests(unittest.TestCase):
    def test_update_preserves_runtime_configuration_and_does_not_restart(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            runtime = root / "runtime"
            runtime.mkdir()
            (runtime / "config.yaml").write_text("existing private settings")
            binary = root / "compiled"
            binary.write_bytes(b"new executable")
            config = root / "config.yaml"
            config.write_text("different settings")
            service.stage(runtime, binary, config, "linux")
            self.assertEqual((runtime / "config.yaml").read_text(), "existing private settings")
            self.assertEqual((runtime / "coport").read_bytes(), b"new executable")

    def test_registration_escapes_paths_and_uses_native_binary(self):
        path = PurePosixPath('/home/test user/100% "quoted"/proxy')
        unit = service.systemd_unit(path)
        self.assertIn('100%% \\"quoted\\"', unit)
        self.assertIn('WorkingDirectory=/home/test user/100%% "quoted"/proxy/\n', unit)
        self.assertIn('EnvironmentFile=-/home/test user/100%% "quoted"/proxy/service.env\n', unit)
        self.assertIn("Restart=on-failure", unit)
        self.assertNotIn("python", unit)
        plist = plistlib.loads(service.launchd_plist(path))
        self.assertEqual(plist["ProgramArguments"], [str(path / "coport"), "--config", str(path / "config.yaml")])
        self.assertEqual(plist["Umask"], 0o077)
        script = service.windows_script("install", Path("C:/Users/O'Brien/Proxy App"))
        self.assertIn("O''Brien", script)
        self.assertIn("-LogonType Interactive -RunLevel Limited", script)
        self.assertIn("coport.exe", script)
        self.assertIn("[TimeSpan]::Zero", script)

    def test_uninstall_removes_registered_but_unloaded_linux_unit(self):
        with tempfile.TemporaryDirectory() as temp:
            home = Path(temp)
            unit = home / ".config/systemd/user" / f"{service.LABEL}.service"
            unit.parent.mkdir(parents=True)
            unit.write_text("invalid unit")
            def run(command, **kwargs):
                return service.subprocess.CompletedProcess(command, 5 if "stop" in command else 0)
            with patch.object(service.sys, "platform", "linux"), patch.object(Path, "home", return_value=home), \
                 patch.dict(service.os.environ, {"XDG_CONFIG_HOME": str(home / ".config")}), \
                 patch.object(service, "runtime_dir", return_value=home / "runtime"), \
                 patch.object(service.subprocess, "run", side_effect=run):
                self.assertEqual(service.manage("uninstall", home / "binary", home / "config"), 0)
                self.assertFalse(unit.exists())

    def test_linux_update_does_not_call_systemctl(self):
        with tempfile.TemporaryDirectory() as temp:
            home = Path(temp)
            unit = home / ".config/systemd/user" / f"{service.LABEL}.service"
            unit.parent.mkdir(parents=True)
            unit.write_text("installed")
            with patch.object(service.sys, "platform", "linux"), patch.object(Path, "home", return_value=home), \
                 patch.dict(service.os.environ, {"XDG_CONFIG_HOME": str(home / ".config")}), \
                 patch.object(service, "runtime_dir", return_value=home / "runtime"), \
                 patch.object(service, "stage") as stage, patch.object(service.subprocess, "run") as run:
                self.assertEqual(service.manage("update", home / "binary", home / "config"), 0)
                stage.assert_called_once()
                run.assert_not_called()


if __name__ == "__main__":
    unittest.main()

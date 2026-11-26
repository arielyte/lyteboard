#!/usr/bin/env python3
"""Exercise the installer with real release binaries and isolated user paths."""

import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / 'scripts/install-linux.py'
spec = importlib.util.spec_from_file_location('install_linux', SCRIPT)
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)


class InstallationTests(unittest.TestCase):
    def test_service_restart_checks_profile_and_executable(self):
        installation = installer.Installation()
        status = subprocess.CompletedProcess([], 0, 'Daemon running (PID 123, version 0.1.0)\n', '')
        with patch.object(installation, 'service_active', return_value=True), \
                patch.object(installer.subprocess, 'run', return_value=status), \
                patch.object(installer, 'run') as command, \
                patch.object(installer, 'wait_exit') as wait:
            with patch.object(installer.subprocess, 'check_output', side_effect=['123\n', f'{{ path={installation.links}/lyteboardd ; }}']):
                self.assertEqual(installation.stop(Path('/test/cli'), False), (True, True))
                command.assert_called_once_with(['systemctl', '--user', 'stop', 'lyteboardd.service'])
                wait.assert_called_once_with(123)
            command.reset_mock()
            with patch.object(installer.subprocess, 'check_output', return_value='456\n'):
                with self.assertRaisesRegex(RuntimeError, 'different runtime profile'):
                    installation.stop(Path('/test/cli'), False)
            with patch.object(installer.subprocess, 'check_output', side_effect=['123\n', '{ path=/old/daemon ; }']):
                with self.assertRaisesRegex(RuntimeError, 'custom executable'):
                    installation.stop(Path('/test/cli'), False)
            command.assert_not_called()

    def test_desktop_escaping(self):
        self.assertEqual(installer.desktop_exec(Path('/tmp/a b/%app')), '"/tmp/a b/%%app"')
        self.assertIn('\\\\$', installer.desktop_exec(Path('/tmp/$app')))
        with self.assertRaises(RuntimeError):
            installer.desktop_exec(Path('/tmp/line\nbreak'))

    def test_install_upgrade_uninstall(self):
        with tempfile.TemporaryDirectory(prefix='lyteboard-install-') as temporary:
            home = Path(temporary)
            runtime = home / 'run'
            runtime.mkdir(mode=0o700)
            env = dict(os.environ, HOME=str(home), XDG_RUNTIME_DIR=str(runtime),
                       XDG_DATA_HOME=str(home / 'data'), XDG_CONFIG_HOME=str(home / 'config'))
            env.pop('LYTEBOARD_PROFILE', None)
            # Do not inspect or change the login session's optional service or caches.
            helpers = home / 'helpers'
            helpers.mkdir()
            for name, result in (('systemctl', 1), ('kbuildsycoca6', 0), ('update-desktop-database', 0)):
                path = helpers / name
                path.write_text(f'#!/bin/sh\nexit {result}\n')
                path.chmod(0o700)
            env['PATH'] = str(helpers) + ':' + env['PATH']
            def command(*args):
                return subprocess.run(args, env=env, capture_output=True, text=True, check=True)
            def install(*args):
                return command('python3', str(SCRIPT), 'install', '--skip-build', *args)
            cli = home / '.local/bin/lyteboard'
            try:
                install()
                preferences = home / 'config/lyteboard/desktop.json'
                self.assertTrue(json.loads(preferences.read_text())['startDaemon'])
                self.assertFalse((home / f'config/autostart/{installer.APP}.desktop').exists())
                command('desktop-file-validate', str(home / f'data/applications/{installer.APP}.desktop'))
                self.assertEqual(cli.resolve(), home / '.local/lib/lyteboard/bin/lyteboard')
                command(str(cli), 'daemon', 'start')
                before = command(str(cli), 'status').stdout
                pid = int(re.search(r'PID (\d+),', before)[1])
                command(str(cli), 'daemon', 'start')
                self.assertIn(f'PID {pid},', command(str(cli), 'status').stdout)
                other_runtime = home / 'other-run'
                other_runtime.mkdir(mode=0o700)
                other_env = dict(env, XDG_RUNTIME_DIR=str(other_runtime))
                subprocess.run([str(cli), 'daemon', 'start'], env=other_env, check=True, capture_output=True)
                try:
                    blocked = subprocess.run(['python3', str(SCRIPT), 'install', '--skip-build'], env=env, capture_output=True)
                    self.assertNotEqual(blocked.returncode, 0)
                    self.assertIn(b'Another runtime profile', blocked.stderr)
                    self.assertIn(f'PID {pid},', command(str(cli), 'status').stdout)
                finally:
                    subprocess.run([str(cli), 'daemon', 'stop'], env=other_env, check=True, capture_output=True)
                # Preserve all user data and explicitly selected preferences during upgrades.
                preferences.write_text('{"startDaemon":false,"closeToTray":true}')
                sentinel = home / 'data/lyteboard/user-data'
                sentinel.parent.mkdir(parents=True, exist_ok=True)
                sentinel.write_bytes(b'unchanged identity/trust/history sentinel')
                login = home / f'config/autostart/{installer.APP}.desktop'
                login.parent.mkdir()
                login.write_text('[Desktop Entry]\nExec=/old/path\n' + installer.MARKER)
                install()
                self.assertNotIn(f'PID {pid},', command(str(cli), 'status').stdout)
                new_pid = int(re.search(r'PID (\d+),', command(str(cli), 'status').stdout)[1])
                self.assertEqual(os.readlink(f'/proc/{new_pid}/exe'), str(cli.resolve().with_name('lyteboardd')))
                self.assertFalse(json.loads(preferences.read_text())['startDaemon'])
                self.assertIn(str(cli.resolve().with_name('lyteboard-desktop')), login.read_text())
                self.assertTrue(sentinel.exists())
                # Missing release input fails before stopping an active daemon.
                failure = subprocess.run(['python3', str(SCRIPT), 'install', '--skip-build', '--release-dir', str(home / 'missing')], env=env, capture_output=True)
                self.assertNotEqual(failure.returncode, 0)
                self.assertIn(f'PID {new_pid},', command(str(cli), 'status').stdout)
                command('python3', str(home / '.local/lib/lyteboard/install-linux.py'), 'uninstall')
                self.assertFalse(cli.exists())
                self.assertFalse(login.exists())
                self.assertTrue(preferences.exists())
                self.assertTrue(sentinel.exists())
                # Repeated uninstall is safe.
                command('python3', str(SCRIPT), 'uninstall')
                install('--cli-only')
                self.assertFalse((home / '.local/bin/lyteboard-desktop').exists())
                self.assertFalse(json.loads(preferences.read_text())['startDaemon'])
                command('python3', str(SCRIPT), 'uninstall')
            finally:
                if cli.exists():
                    subprocess.run([str(cli), 'daemon', 'stop'], env=env, capture_output=True)


if __name__ == '__main__':
    unittest.main()

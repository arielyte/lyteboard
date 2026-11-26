#!/usr/bin/env python3
"""Build/install Lyteboard for the current Linux user. No services are enabled."""

import argparse
from contextlib import ExitStack, contextmanager
import fcntl
import json
import os
from pathlib import Path
import re
import select
import shutil
import signal
import stat
import subprocess
import sys
import tempfile

APP = "io.github.arielyte.lyteboard"
MARKER = "X-Lyteboard-Managed=true\n"
ROOT = Path(__file__).resolve().parent.parent


def run(command, **kwargs):
    return subprocess.run([str(arg) for arg in command], check=True, **kwargs)


def directory(path, private=False):
    path.mkdir(parents=True, exist_ok=True, mode=0o700 if private else 0o755)
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid():
        raise RuntimeError(f"Unsafe directory: {path}")
    if private and stat.S_IMODE(info.st_mode) != 0o700:
        raise RuntimeError(f"Expected private directory (0700): {path}")


def atomic(path, content, mode=0o644):
    directory(path.parent)
    with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as file:
        pending = Path(file.name)
        try:
            os.fchmod(file.fileno(), mode)
            file.write(content)
            file.flush()
            os.fsync(file.fileno())
            os.replace(pending, path)
        finally:
            pending.unlink(missing_ok=True)
    descriptor = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def desktop_exec(path):
    text = str(path)
    if any(ord(char) < 32 for char in text):
        raise RuntimeError("Launcher paths cannot contain control characters")
    escaped = ''.join('\\' + ch if ch in '\\"`$' else '%%' if ch == '%' else ch for ch in text)
    return '"' + escaped.replace('\\', '\\\\') + '"'


@contextmanager
def locked(path):
    descriptor = os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    try:
        info = os.fstat(descriptor)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise RuntimeError(f"Unsafe lock: {path}")
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise RuntimeError(f"Lyteboard is busy. Quit its window and retry ({path.name}).") from None
        yield
    finally:
        os.close(descriptor)


def wait_exit(pid):
    try:
        descriptor = os.pidfd_open(pid)
    except ProcessLookupError:
        return
    try:
        poller = select.poll()
        poller.register(descriptor, select.POLLIN)
        if not poller.poll(10000):
            raise RuntimeError(f"Process {pid} did not stop. Installation has been left unchanged.")
    finally:
        os.close(descriptor)


class Installation:
    def __init__(self):
        home = Path.home()
        self.base = home / ".local/lib/lyteboard"
        self.bin = self.base / "bin"
        self.links = home / ".local/bin"
        self.data = Path(os.environ.get("XDG_DATA_HOME") or home / ".local/share")
        self.config = Path(os.environ.get("XDG_CONFIG_HOME") or home / ".config")
        self.runtime = Path(os.environ.get("XDG_RUNTIME_DIR", ""))
        if not all(path.is_absolute() and '..' not in path.parts for path in
                   (self.data, self.config, self.runtime)):
            raise RuntimeError("Use absolute XDG paths and run inside your Linux desktop session.")
        self.launcher = self.data / f"applications/{APP}.desktop"
        self.icon = self.data / f"icons/hicolor/256x256/apps/{APP}.png"
        self.login = self.config / f"autostart/{APP}.desktop"

    def service_active(self):
        if not shutil.which("systemctl"):
            return False
        return subprocess.run(["systemctl", "--user", "is-active", "--quiet", "lyteboardd.service"],
                              stderr=subprocess.DEVNULL).returncode == 0

    def stop(self, cli, uninstall):
        service = self.service_active()
        if uninstall and service:
            raise RuntimeError("Disable the optional service first: systemctl --user disable --now lyteboardd.service")
        status = subprocess.run([str(cli), "status"], capture_output=True, text=True, timeout=6)
        match = re.fullmatch(r"Daemon running \(PID (\d+),.*\)\n", status.stdout)
        if status.returncode and "daemon is offline" not in status.stderr.lower():
            # Never interpret an IPC authentication or permissions failure as offline.
            raise RuntimeError(f"Cannot check daemon status: {status.stderr.strip()}")
        if status.returncode == 0 and not match:
            raise RuntimeError("Unrecognized daemon status. Quit the daemon before upgrading.")
        if service:
            service_pid = subprocess.check_output(
                ['systemctl', '--user', 'show', '--property=MainPID', '--value', 'lyteboardd.service'],
                text=True).strip()
            if not match or match[1] != service_pid:
                raise RuntimeError('The active systemd service uses a different runtime profile. Stop it explicitly before installing.')
            executable = subprocess.check_output(
                ['systemctl', '--user', 'show', '--property=ExecStart', '--value', 'lyteboardd.service'],
                text=True)
            path = re.search(r'\bpath=(.*?)\s*;', executable)
            if not path or path[1] not in (str(self.bin / 'lyteboardd'), str(self.links / 'lyteboardd')):
                raise RuntimeError('The active service uses a custom executable. Update its ExecStart to ~/.local/bin/lyteboardd before installing.')
            run(["systemctl", "--user", "stop", "lyteboardd.service"])
        elif match:
            run([cli, "daemon", "stop"])
        if match:
            wait_exit(int(match[1]))
        return bool(match), service

    def installed_pids(self, name):
        result = []
        for proc in Path('/proc').iterdir():
            if not proc.name.isdigit():
                continue
            try:
                executable = os.readlink(proc / 'exe').removesuffix(' (deleted)')
                if proc.stat().st_uid == os.getuid() and executable == str(self.bin / name):
                    result.append(int(proc.name))
            except (FileNotFoundError, ProcessLookupError, PermissionError):
                pass
        return result

    def check_running_profiles(self, cli):
        status = subprocess.run([str(cli), 'status'], capture_output=True, text=True, timeout=6)
        match = re.search(r'PID (\d+),', status.stdout)
        current = int(match[1]) if match else None
        if any(pid != current for pid in self.installed_pids('lyteboardd')):
            raise RuntimeError('Another runtime profile uses the installed daemon. Stop that profile before upgrading or removing binaries.')
        for pid in self.installed_pids('lyteboard-desktop'):
            try:
                environment = Path(f'/proc/{pid}/environ').read_bytes().split(b'\0')
            except FileNotFoundError:
                continue
            expected = os.fsencode('XDG_RUNTIME_DIR=' + str(self.runtime))
            if expected not in environment:
                raise RuntimeError('Quit installed Lyteboard windows using other runtime profiles before upgrading or removing binaries.')

    def close_installed_windows(self):
        # Restrict signals to this user's exact installed executable, never pkill by name.
        pids = self.installed_pids('lyteboard-desktop')
        for pid in pids:
            try:
                os.kill(pid, signal.SIGTERM)
                wait_exit(pid)
            except ProcessLookupError:
                pass
        return bool(pids)

    def check_links(self, names):
        for name in names:
            link = self.links / name
            if (link.exists() or link.is_symlink()) and (
                    not link.is_symlink() or os.readlink(link) != str(self.bin / name)):
                raise RuntimeError(f"Refusing to replace an unmanaged executable: {link}")
        for path in (self.launcher, self.login):
            if path.exists() or path.is_symlink():
                if path.is_symlink() or not path.is_file() or path.stat().st_size > 4096:
                    raise RuntimeError(f"Unsafe launcher: {path}")
                # Also recognize the repository's pre-installer desktop template.
                if MARKER not in path.read_text() and not (
                        path == self.launcher and 'Name=Lyteboard\n' in path.read_text()
                        and f'Icon={APP}\n' in path.read_text()):
                    raise RuntimeError(f"Refusing to replace an unmanaged launcher: {path}")

    def install(self, args):
        if args.cli_only and (self.bin / 'lyteboard-desktop').exists():
            raise RuntimeError('A desktop installation exists. Upgrade all three binaries together without --cli-only.')
        names = ['lyteboard', 'lyteboardd'] + ([] if args.cli_only else ['lyteboard-desktop'])
        if not args.skip_build:
            run(['cargo', 'build', '--locked', '--release', '-p', 'lyteboard-daemon', '-p', 'lyteboard-cli'], cwd=ROOT)
            if not args.cli_only:
                run(['npm', 'ci'], cwd=ROOT / 'apps/desktop')
                run(['npm', 'run', 'tauri', 'build', '--', '--no-bundle', '--ci', '--', '--locked'], cwd=ROOT / 'apps/desktop')
        self.check_links(names)
        first_gui = not (self.bin / 'lyteboard-desktop').exists()
        preferences = self.config / 'lyteboard/desktop.json'
        choices = None
        if first_gui and not args.cli_only:
            directory(preferences.parent, private=True)
            choices = {'startDaemon': True}
            if preferences.exists() or preferences.is_symlink():
                info = preferences.lstat()
                if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077 or info.st_size > 1024:
                    raise RuntimeError('Unsafe desktop preferences; check ownership and private permissions.')
                choices = json.loads(preferences.read_text())
                if not isinstance(choices, dict) or any(key not in ('startDaemon', 'closeToTray', 'minimizeToTray') or type(value) is not bool for key, value in choices.items()):
                    raise RuntimeError('Invalid desktop preferences; configuration was preserved.')
                choices['startDaemon'] = True
        with tempfile.TemporaryDirectory(prefix='.stage-', dir=self.base) as staging:
            stage = Path(staging)
            for name in names:
                source = args.release_dir / name
                with source.open('rb') as binary:
                    signature = binary.read(4)
                if signature != b'\x7fELF':
                    raise RuntimeError(f'Expected a native Linux release binary: {source}')
                shutil.copyfile(source, stage / name)
                os.chmod(stage / name, 0o755)
                with (stage / name).open('rb') as binary:
                    os.fsync(binary.fileno())
            if not args.cli_only:
                shutil.copyfile(ROOT / 'apps/desktop/src-tauri/icons/icon.png', stage / 'icon.png')
                launcher = (ROOT / f'contrib/desktop/{APP}.desktop').read_text()
                launcher = launcher.replace('Exec=lyteboard-desktop', 'Exec=' + desktop_exec(self.bin / 'lyteboard-desktop'))
            # Use the new CLI's authenticated IPC against the current runtime profile.
            self.check_running_profiles(stage / 'lyteboard')
            gui_running = self.close_installed_windows()
            with ExitStack() as locks:
                runtime = self.runtime / 'lyteboard'
                directory(runtime, private=True)
                locks.enter_context(locked(runtime / 'desktop.lock'))
                locks.enter_context(locked(runtime / 'launcher.lock'))
                was_running, service = self.stop(stage / 'lyteboard', False)
                locks.enter_context(locked(runtime / 'daemon.lock'))
                directory(self.bin)
                directory(self.links)
                for name in names:
                    os.replace(stage / name, self.bin / name)
                    link = self.links / name
                    if not link.is_symlink():
                        link.symlink_to(self.bin / name)
                if not args.cli_only:
                    atomic(self.launcher, (launcher + MARKER).encode())
                    atomic(self.icon, (stage / 'icon.png').read_bytes())
                    if self.login.exists():
                        atomic(self.login, (launcher + MARKER).encode())
                    if choices is not None:
                        atomic(preferences, json.dumps(choices).encode(), 0o600)
                atomic(self.base / 'install-linux.py', Path(__file__).read_bytes(), 0o755)
            if service:
                run(['systemctl', '--user', 'start', 'lyteboardd.service'])
            elif was_running:
                run([self.bin / 'lyteboard', 'daemon', 'start'])
            if gui_running:
                subprocess.Popen([str(self.bin / 'lyteboard-desktop')], start_new_session=True,
                                 stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.caches()
        print(f'Installed in {self.bin}.' + ('' if args.cli_only else ' Open Lyteboard from your application launcher.'))
        print(f'CLI: {self.links}/lyteboard (add {self.links} to PATH if needed).')

    def uninstall(self):
        names = ['lyteboard', 'lyteboardd', 'lyteboard-desktop']
        self.check_links(names)
        if shutil.which('systemctl') and subprocess.run(
                ['systemctl', '--user', 'is-enabled', '--quiet', 'lyteboardd.service'],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0:
            raise RuntimeError('Disable the optional service first: systemctl --user disable --now lyteboardd.service')
        if (self.bin / 'lyteboard').exists():
            self.check_running_profiles(self.bin / 'lyteboard')
        self.close_installed_windows()
        with ExitStack() as locks:
            runtime = self.runtime / 'lyteboard'
            directory(runtime, private=True)
            locks.enter_context(locked(runtime / 'desktop.lock'))
            locks.enter_context(locked(runtime / 'launcher.lock'))
            if (self.bin / 'lyteboard').exists():
                # Leave a daemon belonging to another installation untouched.
                status = subprocess.run([str(self.bin / 'lyteboard'), 'status'], capture_output=True, text=True, timeout=6)
                match = re.search(r'PID (\d+),', status.stdout)
                if match and os.readlink(f'/proc/{match[1]}/exe') == str(self.bin / 'lyteboardd'):
                    self.stop(self.bin / 'lyteboard', True)
            locks.enter_context(locked(runtime / 'daemon.lock'))
            for name in names:
                (self.links / name).unlink(missing_ok=True)
                (self.bin / name).unlink(missing_ok=True)
            for path in (self.launcher, self.icon, self.login, self.base / 'install-linux.py'):
                path.unlink(missing_ok=True)
        self.caches()
        print('Uninstalled. Device identities, trust, history, and settings were preserved.')

    def caches(self):
        for command in (['update-desktop-database', str(self.data / 'applications')], ['kbuildsycoca6', '--noincremental']):
            if shutil.which(command[0]):
                if subprocess.run(command, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode:
                    print(f'Warning: {command[0]} could not refresh the launcher cache. Log out and back in if the entry is missing.', file=sys.stderr)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['install', 'uninstall'])
    parser.add_argument('--skip-build', action='store_true', help='Install already-built release binaries')
    parser.add_argument('--release-dir', type=Path, default=ROOT / 'target/release')
    parser.add_argument('--cli-only', action='store_true', help='Install only the daemon and CLI')
    args = parser.parse_args()
    if sys.platform != 'linux' or os.getuid() == 0:
        parser.error('Run as your normal Linux desktop user, without sudo.')
    os.umask(0o077)
    installation = Installation()
    directory(installation.base, private=True)
    with locked(installation.base / 'install.lock'):
        if args.action == 'install':
            installation.install(args)
        else:
            installation.uninstall()


if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        sys.exit(f'lyteboard installer: {error}')

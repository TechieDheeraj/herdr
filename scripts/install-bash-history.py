#!/usr/bin/env python3
"""Install Herdr's optional Bash history hook without replacing shell settings."""
import os
from pathlib import Path
import stat
import tempfile
import time

ASSET = Path(__file__).resolve().parents[1] / 'src/integration/assets/bash/herdr-history.bash'
LOADER = '''# Herdr Bash history (managed loader)
if [ -n "${BASH_VERSION:-}" ]; then
  _herdr_history_config=${XDG_CONFIG_HOME:-$HOME/.config}
  case $_herdr_history_config in /*) ;; *) _herdr_history_config=$HOME/.config ;; esac
  if [ -r "$_herdr_history_config/herdr/bash-history.bash" ]; then
    . "$_herdr_history_config/herdr/bash-history.bash"
  fi
  unset _herdr_history_config
fi
'''


def write_if_changed(path, content):
    # Preserve symlinked dotfiles (including bash_profile -> bashrc).
    target = path.resolve()
    original = target.read_bytes() if target.exists() else None
    payload = content.encode()
    if original == payload:
        return
    target.parent.mkdir(parents=True, exist_ok=True)
    mode = stat.S_IMODE(target.stat().st_mode) if original is not None else 0o600
    if original is not None:
        if not mode & 0o222:
            raise PermissionError(f'{target} is read-only')
        backup = target.with_name(target.name + '.herdr-history-backup-' + str(time.time_ns()))
        with backup.open('xb') as handle:
            os.fchmod(handle.fileno(), 0o600)
            handle.write(original)
    fd, temporary = tempfile.mkstemp(prefix='.' + target.name + '-', dir=str(target.parent))
    try:
        with os.fdopen(fd, 'wb') as handle:
            os.fchmod(handle.fileno(), mode)
            handle.write(payload)
        os.replace(temporary, target)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def install(home, config_home):
    write_if_changed(config_home / 'herdr/bash-history.bash', ASSET.read_text())
    rc = home / '.bashrc'
    profile = next((home / name for name in ('.bash_profile', '.bash_login', '.profile')
                    if (home / name).exists()), home / '.bash_profile')
    seen = set()
    for path in (rc, profile):
        target = path.resolve()
        if target in seen:
            continue
        seen.add(target)
        text = target.read_text() if target.exists() else ''
        if '# Herdr Bash history (managed loader)' in text:
            continue
        if path == profile and not text:
            text = 'if [ -r "$HOME/.bashrc" ]; then . "$HOME/.bashrc"; fi\n'
        write_if_changed(path, text.rstrip('\n') + '\n\n' + LOADER)


if __name__ == '__main__':
    home = Path.home()
    config_home = Path(os.environ.get('XDG_CONFIG_HOME', str(home / '.config')))
    if not config_home.is_absolute():
        config_home = home / '.config'
    install(home, config_home)
    print('Bash history integration installed for new/restored Herdr shells.')

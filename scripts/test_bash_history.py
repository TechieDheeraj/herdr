import importlib.util
import os
from pathlib import Path
import select
import shlex
import shutil
import subprocess
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('history_installer', ROOT / 'scripts/install-bash-history.py')
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)
BASH = os.environ.get('HERDR_TEST_BASH') or shutil.which('bash')
PROMPT = b'HISTORY-PROMPT> '


@unittest.skipUnless(os.name == 'posix', 'Bash startup installation targets Unix')
class InstallerTests(unittest.TestCase):
    def test_idempotent_install_preserves_symlinks_and_contents(self):
        with tempfile.TemporaryDirectory() as folder:
            home = Path(folder)
            original = '# personal settings\nexport EDITOR=vim\n'
            target = home / 'dotfiles/rc'
            target.parent.mkdir()
            target.write_text(original)
            (home / '.bashrc').symlink_to(target)
            (home / '.bash_profile').symlink_to(home / '.bashrc')
            installer.install(home, home / '.config')
            installed = target.read_text()
            self.assertTrue(installed.startswith(original))
            self.assertEqual(installed.count('# Herdr Bash history (managed loader)'), 1)
            installer.install(home, home / '.config')
            self.assertEqual(target.read_text(), installed)
            self.assertTrue((home / '.bashrc').is_symlink())
            backups = list(target.parent.glob('rc.herdr-history-backup-*'))
            self.assertEqual(len(backups), 1)
            self.assertEqual(backups[0].read_text(), original)
            self.assertEqual(backups[0].stat().st_mode & 0o777, 0o600)

    def test_existing_login_file_retained(self):
        with tempfile.TemporaryDirectory() as folder:
            home = Path(folder)
            (home / '.profile').write_text('# existing profile\n')
            installer.install(home, home / '.config')
            self.assertFalse((home / '.bash_profile').exists())
            self.assertTrue((home / '.profile').read_text().startswith('# existing profile\n'))


class Shell:
    def __init__(self, home, identity, setup='', extra_env=None, login=False):
        import pty
        self.master, slave = pty.openpty()
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(('HERDR_', '_HERDR_')) and k not in
               ('ENV', 'BASH_ENV', 'PROMPT_COMMAND', 'SHELLOPTS', 'BASHOPTS', 'HISTFILE', 'HISTIGNORE', 'HISTCONTROL')}
        env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / '.config'),
                   XDG_STATE_HOME=str(home / 'state with spaces'), HERDR_ENV='1',
                   HERDR_SHELL_HISTORY_ID=identity, TERM='xterm')
        env.update(extra_env or {})
        rc = home / '.bashrc'
        rc.write_text("PS1='HISTORY-PROMPT> '\nHISTSIZE=1000\nHISTFILESIZE=2000\n" + setup + '\n' + installer.LOADER)
        args = [BASH, '--login', '-i'] if login else [BASH, '--noprofile', '--rcfile', str(rc), '-i']
        self.child = subprocess.Popen(args, env=env, stdin=slave, stdout=slave, stderr=slave,
                                      start_new_session=True)
        os.close(slave)
        self.prompt()

    def prompt(self):
        result = b''
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if select.select([self.master], [], [], .1)[0]:
                try:
                    result += os.read(self.master, 65536)
                except OSError:
                    break
                if result.endswith(PROMPT):
                    return result.decode(errors='replace')
        raise AssertionError(f'Bash prompt not reached: {result!r}')

    def send(self, command):
        os.write(self.master, command.encode() + b'\n')
        return self.prompt()

    def close(self):
        if self.child.poll() is None:
            self.child.kill()  # Deliberately no graceful-exit history flush.
        self.child.wait(timeout=5)
        os.close(self.master)


@unittest.skipUnless(os.name == 'posix' and BASH, 'interactive Bash requires Unix PTYs')
class BashHistoryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='herdr-bash-')
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name)
        self.config = self.home / '.config'
        self.original = 'echo global-seed\n'
        (self.home / '.bash_history').write_text(self.original)
        installer.install(self.home, self.config)

    def shell(self, identity='a' * 32, **kwargs):
        shell = Shell(self.home, identity, **kwargs)
        self.addCleanup(lambda: shell.close() if shell.master is not None else None)
        return shell

    def kill(self, shell):
        shell.close()
        shell.master = None

    def history(self, identity='a' * 32):
        return self.home / 'state with spaces/herdr/shell-history/bash' / (identity + '.history')

    def test_prompt_saves_and_up_arrow_recalls_after_abrupt_restart(self):
        first = self.shell()
        first.send("echo pane-one-command")
        self.assertIn('echo pane-one-command', self.history().read_text())
        self.kill(first)
        second = self.shell('b' * 32)
        second.send('echo pane-two-command')
        self.kill(second)
        restored = self.shell()
        result = restored.send('\x1b[A')
        self.assertIn('pane-one-command', result)
        self.assertNotIn('pane-two-command', result)
        self.assertEqual((self.home / '.bash_history').read_text(), self.original)
        self.assertEqual(self.history().stat().st_mode & 0o777, 0o600)

    def test_only_this_panes_commands_are_loaded_and_traversed(self):
        shell = self.shell(setup='history -r "$HOME/.bash_history"')
        shell.send('echo pane-first')
        shell.send('echo pane-second')
        self.kill(shell)
        other = self.shell('b' * 32)
        other.send('echo other-pane')
        self.kill(other)
        restored = self.shell(setup='history -r "$HOME/.bash_history"')
        self.assertEqual(self.history().read_text().splitlines(), ['echo pane-first', 'echo pane-second'])
        # Traverse two entries without submitting either command. Further Up
        # must remain on the first entry, never reveal global/other-pane history.
        os.write(restored.master, b'\x1b[A\x1b[A\x1b[A\n')
        output = restored.prompt()
        self.assertIn('pane-first', output)
        self.assertNotIn('global-seed', output)
        self.assertNotIn('other-pane', output)

    def test_empty_initial_history_and_small_history_limit(self):
        (self.home / '.bash_history').unlink()
        shell = self.shell(setup='HISTSIZE=1')
        shell.send('echo first-ever-command')
        self.assertIn('echo first-ever-command', self.history().read_text())
        shell.send('echo second-command')
        self.assertIn('echo second-command', self.history().read_text())
        self.kill(shell)
        restored = self.shell(setup='HISTSIZE=1')
        self.assertIn('second-command', restored.send('\x1b[A'))

    def test_existing_prompt_history_hooks_cannot_import_global_or_erase_commands(self):
        for prompt in ['history -w', 'history -a; history -c; history -r']:
            with self.subTest(prompt=prompt):
                if self.history().exists():
                    self.history().unlink()
                setup = 'history -r "$HOME/.bash_history"\nPROMPT_COMMAND=' + shlex.quote(prompt)
                shell = self.shell(setup=setup)
                shell.send('echo private-command')
                self.kill(shell)
                restored = self.shell(setup=setup)
                self.assertEqual(self.history().read_text().splitlines(), ['echo private-command'])
                result = restored.send('\x1b[A\x1b[A')
                self.assertIn('private-command', result)
                self.assertNotIn('global-seed', result)
                self.kill(restored)

    def test_normal_exit_does_not_duplicate_seeded_pane_history(self):
        self.history().parent.mkdir(parents=True)
        self.history().write_text('echo earlier-command-in-this-pane\n')
        shell = self.shell()
        shell.send('echo saved-once')
        os.write(shell.master, b'exit\n')
        deadline = time.monotonic() + 5
        while shell.child.poll() is None and time.monotonic() < deadline:
            if select.select([shell.master], [], [], .1)[0]:
                try:
                    os.read(shell.master, 65536)
                except OSError:
                    break
        shell.child.wait(timeout=1)
        self.assertEqual(self.history().read_text().splitlines().count('echo saved-once'), 1)

    def test_filters_multiline_and_command_status_preserved(self):
        shell = self.shell(setup="HISTCONTROL=ignoreboth\nHISTIGNORE='echo ignored'\nshopt -s cmdhist lithist\nPROMPT_COMMAND='saved_status=$? # existing prompt'\n")
        shell.send(' echo secret')
        shell.send('echo ignored')
        shell.send("for x in a b; do\n echo \"$x\"\ndone")
        shell.send('false')
        output = shell.send('echo STATUS=$saved_status')
        self.assertIn('STATUS=1', output)
        history = self.history().read_text()
        self.assertNotIn('echo secret', history)
        self.assertNotIn('echo ignored', history)
        self.assertIn('for x in a b;', history)

    def test_repeated_source_repairs_prompt_after_login_profile_override(self):
        (self.home / '.bash_profile').write_text('. "$HOME/.bashrc"\nPROMPT_COMMAND="echo login-hook"\n' + installer.LOADER)
        shell = self.shell(login=True)
        result = shell.send('echo after-login')
        self.assertIn('login-hook', result)
        self.assertIn('echo after-login', self.history().read_text())
        shell.send('. "$HOME/.bashrc"; . "$HOME/.bashrc"')
        output = shell.send('declare -p PROMPT_COMMAND')
        self.assertEqual(output.count('__herdr_history_flush'), 1)

    def test_unset_prompt_with_errexit_and_nested_shell(self):
        shell = self.shell(setup='set -e\nunset PROMPT_COMMAND')
        shell.send('echo errexit-ok')
        shell.send(shlex.quote(BASH) + ' --noprofile --rcfile "$HOME/.bashrc" -i')
        shell.send('echo nested-ok')
        self.assertIn('echo nested-ok', self.history().read_text())
        shell.send('exit')

    def test_history_opt_out_and_old_server_are_untouched(self):
        for setup, env in [('HISTFILE=/dev/null', {}), ('unset HISTFILE', {}),
                           ('set +o history', {}), ('readonly HISTFILE', {}),
                           ('readonly PROMPT_COMMAND=:', {}), ('', {'HERDR_ENV': ''}),
                           ('', {'HERDR_SHELL_HISTORY_ID': ''})]:
            with self.subTest(setup=setup, env=env):
                shell = self.shell(setup=setup, extra_env=env)
                shell.send('echo disabled')
                self.kill(shell)
                self.assertFalse(self.history().exists())

    def test_relative_config_home_matches_installer_fallback(self):
        shell = self.shell(extra_env={'XDG_CONFIG_HOME': 'relative'})
        shell.send('echo fallback')
        self.assertIn('echo fallback', self.history().read_text())

    def test_history_symlink_is_not_followed(self):
        target = self.home / 'do-not-touch'
        target.write_text('original\n')
        self.history().parent.mkdir(parents=True)
        self.history().symlink_to(target)
        shell = self.shell()
        shell.send('echo safe')
        self.kill(shell)
        self.assertEqual(target.read_text(), 'original\n')

    def test_prompt_array_on_modern_bash(self):
        version = subprocess.check_output([BASH, '-c', 'echo "${BASH_VERSINFO[0]}.${BASH_VERSINFO[1]}"'], text=True).strip()
        if tuple(map(int, version.split('.'))) < (5, 1):
            self.skipTest('PROMPT_COMMAND arrays require Bash 5.1+')
        shell = self.shell(setup="PROMPT_COMMAND=('saved_status=$?' 'echo array-hook')")
        result = shell.send('echo array-command')
        self.assertIn('array-hook', result)
        self.assertIn('echo array-command', self.history().read_text())

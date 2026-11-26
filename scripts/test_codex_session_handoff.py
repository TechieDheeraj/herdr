import importlib.util
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('handoff', ROOT / 'scripts/codex-session-handoff.py')
handoff = importlib.util.module_from_spec(spec)
spec.loader.exec_module(handoff)
SID = '01234567-89ab-cdef-0123-456789abcdef'


class MigrationTests(unittest.TestCase):
    def test_draft_uuid_is_not_a_session_footer(self):
        self.assertIsNone(handoff.footer_id(f'› investigate\n  {SID}\n\n  model · directory\n  ? for shortcuts'))

    def test_migration_preserves_items_exported_by_installer(self):
        with tempfile.TemporaryDirectory() as folder:
            Path(folder, 'herdr-codex-status-line.json').write_text(json.dumps({'version':11, 'items':['session-id','git-branch','model']}))
            with patch.dict(os.environ, {'CODEX_HOME':folder}):
                self.assertEqual(handoff.configured_items(), ['session-id','git-branch','model'])

    def test_placeholder_requires_dim_style(self):
        self.assertTrue(handoff.empty_composer('› \x1b[2mAsk Codex to do anything\x1b[0m'))
        self.assertFalse(handoff.empty_composer('› Ask Codex to do anything'))
        self.assertFalse(handoff.empty_composer('› \x1b[2mmy draft\x1b[0m'))

    def test_busy_or_draft_pane_is_never_typed_into(self):
        for status, empty in [('working', True), ('idle', False)]:
            migration = handoff.Migration('/test/herdr', 'test')
            with patch.object(migration, 'process', return_value={'foreground_process_group_id': 10}), \
                 patch.object(migration, 'screen', return_value='› draft'), \
                 patch.object(migration, 'pane', return_value={'agent_status': status, 'scroll': {'offset_from_bottom': 0}}), \
                 patch.object(migration, 'stable', return_value=True), \
                 patch.object(handoff, 'empty_composer', return_value=empty), \
                 patch.object(migration, 'cli', return_value='draft') as cli:
                with self.assertRaises(RuntimeError):
                    migration.migrate('w1:p1', ['session-id'])
                self.assertFalse(any('send-text' in call.args or 'send-keys' in call.args for call in cli.call_args_list))

    def test_captured_footer_is_idempotent_without_input(self):
        migration = handoff.Migration('/test/herdr', 'test')
        screen = f'› \n\n  {SID}\n  ? for shortcuts'
        with patch.object(migration, 'process', return_value={'foreground_process_group_id': 10}), \
             patch.object(migration, 'screen', return_value=screen), \
             patch.object(migration, 'pane', return_value={'agent_session': {'value': SID}}), \
             patch.object(migration, 'stable', return_value=True), \
             patch.object(migration, 'cli') as cli:
            self.assertEqual(migration.migrate('w1:p1', ['session-id']), SID)
            cli.assert_not_called()

    def test_old_server_gets_verified_id_before_handoff(self):
        migration = handoff.Migration('/test/herdr', 'test')
        screen = f'› \n\n  {SID}\n  ? for shortcuts'
        with patch.object(migration, 'process', return_value={'foreground_process_group_id':10}), \
             patch.object(migration, 'screen', return_value=screen), \
             patch.object(migration, 'pane', side_effect=[{}, {'agent_session': {'value':SID}}]), \
             patch.object(migration, 'stable', return_value=True), \
             patch.object(migration, 'cli') as cli, \
             patch.object(handoff.time, 'sleep'):
            self.assertEqual(migration.migrate('w1:p1', ['session-id']), SID)
            self.assertEqual(cli.call_count, 1)
            self.assertEqual(cli.call_args.args[:3], ('pane', 'report-agent-session', 'w1:p1'))
            self.assertIn(SID, cli.call_args.args)

    def test_explicit_session_clears_inherited_sockets(self):
        with patch.dict(os.environ, {'HERDR_SOCKET_PATH': 'wrong', 'HERDR_SESSION': 'wrong'}):
            migration = handoff.Migration('/test/herdr', 'target')
            self.assertEqual(migration.command, ['/test/herdr', '--session', 'target'])
            self.assertNotIn('HERDR_SOCKET_PATH', migration.env)
            self.assertNotIn('HERDR_SESSION', migration.env)


@unittest.skipUnless(os.name == 'posix', 'Unix hook transport')
class HookOwnershipTests(unittest.TestCase):
    def test_only_a_descendant_of_the_target_pane_can_report(self):
        for owner, daemon in [(os.getpid(), False), (0, False), (os.getpid(), True)]:
            with self.subTest(owner=owner, daemon=daemon), tempfile.TemporaryDirectory(prefix='cxhook-', dir='/tmp') as base:
                listener = socket.socket(socket.AF_UNIX)
                path = str(Path(base) / 'api.sock')
                listener.bind(path)
                listener.listen()
                listener.settimeout(.1)
                done = threading.Event()
                reports = []
                def serve():
                    while not done.is_set():
                        try:
                            conn, _ = listener.accept()
                        except socket.timeout:
                            continue
                        with conn:
                            request = json.loads(conn.makefile().readline())
                            if request['method'] == 'pane.process_info':
                                reply = {'result': {'process_info': {'foreground_processes': [{'pid': owner}]}}}
                            else:
                                reports.append(request)
                                reply = {'result': {'type': 'ok'}}
                            conn.sendall((json.dumps(reply) + '\n').encode())
                server = threading.Thread(target=serve)
                server.start()
                try:
                    env = dict(os.environ, HERDR_ENV='1', HERDR_SOCKET_PATH=path, HERDR_PANE_ID='test')
                    env.pop('CODEX_THREAD_ID', None)
                    command = ['/bin/sh', str(ROOT / 'src/integration/assets/codex/herdr-agent-state.sh'), 'session']
                    if daemon:
                        command = [sys.executable, '-c', 'import subprocess,sys; sys.exit(subprocess.run(sys.argv[2:]).returncode)', 'app-server'] + command
                    result = subprocess.run(command,
                        env=env, input=json.dumps({'hook_event_name':'SessionStart', 'session_id':SID, 'transcript_path':'/tmp/test.jsonl'}),
                        text=True, capture_output=True, timeout=5, start_new_session=True)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(len(reports), 1 if owner and not daemon else 0)
                    if owner and not daemon:
                        self.assertEqual(reports[0]['params']['agent_session_id'], SID)
                finally:
                    done.set()
                    server.join(timeout=2)
                    listener.close()


if __name__ == '__main__':
    unittest.main()

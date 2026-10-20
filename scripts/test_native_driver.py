#!/usr/bin/env python3
"""Focused wrapper regression tests: python3 scripts/test_native_driver.py."""
import importlib.util
import json
from pathlib import Path
import sqlite3
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock

spec = importlib.util.spec_from_file_location('native_driver', Path(__file__).with_name('native-driver.py'))
driver = importlib.util.module_from_spec(spec)
spec.loader.exec_module(driver)


class PersistenceTest(unittest.TestCase):
    def test_arbitrary_legacy_names_and_existing_sqlite_headers(self):
        for name in ['state', 'state.backup', 'state.sqlite3']:
            path = self.root / name
            path.write_text(' ' * 64 + '{"version":1,"workspace":{}}')
            self.assertEqual(driver.database_path(path), Path(str(path) + '.sqlite3'))
            live = driver.database_path(path)
            live.write_bytes(self.database.read_bytes())
            path.write_bytes(b'')
            self.assertEqual(driver.database_path(path), live)
            path.unlink()
            self.assertEqual(driver.database_path(path), live)
        padded = self.root / 'padded.backup'
        padded.write_bytes(b' ' * 16 + self.database.read_bytes()[:16])
        self.assertEqual(driver.database_path(padded), Path(str(padded) + '.sqlite3'))
        existing = self.root / 'database.custom'
        existing.write_bytes(self.database.read_bytes())
        self.assertEqual(driver.database_path(existing), existing)
        self.assertEqual(driver.database_path(self.root / 'new-state'), self.root / 'new-state.sqlite3')

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='keystone-driver-test-')
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.state = self.root / 'workspace.json'
        self.database = driver.database_path(self.state)
        self.writer = sqlite3.connect(self.database)
        self.addCleanup(self.writer.close)
        self.writer.executescript('''
            PRAGMA journal_mode=WAL;
            PRAGMA wal_autocheckpoint=0;
            CREATE TABLE workspace(singleton INTEGER PRIMARY KEY, payload TEXT);
            CREATE TABLE sessions(position INTEGER, payload TEXT);
            CREATE TABLE log_entries(sequence INTEGER, payload TEXT);
        ''')
        self.saved = {'version': 1, 'next_id': 3, 'future': {'preserved': True},
                      'workspace': {'selected': 2, 'sessions': [], 'theme': 'Dark'}}
        self.writer.execute('INSERT INTO workspace VALUES(1, ?)', [json.dumps(self.saved)])
        self.writer.executemany('INSERT INTO sessions VALUES(?, ?)', [
            (1, json.dumps({'id': 2, 'draft': 'second'})),
            (0, json.dumps({'id': 1, 'draft': 'first'}))])
        self.writer.commit()
        self.writer.execute('PRAGMA wal_checkpoint(TRUNCATE)')
        self.writer.execute('UPDATE sessions SET payload=? WHERE position=0',
                            [json.dumps({'id': 1, 'draft': 'committed in WAL'})])
        self.writer.execute('INSERT INTO log_entries VALUES(1, ?)', ['{"entry":"turn_started"}'])
        self.writer.commit()

    def run_without_process(self):
        run = driver.Run.__new__(driver.Run)
        run.state = self.state
        run.evidence = self.root / 'evidence'
        run.evidence.mkdir()
        run.sequence = 7
        run.record = Mock()
        return run

    def test_projection_reads_wal_preserves_fields_and_orders_sessions(self):
        self.state.write_text('{"legacy":"must not read"}')
        snapshot = driver.read_workspace(self.state)
        self.assertEqual(snapshot['future'], {'preserved': True})
        self.assertEqual(snapshot['workspace']['selected'], 2)
        self.assertEqual(snapshot['workspace']['sessions'], [
            {'id': 1, 'draft': 'committed in WAL'}, {'id': 2, 'draft': 'second'}])
        self.assertGreater(Path(str(self.database) + '-wal').stat().st_size, 0)
        with driver.closing(driver.open_database(self.database)) as connection:
            with self.assertRaisesRegex(sqlite3.OperationalError, 'readonly'):
                connection.execute('DELETE FROM sessions')

    def test_projection_uses_one_consistent_read_transaction(self):
        with driver.closing(driver.open_database(self.database)) as connection:
            writer = self.writer

            class Interleaved:
                def execute(inner, sql):
                    if sql.startswith('SELECT payload FROM sessions'):
                        writer.execute('UPDATE workspace SET payload=?', [json.dumps({
                            **self.saved, 'workspace': {'selected': 1, 'sessions': []}})])
                        writer.execute('DELETE FROM sessions WHERE position=0')
                        writer.commit()
                    return connection.execute(sql)

                def rollback(inner):
                    connection.rollback()

            snapshot = driver.workspace_projection(Interleaved())
        self.assertEqual(snapshot['workspace']['selected'], 2)
        self.assertEqual([session['id'] for session in snapshot['workspace']['sessions']], [1, 2])
        self.assertEqual(driver.read_workspace(self.state)['workspace']['selected'], 1)

    def test_snapshot_backup_includes_wal_and_matches_json_projection(self):
        run = self.run_without_process()
        result = run.snapshot()
        backup = Path(result['database_path'])
        self.assertEqual(result['database_sha256'], driver.digest(backup))
        self.assertEqual(result['sha256'], driver.digest(result['path']))
        self.assertEqual(json.loads(Path(result['path']).read_text()), driver.read_workspace(backup))
        self.assertEqual(driver.read_workspace(backup)['workspace']['sessions'][0]['draft'],
                         'committed in WAL')
        with driver.closing(driver.open_database(backup)) as connection:
            self.assertEqual(connection.execute('SELECT payload FROM log_entries').fetchall(),
                             [('{"entry":"turn_started"}',)])
        with driver.closing(sqlite3.connect(backup.as_uri() + '?mode=ro&immutable=1',
                                             uri=True)) as connection:
            self.assertEqual(driver.workspace_projection(connection),
                             json.loads(Path(result['path']).read_text()))

    def test_missing_database_does_not_read_legacy_or_create_sqlite(self):
        legacy = self.root / 'missing.json'
        legacy.write_text('{}')
        self.assertEqual(driver.read_workspace(legacy), {'state_absent': True})
        self.assertFalse(legacy.with_suffix('.sqlite3').exists())

    def test_stress_observes_sqlite_draft_instead_of_legacy_json(self):
        spec = importlib.util.spec_from_file_location(
            'native_stress', Path(__file__).with_name('native-stress.py'))
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        stress = module.Stress.__new__(module.Stress)
        stress.run = SimpleNamespace(state=self.state)
        self.state.write_text('{"workspace":{"sessions":[{"draft":"stale legacy"}]}}')
        self.assertEqual(stress.draft(), 'committed in WAL')
        stress.run.state = self.root / 'absent.json'
        self.assertIsNone(stress.draft())

    def test_stop_keeps_consistent_backup_and_cleans_owned_scratch(self):
        run = self.run_without_process()
        run.closed = False
        run.scratch = self.root / 'scratch'
        run.scratch.mkdir()
        run.process = Mock()
        run.log = Mock()
        run.stderr = Mock()
        run.call = Mock()
        result = run.stop()
        self.assertEqual(result['errors'], [])
        self.assertIsNone(result['scratch_retained'])
        self.assertFalse(run.scratch.exists())
        self.assertTrue(Path(result['snapshot']['database_path']).exists())
        run.process.stdin.close.assert_called_once()
        run.log.close.assert_called_once()
        run.stderr.close.assert_called_once()
        self.assertEqual(run.stop(), {'already_closed': True})

    def test_failed_snapshot_still_closes_process_and_retains_scratch(self):
        run = self.run_without_process()
        run.closed = False
        run.scratch = self.root / 'scratch'
        run.scratch.mkdir()
        run.process = Mock()
        run.log = Mock()
        run.stderr = Mock()
        run.call = Mock()
        run.snapshot = Mock(side_effect=sqlite3.OperationalError('read failed'))
        with self.assertRaisesRegex(RuntimeError, 'read failed'):
            run.stop()
        self.assertTrue(run.scratch.exists())
        run.process.stdin.close.assert_called_once()
        run.process.wait.assert_called_once_with(timeout=10)
        run.log.close.assert_called_once()
        run.stderr.close.assert_called_once()


class WheelTest(unittest.TestCase):
    def setUp(self):
        self.server = driver.Server(SimpleNamespace())
        self.run = Mock(closed=False)
        self.run.find.return_value = {'center': {'x': 500, 'y': 300}}
        self.run.call.return_value = {'structuredContent': {'ok': True}}
        self.server.run = self.run

    def test_named_wheel_uses_live_center_and_signed_axis_distances(self):
        self.assertEqual(self.server.handle('wheel', {'id': 'conversation', 'vertical': -3,
                                                     'horizontal': 2}), ({'ok': True}, None))
        self.run.find.assert_called_once_with('conversation')
        self.run.call.assert_called_once_with('input', {'actions': [
            {'action': 'scroll', 'x': 500, 'y': 300, 'scroll_x': 30, 'scroll_y': -45}]})

    def test_explicit_pixels_and_defaults(self):
        self.server.handle('wheel', {'x': 20, 'y': 30, 'vertical': 1})
        self.run.find.assert_not_called()
        self.run.call.assert_called_once_with('input', {'actions': [
            {'action': 'scroll', 'x': 20, 'y': 30, 'scroll_x': 0, 'scroll_y': 15}]})

    def test_rejects_nonintegral_or_unbounded_notches_before_input(self):
        for value in (True, 0.5, '3', -101, 101, None):
            for axis in ('horizontal', 'vertical'):
                with self.subTest(value=value, axis=axis), self.assertRaises(ValueError):
                    self.server.handle('wheel', {'id': 'conversation', axis: value})
        self.run.call.assert_not_called()

    def test_requires_target_and_propagates_missing_element(self):
        with self.assertRaises(ValueError):
            self.server.handle('wheel', {'vertical': -1})
        self.run.find.side_effect = RuntimeError('clipped')
        with self.assertRaisesRegex(RuntimeError, 'clipped'):
            self.server.handle('wheel', {'id': 'hidden', 'vertical': -1})
        self.run.call.assert_not_called()

    def test_raw_scroll_input_remains_unchanged(self):
        actions = [{'action': 'scroll', 'x': 1, 'y': 2, 'scroll_x': 0, 'scroll_y': -45}]
        self.server.handle('input', {'actions': actions})
        self.run.call.assert_called_once_with('input', {'actions': actions})

    def test_files_editor_overrides_survive_driver_integration(self):
        env = {'VISUAL': '/disposable/editor --wait', 'EDITOR': '/disposable/fallback',
               'KEYSTONE_PROJECT_DIR': '/disposable/project'}
        self.assertEqual(driver.app_env(env), env)
        with self.assertRaises(ValueError):
            driver.app_env({'KEYSTONE_STATE_PATH': '/not-owned-by-run'})


if __name__ == '__main__':
    unittest.main()

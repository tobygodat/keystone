#!/usr/bin/env python3
"""Focused wrapper regression tests: python3 scripts/test_native_driver.py."""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import sqlite3
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

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
            CREATE TABLE metadata(key TEXT PRIMARY KEY, value TEXT);
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
        run.bus = None
        run.recording = None
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

    def test_projection_reads_accounts_in_order_when_the_table_exists(self):
        self.assertNotIn('accounts', driver.read_workspace(self.state)['workspace'])
        self.writer.execute('CREATE TABLE accounts(id TEXT, position INTEGER, payload TEXT)')
        self.writer.commit()
        self.assertNotIn('accounts', driver.read_workspace(self.state)['workspace'])
        self.writer.executemany('INSERT INTO accounts VALUES(?, ?, ?)', [
            ('codex-b', 1, json.dumps({'id': 'codex-b', 'label': 'B'})),
            ('claude-a', 0, json.dumps({'id': 'claude-a', 'label': 'A'}))])
        self.writer.commit()
        self.assertEqual([a['id'] for a in driver.read_workspace(self.state)['workspace']['accounts']],
                         ['claude-a', 'codex-b'])

    def test_projection_reads_projects_in_order_when_the_table_exists(self):
        self.assertNotIn('projects', driver.read_workspace(self.state)['workspace'])
        self.writer.execute('CREATE TABLE projects(id TEXT, position INTEGER, payload TEXT)')
        self.writer.commit()
        self.assertNotIn('projects', driver.read_workspace(self.state)['workspace'])
        self.writer.executemany('INSERT INTO projects VALUES(?, ?, ?)', [
            ('/b', 1, json.dumps({'path': '/b'})),
            ('/a', 0, json.dumps({'path': '/a', 'last_opened': 5}))])
        self.writer.commit()
        self.assertEqual([p['path'] for p in driver.read_workspace(self.state)['workspace']['projects']],
                         ['/a', '/b'])
    def test_projection_reads_prompts_in_order_when_the_table_exists(self):
        self.assertNotIn('prompts', driver.read_workspace(self.state)['workspace'])
        self.writer.execute('CREATE TABLE prompts(id TEXT, position INTEGER, payload TEXT)')
        self.writer.executemany('INSERT INTO prompts VALUES(?, ?, ?)', [
            ('4', 1, json.dumps({'id': 4, 'title': 'Second'})),
            ('3', 0, json.dumps({'id': 3, 'title': 'First'}))])
        self.writer.commit()
        self.assertEqual([p['title'] for p in driver.read_workspace(self.state)['workspace']['prompts']],
                         ['First', 'Second'])

    def test_projection_reads_notes_when_the_table_exists(self):
        self.assertNotIn('notes', driver.read_workspace(self.state)['workspace'])
        self.writer.execute('CREATE TABLE notes(project TEXT, position INTEGER, payload TEXT)')
        self.writer.execute('INSERT INTO notes VALUES(?, ?, ?)',
                            ('/a', 0, json.dumps({'project': '/a', 'text': 'Remember'})))
        self.writer.commit()
        self.assertEqual(driver.read_workspace(self.state)['workspace']['notes'],
                         [{'project': '/a', 'text': 'Remember'}])

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

    def test_the_app_gets_a_private_bus_that_stop_ends(self):
        run = self.run_without_process()
        run.scratch = self.root / 'scratch'
        run.scratch.mkdir()
        run.stderr = None
        run.start_bus()
        self.addCleanup(run.stop_bus)
        self.assertTrue(run.bus_address.startswith(f'unix:path={run.scratch}/bus'),
                        run.bus_address)
        self.assertNotIn('/run/user/', run.bus_address)
        run.geometry = run.scratch / 'elements.json'
        run.env = {}
        run.trace_wayland = False
        run.prime_keyboard = False
        run.binary = Path('/disposable/keystone-desktop')
        run.exit_files, run.app_logs, run.app_pids = {}, {}, []
        run.call = Mock(side_effect=RuntimeError('stop at launch'))
        with self.assertRaisesRegex(RuntimeError, 'stop at launch'):
            run.launch()
        argv = run.call.call_args.args[1]['argv']
        self.assertIn(f'DBUS_SESSION_BUS_ADDRESS={run.bus_address}', argv)
        bus = run.bus
        run.stop_bus()
        self.assertIsNotNone(bus.poll(), 'the daemon is gone')

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
        self.run.find.assert_called_once_with('conversation', None)
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


class WindowsTest(unittest.TestCase):
    def frame(self, path, windows):
        path.write_text(json.dumps({'pid': 7, 'revision': 3, 'windows': windows}))

    def run_for(self, compositor):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        run = SimpleNamespace(geometry=Path(temporary.name) / 'elements.json', app_pids=[7],
                              record=Mock(),
                              call=Mock(return_value={'structuredContent': {'windows': compositor}}))
        run.window_rects = lambda pid: driver.Run.window_rects(run, pid)
        run.elements = lambda window=None: driver.Run.elements(run, window)
        return run

    def element(self, name, x):
        bounds = {'x': x, 'y': 10, 'w': 20, 'h': 10}
        return {'id': name, 'bounds': bounds, 'visible': bounds}

    def test_elements_of_each_window_are_placed_by_its_title(self):
        compositor = [{'pid': 7, 'title': 'Keystone', 'rect': {'x': 0, 'y': 0, 'width': 800, 'height': 600}},
                      {'pid': 7, 'title': 'Keystone 2', 'rect': {'x': 900, 'y': 50, 'width': 800, 'height': 600}},
                      {'pid': 8, 'title': 'Keystone 2', 'rect': {'x': 5, 'y': 5, 'width': 1, 'height': 1}}]
        run = self.run_for(compositor)
        viewport = {'w': 800, 'h': 600}
        self.frame(run.geometry, [
            {'title': 'Keystone', 'viewport': viewport, 'elements': [self.element('composer', 100)]},
            {'title': 'Keystone 2', 'viewport': viewport, 'elements': [self.element('composer', 100),
                                                                       self.element('session-1', 0)]}])
        listed = driver.Run.elements(run)
        self.assertEqual(listed['windows'], ['Keystone', 'Keystone 2'])
        self.assertEqual([(e['id'], e['window'], e['center']['x']) for e in listed['elements']],
                         [('composer', 'Keystone', 110), ('composer', 'Keystone 2', 1010),
                          ('session-1', 'Keystone 2', 910)])
        with self.assertRaisesRegex(RuntimeError, 'pass window'):
            driver.Run.find(run, 'composer')
        self.assertEqual(driver.Run.find(run, 'composer', 'Keystone 2')['center'], {'x': 1010, 'y': 65})
        self.assertEqual(driver.Run.find(run, 'session-1')['window'], 'Keystone 2')
        self.assertEqual(driver.Run.elements(run, 'Keystone 3')['elements'], [],
                         'a closed window shows nothing')
        self.frame(run.geometry, [
            {'title': 'Keystone', 'viewport': viewport, 'elements': [self.element('composer', 100)]}])
        with self.assertRaises(driver.NotReady):
            driver.Run.elements(run, 'Keystone 2')

    def test_an_app_with_no_window_open_shows_no_elements(self):
        run = self.run_for([{'pid': 8, 'title': 'Keystone',
                             'rect': {'x': 0, 'y': 0, 'width': 800, 'height': 600}}])
        self.frame(run.geometry, [{'title': 'Keystone', 'viewport': {'w': 800, 'h': 600},
                                   'elements': [self.element('composer', 100)]}])
        self.assertEqual(driver.Run.elements(run)['elements'], [])
        self.assertEqual(driver.Run.wait_for(run, 'composer', False, 100),
                         {'id': 'composer', 'present': False})
        self.frame(run.geometry, [])
        self.assertEqual(driver.Run.elements(run, 'Keystone')['elements'], [])

    def test_a_window_still_resizing_is_not_ready(self):
        run = self.run_for([{'pid': 7, 'title': 'Keystone',
                             'rect': {'x': 0, 'y': 0, 'width': 800, 'height': 600}}])
        self.frame(run.geometry, [{'title': 'Keystone', 'viewport': {'w': 400, 'h': 600},
                                   'elements': []}])
        with self.assertRaises(driver.NotReady):
            driver.Run.elements(run)



class CaptureSizeTest(unittest.TestCase):
    def test_start_defaults_to_a_full_hd_desktop(self):
        server = driver.Server.__new__(driver.Server)
        server.run = None
        server.args = SimpleNamespace(driver='driver', binary='binary', trace_wayland=False)
        started = Mock(return_value={})
        original = driver.Run
        driver.Run = Mock(return_value=SimpleNamespace(start=started, state=Path('/tmp/state.json')))
        try:
            server.handle('start', {})
        finally:
            driver.Run = original
        self.assertEqual(started.call_args.args[:2], (1920, 1080))
        start = next(tool for tool in driver.TOOLS if tool[0] == 'start')
        self.assertEqual((start[2]['properties']['width']['default'],
                          start[2]['properties']['height']['default']), (1920, 1080))

    def test_png_size_reads_the_saved_pixels(self):
        with tempfile.NamedTemporaryFile(suffix='.png') as file:
            file.write(b'\x89PNG\r\n\x1a\n' + b'\x00\x00\x00\rIHDR'
                       + (1920).to_bytes(4, 'big') + (1080).to_bytes(4, 'big'))
            file.flush()
            self.assertEqual(driver.png_size(file.name), (1920, 1080))


class AtspiTest(unittest.TestCase):
    def test_launcher_flag_is_dropped_only_where_at_spi_rejects_it(self):
        def flag(version):
            return driver.session_options(1920, 1080, True, version).get('a11y_flag', True)
        self.assertTrue(flag(None))
        self.assertTrue(flag((2, 50, 0)))
        self.assertTrue(flag((2, 60, 5)))
        self.assertFalse(flag((2, 61, 0)))
        self.assertFalse(flag((2, 62, 1)))
        self.assertFalse(flag((3, 0, 0)))
        self.assertNotIn('a11y_flag', driver.session_options(1920, 1080, True, (2, 60, 5)))

    def test_a_driver_without_the_patch_names_the_rebuild(self):
        run = driver.Run.__new__(driver.Run)
        run.request = Mock()
        run.process = Mock()
        run.atspi_version = (2, 62, 1)
        run.call = Mock(side_effect=RuntimeError('unknown field `a11y_flag`, expected one of ...'))
        with self.assertRaisesRegex(RuntimeError, '2.62.1 aborts on --a11y=1.*setup.md'):
            run.start(1920, 1080, True)
        self.assertEqual(run.call.call_args.args[1]['a11y_flag'], False)

    def test_installed_version_is_read_from_libatspi(self):
        version = driver.atspi_version()
        if version is None:
            self.skipTest('libatspi is not installed')
        self.assertEqual(len(version), 3)
        self.assertGreaterEqual(version[:2], (2, 50))


@unittest.skipUnless(shutil.which('ffmpeg') and shutil.which('ffprobe'), 'needs ffmpeg and ffprobe')
class RecordingTest(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='keystone-recording-test-')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        for tool in ('ffmpeg', 'ffprobe', 'cat', 'sleep'):
            (self.bin / tool).symlink_to(shutil.which(tool))
        environment = patch.dict(os.environ, {'PATH': str(self.bin)})
        environment.start()
        self.addCleanup(environment.stop)
        self.run = driver.Run.__new__(driver.Run)
        self.run.evidence = self.root / 'evidence'
        self.run.evidence.mkdir()
        self.run.session = {'wayland_socket': str(self.root / 'wayland-1')}
        self.run.recording = None
        self.run.recordings = 0
        self.run.record = Mock()

    def fake(self, name, script):
        path = self.bin / name
        path.write_text(script)
        path.chmod(0o755)

    def test_grim_frames_become_an_upload_ready_mp4_scaled_into_full_hd(self):
        frame = self.root / 'frame.ppm'
        frame.write_bytes(b'P6\n2000 1125\n255\n' + bytes([40, 90, 200]) * (2000 * 1125))
        self.fake('grim', f'#!/bin/sh\n[ "$WAYLAND_DISPLAY" = "{self.root}/wayland-1" ] || exit 9\n'
                          f'[ "$*" = "-t ppm -" ] || exit 8\nsleep 0.1\nexec cat {frame}\n')
        started = driver.time.monotonic()
        self.assertEqual(self.run.record_start()['recorder'], 'grim')
        with self.assertRaisesRegex(RuntimeError, 'recording is running'):
            self.run.record_start()
        driver.time.sleep(1.5)
        result = self.run.record_stop()
        elapsed = driver.time.monotonic() - started
        self.assertEqual((result['codec'], result['pix_fmt'], result['color_space']),
                         ('h264', 'yuv420p', 'bt709'))
        self.assertEqual((result['width'], result['height'], result['fps']), (1920, 1080, 30))
        self.assertAlmostEqual(result['duration_s'], elapsed, delta=0.4)
        self.assertLess(result['captured_fps'], 11)
        self.assertEqual(driver.probe_video(result['path'])['frames'], result['frames'])
        self.assertEqual(Path(result['path']), self.run.evidence / 'recording-01.mp4')
        with self.assertRaisesRegex(RuntimeError, 'No recording'):
            self.run.record_stop()

    def test_wf_recorder_is_preferred_and_finished_by_interrupt(self):
        argv = self.root / 'argv'
        self.fake('wf-recorder', f'''#!/bin/sh
echo "$WAYLAND_DISPLAY $*" > {argv}
for last; do :; done
trap 'exec ffmpeg -v error -f lavfi -i testsrc=size=1280x720:rate=30 -t 0.5 -c:v libx264 -pix_fmt yuv420p "$last"' INT
while :; do sleep 0.05; done
''')
        self.assertEqual(self.run.record_start()['recorder'], 'wf-recorder')
        driver.time.sleep(0.5)
        result = self.run.record_stop()
        self.assertEqual((result['width'], result['height'], result['fps']), (1280, 720, 30))
        recorded = argv.read_text().split()
        self.assertEqual(recorded[0], self.run.session['wayland_socket'])
        for option in (['-D'], ['-c', 'libx264'], ['-x', 'yuv420p'], ['-r', '30'],
                       ['-F', driver.RECORDING_FIT], ['-p', 'color_range=tv'],
                       ['-f', result['path']]):
            self.assertIn(' '.join(option), ' '.join(recorded))

    def test_a_failing_grim_fails_record_start(self):
        self.fake('grim', '#!/bin/sh\nexit 1\n')
        with self.assertRaisesRegex(RuntimeError, 'grim capture failed'):
            self.run.record_start()
        self.assertIsNone(self.run.recording)

    def test_an_encoder_that_dies_is_named(self):
        frame = self.root / 'frame.ppm'
        frame.write_bytes(b'P6\n64 48\n255\n' + bytes(64 * 48 * 3))
        self.fake('grim', f'#!/bin/sh\nsleep 0.3\nexec cat {frame}\n')
        (self.bin / 'ffmpeg').unlink()
        self.fake('ffmpeg', '#!/bin/sh\nexit 3\n')
        with self.assertRaisesRegex(RuntimeError, 'ffmpeg exited with 3'):
            self.run.record_start()

    def test_missing_tools_are_named_before_recording(self):
        (self.bin / 'ffmpeg').unlink()
        self.fake('grim', '#!/bin/sh\nexit 0\n')
        with self.assertRaisesRegex(RuntimeError, 'missing ffmpeg'):
            self.run.record_start()

    def test_a_grim_that_times_out_fails_record_start(self):
        self.fake('grim', '#!/bin/sh\nexit 0\n')

        def hang(command, **options):
            driver.time.sleep(0.3)
            raise driver.subprocess.TimeoutExpired(command, options['timeout'])
        with patch.object(driver.subprocess, 'run', hang), \
                self.assertRaisesRegex(RuntimeError, 'grim capture failed.*timed out'):
            self.run.record_start()
        self.assertIsNone(self.run.recording)

    def test_stop_finishes_a_running_recording_before_the_desktop(self):
        frame = self.root / 'frame.ppm'
        frame.write_bytes(b'P6\n65 49\n255\n' + bytes([200, 30, 30]) * (65 * 49))
        self.fake('grim', f'#!/bin/sh\nexec cat {frame}\n')
        self.run.record_start()
        driver.time.sleep(0.5)
        self.run.closed = False
        self.run.process = Mock()
        self.run.log = Mock()
        self.run.stderr = Mock()
        self.run.bus = None
        self.run.scratch = self.root / 'scratch'
        self.run.scratch.mkdir()
        self.run.snapshot = Mock(return_value={'state_absent': True})
        self.run.call = Mock(side_effect=lambda *args: self.assertIsNone(self.run.recording))
        result = self.run.stop()
        self.run.call.assert_called_once_with('session_stop')
        self.assertEqual(result['errors'], [])
        self.assertTrue(Path(result['recording']['path']).exists())
        self.assertEqual((result['recording']['width'], result['recording']['height']), (64, 48))
        self.assertLess(result['recording']['captured_fps'], 35)

    def test_upload_limits(self):
        good = {'codec': 'h264', 'pix_fmt': 'yuv420p', 'width': 1920, 'height': 1080, 'fps': 30,
                'size_bytes': 100_000_000}
        self.assertEqual(driver.upload_problems(good), [])
        self.assertEqual(len(driver.upload_problems({**good, 'pix_fmt': 'yuv444p'})), 1)
        self.assertEqual(len(driver.upload_problems({**good, 'width': 2560})), 1)
        self.assertEqual(len(driver.upload_problems({**good, 'fps': 120})), 1)
        self.assertEqual(len(driver.upload_problems({**good, 'fps': 15})), 1)
        self.assertEqual(len(driver.upload_problems({**good, 'size_bytes': 100_000_001})), 1)


if __name__ == '__main__':
    unittest.main()

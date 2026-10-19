#!/usr/bin/env python3
"""Focused wrapper regression tests: python3 scripts/test_native_driver.py."""
import importlib.util
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import Mock

spec = importlib.util.spec_from_file_location('native_driver', Path(__file__).with_name('native-driver.py'))
driver = importlib.util.module_from_spec(spec)
spec.loader.exec_module(driver)


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

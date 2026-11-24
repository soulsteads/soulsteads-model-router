"""Exercise the notice generator used by the release build with local fixtures."""

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest


WORKFLOW = Path(__file__).resolve().parents[1] / '.github/workflows/release.yml'


@unittest.skipUnless(WORKFLOW.is_file(), 'requires the source checkout release workflow')
class ReleaseNoticeTests(unittest.TestCase):
    def setUp(self):
        root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.source = root / 'fixture-crate'
        self.source.mkdir()
        self.bundle = root / 'bundle'
        self.bundle.mkdir()
        self.metadata = root / 'metadata.json'
        self.metadata.write_text(json.dumps({
            'resolve': {'root': 'router', 'nodes': [
                {'id': 'router', 'deps': [{'pkg': 'dependency', 'dep_kinds': [{'kind': None}]}]},
                {'id': 'dependency', 'deps': []},
            ]},
            'packages': [{'id': 'dependency', 'name': 'fixture-crate', 'version': '1.0.0',
                          'manifest_path': str(self.source / 'Cargo.toml'),
                          'license': 'Apache-2.0', 'license_file': None}],
        }), encoding='utf-8')
        workflow = WORKFLOW.read_text(encoding='utf-8')
        self.script = textwrap.dedent(workflow.split("<<'PY'\n", 1)[1].split('\n          PY\n', 1)[0])
        self.notices = self.bundle / 'dependency-notices'

    def generate(self):
        return subprocess.run([sys.executable, '-B', '-c', self.script,
                               self.metadata, self.bundle], capture_output=True, text=True, timeout=10)

    def assert_missing_notice_fails(self):
        result = self.generate()
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn('No nonempty license or notice files for fixture-crate-1.0.0', result.stderr)
        self.assertFalse((self.notices / 'INDEX.tsv').exists())

    def test_spdx_declaration_without_notice_fails(self):
        self.assert_missing_notice_fails()

    def test_whitespace_only_notice_fails(self):
        (self.source / 'LICENSE').write_bytes(b' \n\t')
        self.assert_missing_notice_fails()

    def test_notice_is_indexed_and_preserved_byte_for_byte(self):
        notice = b'Synthetic notice.\r\n'
        (self.source / 'LICENSE').write_bytes(notice)
        (self.source / 'NOTICE').touch()
        result = self.generate()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.notices / 'fixture-crate-1.0.0' / 'LICENSE').read_bytes(), notice)
        self.assertIn('fixture-crate-1.0.0\tApache-2.0\tLICENSE\n',
                      (self.notices / 'INDEX.tsv').read_text())

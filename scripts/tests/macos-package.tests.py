#!/usr/bin/env python3
"""Release verification rejects missing timestamp/runtime and wrong publishers."""
import importlib.util
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location('package', Path(__file__).parents[1] / 'package-portal-macos.py')
package = importlib.util.module_from_spec(spec)
spec.loader.exec_module(package)


class SignatureTests(unittest.TestCase):
    def verify(self, details):
        with patch.object(package, 'run', side_effect=['', details, 'designated => release requirement']) as run:
            result = package.verify_release_signature(Path('artifact'))
            self.assertEqual(run.call_args_list[0].args[0],
                             ['/usr/bin/codesign', '--verify', '--strict', '-R', package.REQUIREMENT, Path('artifact')])
            return result

    def test_timestamped_developer_id_signature(self):
        result = self.verify('CodeDirectory v=20500 flags=0x10000(runtime)\nTimestamp=Sep 9, 2026 at 12:00:00\n')
        self.assertIn('2026', result['secure_timestamp'])

    def test_missing_timestamp_and_runtime_are_rejected(self):
        for details in ['CodeDirectory v=20500 flags=0x10000(runtime)\n',
                        'CodeDirectory v=20500 flags=0x10000(runtime)\nTimestamp=none\n',
                        'CodeDirectory v=20500 flags=0x0(none)\nTimestamp=Sep 9, 2026\n']:
            with self.assertRaises(RuntimeError):
                self.verify(details)

    def test_untrusted_signature_is_not_accepted_by_metadata(self):
        with patch.object(package, 'run', side_effect=RuntimeError('wrong publisher')):
            with self.assertRaisesRegex(RuntimeError, 'wrong publisher'):
                package.verify_release_signature(Path('untrusted'))


if __name__ == '__main__':
    unittest.main(verbosity=2)

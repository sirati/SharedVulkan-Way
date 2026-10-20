#!/usr/bin/env python3
"""Diagnostic parser checks: modifiers, plane identity and ambiguity."""
import importlib.util
from pathlib import Path
import unittest
import sys
sys.dont_write_bytecode = True

spec = importlib.util.spec_from_file_location("bar_audit", Path(__file__).with_name("sv-linux-bar-audit.py"))
audit = importlib.util.module_from_spec(spec)
spec.loader.exec_module(audit)


class ImportReceipts(unittest.TestCase):
    def test_real_tuple_preserves_nonzero_modifier_and_both_planes(self):
        result = audit.decode_protocol([
            '[2.0] zwp_linux_buffer_params_v1@31.add(fd 58, 0, 0, 2048, 16777216, 2)',
            '[2.1] zwp_linux_buffer_params_v1@31.add(fd 59, 1, 1474560, 2048, 16777216, 2)',
            '[2.2] zwp_linux_buffer_params_v1@31.create_immed(new id wl_buffer@42, 1280, 720, 842094158, 0)',
        ])
        self.assertTrue(result['complete_import_metadata'])
        event = result['imports'][0]
        self.assertEqual(event['fourcc'], 'NV12')
        self.assertEqual(event['planes'][1]['offset'], 1474560)
        self.assertEqual(event['planes'][1]['modifier'], '0x0100000000000002')
        self.assertEqual(event['planes'][1]['received_fd'], 59)
        self.assertTrue(result['requires_native_fd_correlation'])

    def test_client_local_id_collision_cannot_be_claimed_complete(self):
        result = audit.decode_protocol([
            'zwp_linux_buffer_params_v1#7.add(4, 0, 0, 3200, 0, 0)',
            'zwp_linux_buffer_params_v1#7.add(8, 0, 0, 3200, 0, 0)',
            'zwp_linux_buffer_params_v1#7.create(800, 600, 875713089, 0)',
        ])
        self.assertFalse(result['complete_import_metadata'])
        self.assertEqual(result['imports'][0]['fourcc'], 'AR24')
        self.assertEqual(len(result['imports'][0]['planes']), 2)

    def test_truncated_or_invalid_fields_do_not_become_linear_defaults(self):
        for add in ['add(fd 4, 0, 0, 3200, 0)', 'add(fd 4, 0, 0, 3200, -1, 0)']:
            result = audit.decode_protocol([
                'zwp_linux_buffer_params_v1@2.' + add,
                'zwp_linux_buffer_params_v1@2.create(800, 600, 875713089, 0)',
            ])
            self.assertFalse(result['complete_import_metadata'])
            self.assertTrue(result['parse_errors'])

    def test_no_requests_is_not_an_import_proof(self):
        self.assertFalse(audit.decode_protocol(['Error during import'])['complete_import_metadata'])


if __name__ == '__main__':
    unittest.main()

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


class NativeReceipts(unittest.TestCase):
    def test_resumed_error_retains_original_fd_and_errno(self):
        result = audit.decode_native([
            '33 1792512000.100 ioctl(47</dev/dri/renderD190>, DRM_IOCTL_PRIME_FD_TO_HANDLE, 0x123 <unfinished ...>',
            '44 1792512000.101 mmap(NULL, 4096, PROT_READ, MAP_SHARED, 9, 0) = 0x10000',
            '33 1792512000.102 <... ioctl resumed>) = -1 EINVAL (Invalid argument)',
        ])
        error = result['native_syscall_errors'][0]
        self.assertEqual(error['errno'], 'EINVAL')
        self.assertIn('47</dev/dri/renderD190>', error['unfinished_entry']['raw'])
        self.assertEqual(error['unfinished_entry']['line'], 1)
        self.assertFalse(result['unfinished_native_calls'])

    def test_send_rights_are_not_reported_as_received_rights(self):
        result = audit.decode_native([
            '33 1792512000.100 sendmsg(4, {msg_control=[{cmsg_type=SCM_RIGHTS, cmsg_data=[8]}]}, 0) = 24',
            '33 1792512000.101 recvmsg(4, {msg_control=[{cmsg_type=SCM_RIGHTS, cmsg_data=[19</dev/shm/object>]}]}, 0) = 24',
        ])
        self.assertEqual(len(result['sent_descriptor_records']), 1)
        self.assertEqual(len(result['received_descriptor_records']), 1)
        self.assertIn('19</dev/shm/object>', result['received_descriptor_records'][0]['raw'])


if __name__ == '__main__':
    unittest.main()

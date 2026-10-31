#!/usr/bin/env python3
"""Run with: python3 deploy/mac/test_antigravity_push.py"""
import json
import unittest

import antigravity_push as push

REPORT = {
    'conversation_id': '',
    'status': 'SUCCESS',
    'num_turns': 0,
    'command': {'name': 'usage', 'data': {'groups': [
        {'name': 'Gemini Models', 'buckets': [
            {'id': 'gemini-weekly', 'window': 'weekly', 'remaining_fraction': 0.99, 'reset_time': '2026-11-04T18:28:16Z'},
            {'id': 'gemini-5h', 'window': '5h', 'remaining_fraction': 0.998, 'reset_time': '2026-10-31T23:05:11Z'},
        ]},
        {'name': 'Claude and GPT models', 'buckets': [
            {'id': '3p-weekly', 'window': 'weekly', 'remaining_fraction': 1, 'reset_time': '2026-11-07T18:26:10Z'},
            {'id': '3p-5h', 'window': '5h', 'remaining_fraction': 1, 'reset_time': '2026-10-31T23:26:10Z'},
        ]},
        {'name': 'Gemini 4 Argon', 'buckets': [
            {'id': 'gemini-argon-wtu-weekly', 'window': 'weekly', 'remaining_fraction': 0.26, 'reset_time': '2026-11-04T18:25:35Z'},
        ]},
    ]}},
}


class ParseReport(unittest.TestCase):
    def test_maps_every_current_bucket(self):
        windows = {w['windowType']: w for w in push.parse_report(json.dumps(REPORT))}
        self.assertEqual(sorted(windows), [
            'hourly5_claudegpt', 'hourly5_gemini', 'weekly_argon', 'weekly_claudegpt', 'weekly_gemini',
        ])
        argon = windows['weekly_argon']
        self.assertEqual(argon['usedPercent'], 74.0)
        self.assertEqual(argon['windowSeconds'], 604_800)
        self.assertEqual(argon['resetAt'], 1793816735)
        self.assertEqual(windows['hourly5_gemini']['windowSeconds'], 18_000)

    def test_absent_fraction_means_exhausted(self):
        report = json.loads(json.dumps(REPORT))
        del report['command']['data']['groups'][0]['buckets'][0]['remaining_fraction']
        windows = {w['windowType']: w for w in push.parse_report(json.dumps(report))}
        self.assertEqual(windows['weekly_gemini']['usedPercent'], 100.0)

    def test_new_bucket_still_appears(self):
        report = json.loads(json.dumps(REPORT))
        report['command']['data']['groups'][0]['buckets'].append(
            {'id': 'nova-weekly', 'window': 'weekly', 'remaining_fraction': 0.5, 'reset_time': '2026-11-04T00:00:00Z'})
        names = [w['windowType'] for w in push.parse_report(json.dumps(report))]
        self.assertIn('weekly_nova', names)

    def test_skips_unknown_window_lengths(self):
        report = json.loads(json.dumps(REPORT))
        report['command']['data']['groups'][0]['buckets'].append(
            {'id': 'gemini-daily', 'window': 'daily', 'remaining_fraction': 0.5})
        self.assertEqual(len(push.parse_report(json.dumps(report))), 5)

    def test_reset_time_with_any_fraction(self):
        self.assertEqual(push.parse_reset('2026-11-04T18:28:16.123456789Z'), 1793816896)
        self.assertEqual(push.parse_reset('2026-11-04T18:28:16.5Z'), 1793816896)

    def test_malformed_bucket_is_reported_not_raised(self):
        report = json.loads(json.dumps(REPORT))
        report['command']['data']['groups'][0]['buckets'][0]['reset_time'] = 'soon'
        with self.assertRaises(push.AgyError) as caught:
            push.parse_report(json.dumps(report))
        self.assertEqual(caught.exception.kind, 'invalid')

    def test_signed_out_report_is_classified_without_its_text(self):
        report = {'status': 'ERROR', 'response': 'Please sign in as someone@example.com'}
        with self.assertRaises(push.AgyError) as caught:
            push.parse_report(json.dumps(report))
        self.assertEqual(caught.exception.kind, 'unauthorized')
        self.assertNotIn('example.com', caught.exception.message)

    def test_other_failures(self):
        with self.assertRaises(push.AgyError) as caught:
            push.parse_report('not json')
        self.assertEqual(caught.exception.kind, 'invalid')
        with self.assertRaises(push.AgyError) as caught:
            push.parse_report(json.dumps({'status': 'SUCCESS', 'command': {'name': 'usage', 'data': {'groups': []}}}))
        self.assertEqual(caught.exception.kind, 'invalid')


if __name__ == '__main__':
    unittest.main()

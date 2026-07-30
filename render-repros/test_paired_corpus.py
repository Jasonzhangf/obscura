#!/usr/bin/env python3

import importlib.util
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("paired-corpus.py")
SPEC = importlib.util.spec_from_file_location("paired_corpus", SCRIPT)
paired_corpus = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(paired_corpus)


class MediaEnvironmentTests(unittest.TestCase):
    def test_canonical_light_default_motion_environment_matches(self):
        self.assertTrue(
            paired_corpus.media_matches_configured(
                dict(paired_corpus.EXPECTED_MEDIA_MATCHES)
            )
        )

    def test_dark_or_reduced_environment_is_rejected(self):
        dark = dict(paired_corpus.EXPECTED_MEDIA_MATCHES)
        dark["prefers_color_scheme_light"] = False
        dark["prefers_color_scheme_dark"] = True
        self.assertFalse(paired_corpus.media_matches_configured(dark))

        reduced = dict(paired_corpus.EXPECTED_MEDIA_MATCHES)
        reduced["prefers_reduced_motion_no_preference"] = False
        reduced["prefers_reduced_motion_reduce"] = True
        self.assertFalse(paired_corpus.media_matches_configured(reduced))


class ControlledScrollTests(unittest.TestCase):
    def test_bottom_expression_resolves_live_document_height(self):
        expression = paired_corpus.scroll_eval_expression((12, "bottom"))
        self.assertIn("const requestedX=12", expression)
        self.assertIn(
            "requestedY=document.documentElement.scrollHeight", expression
        )
        self.assertIn("window.scrollTo(requestedX,requestedY)", expression)

    def test_capture_report_uses_post_settle_state(self):
        stdout = (
            "diagnostic\n"
            '{"evaluation":"{\\"requested\\":{\\"x\\":0,\\"y\\":2702}}",'
            '"captureState":{"scrollX":0,"scrollY":1802,'
            '"innerWidth":1280,"innerHeight":900,'
            '"scrollWidth":1291,"scrollHeight":2702}}\n'
        )
        state = paired_corpus.parse_obscura_scroll_report(stdout)
        self.assertEqual(state["requested"], {"x": 0, "y": 2702})
        self.assertEqual(state["actual"], {"x": 0, "y": 1802})
        self.assertEqual(state["content"]["height"], 2702)
        self.assertEqual(state["sampled_phase"], "immediately-before-screenshot")

    def test_scroll_y_parser_accepts_bottom_and_integer(self):
        self.assertEqual(paired_corpus.parse_scroll_y("bottom"), "bottom")
        self.assertEqual(paired_corpus.parse_scroll_y("-20"), -20)


if __name__ == "__main__":
    unittest.main()

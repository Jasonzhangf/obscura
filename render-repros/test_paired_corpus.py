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

    def test_every_capture_expression_samples_page_state_without_scrolling(self):
        expression = paired_corpus.obscura_state_eval_expression(None)
        self.assertIn("outer_html_fnv1a32", expression)
        self.assertIn("visible_text_fnv1a32", expression)
        self.assertNotIn("window.scrollTo", expression)

    def test_capture_report_keeps_dom_state_and_authoritative_geometry(self):
        stdout = (
            '{"evaluation":"{\\"document\\":{\\"ready_state\\":\\"complete\\",'
            '\\"element_count\\":7,\\"outer_html_fnv1a32\\":\\"12345678\\"},'
            '\\"geometry\\":{\\"document_scroll_height\\":999}}",'
            '"captureState":{"scrollX":3,"scrollY":40,'
            '"innerWidth":640,"innerHeight":480,'
            '"scrollWidth":650,"scrollHeight":1200}}\n'
        )
        state = paired_corpus.parse_obscura_capture_report(stdout)
        self.assertEqual(state["document"]["element_count"], 7)
        self.assertEqual(state["geometry"]["scroll_y"], 40)
        self.assertEqual(state["geometry"]["document_scroll_height"], 1200)

    def test_page_state_comparison_reports_provenance_and_geometry_deltas(self):
        obscura = {
            "document": {
                "ready_state": "complete",
                "element_count": 9,
                "outer_html_utf16": 100,
                "visible_text_utf16": 30,
                "outer_html_fnv1a32": "aaaaaaaa",
                "visible_text_fnv1a32": "bbbbbbbb",
            },
            "geometry": {"document_scroll_height": 1200, "scroll_y": 300},
        }
        chromium = {
            "document": {
                "ready_state": "complete",
                "element_count": 7,
                "outer_html_utf16": 95,
                "visible_text_utf16": 30,
                "outer_html_fnv1a32": "cccccccc",
                "visible_text_fnv1a32": "bbbbbbbb",
            },
            "geometry": {"document_scroll_height": 1000, "scroll_y": 250},
        }
        comparison = paired_corpus.compare_page_states(obscura, chromium)
        self.assertTrue(comparison["ready_state_equal"])
        self.assertEqual(comparison["element_count_delta"], 2)
        self.assertFalse(comparison["outer_html_fingerprint_equal"])
        self.assertTrue(comparison["visible_text_fingerprint_equal"])
        self.assertEqual(
            comparison["geometry_delta"]["document_scroll_height"], 200
        )
        self.assertEqual(comparison["geometry_delta"]["scroll_y"], 50)

    def test_scroll_y_parser_accepts_bottom_and_integer(self):
        self.assertEqual(paired_corpus.parse_scroll_y("bottom"), "bottom")
        self.assertEqual(paired_corpus.parse_scroll_y("-20"), -20)


if __name__ == "__main__":
    unittest.main()

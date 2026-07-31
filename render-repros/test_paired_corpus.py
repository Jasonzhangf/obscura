#!/usr/bin/env python3

import importlib.util
import json
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
        self.assertIn("data-obscura-external-stylesheets", expression)
        self.assertIn("normalized_outer_html_fnv1a32", expression)
        self.assertIn("visible_text_fnv1a32", expression)
        self.assertIn("injectedStyles.reduce", expression)
        self.assertNotIn("cloneNode", expression)
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
                "normalized_outer_html_utf16": 90,
                "visible_text_utf16": 30,
                "outer_html_fnv1a32": "aaaaaaaa",
                "normalized_outer_html_fnv1a32": "dddddddd",
                "visible_text_fnv1a32": "bbbbbbbb",
            },
            "geometry": {"document_scroll_height": 1200, "scroll_y": 300},
        }
        chromium = {
            "document": {
                "ready_state": "complete",
                "element_count": 7,
                "outer_html_utf16": 95,
                "normalized_outer_html_utf16": 90,
                "visible_text_utf16": 30,
                "outer_html_fnv1a32": "cccccccc",
                "normalized_outer_html_fnv1a32": "dddddddd",
                "visible_text_fnv1a32": "bbbbbbbb",
            },
            "geometry": {"document_scroll_height": 1000, "scroll_y": 250},
        }
        comparison = paired_corpus.compare_page_states(obscura, chromium)
        self.assertTrue(comparison["ready_state_equal"])
        self.assertEqual(comparison["element_count_delta"], 2)
        self.assertFalse(comparison["outer_html_fingerprint_equal"])
        self.assertTrue(comparison["normalized_outer_html_fingerprint_equal"])
        self.assertEqual(comparison["normalized_outer_html_utf16_delta"], 0)
        self.assertTrue(comparison["visible_text_fingerprint_equal"])
        self.assertEqual(
            comparison["geometry_delta"]["document_scroll_height"], 200
        )
        self.assertEqual(comparison["geometry_delta"]["scroll_y"], 50)

    def test_scroll_y_parser_accepts_bottom_and_integer(self):
        self.assertEqual(paired_corpus.parse_scroll_y("bottom"), "bottom")
        self.assertEqual(paired_corpus.parse_scroll_y("-20"), -20)


class GeometryProbeTests(unittest.TestCase):
    @staticmethod
    def chromium_state():
        return {
            "document": {
                "outer_html_sha256": "outer",
                "visible_text_sha256": "text",
            },
            "geometry_probes": [],
        }

    class FakePage:
        def __init__(self, state):
            self.state = state
            self.calls = []

        def evaluate(self, expression, *args):
            self.calls.append((expression, args))
            return self.state

    def test_default_state_expressions_do_not_include_probe_work(self):
        obscura_expression = paired_corpus.obscura_state_eval_expression(None)
        self.assertNotIn("sampleGeometrySelector", obscura_expression)
        self.assertNotIn("geometry_probes", obscura_expression)

        page = self.FakePage(self.chromium_state())
        paired_corpus.capture_chromium_state(page)
        self.assertEqual(len(page.calls), 1)
        expression, args = page.calls[0]
        self.assertEqual(args, ())
        self.assertNotIn("sampleGeometrySelector", expression)
        self.assertNotIn("geometry_probes", expression)

    def test_repeatable_selectors_are_passed_safely_in_one_state_expression(self):
        selectors = ["header nav a", '[data-label="a\\\"b"]', "["]
        obscura_expression = paired_corpus.obscura_state_eval_expression(
            (0, 25), selectors
        )
        encoded = json.dumps(selectors, ensure_ascii=True, separators=(",", ":"))
        self.assertIn(encoded, obscura_expression)
        self.assertIn("sampleGeometrySelector", obscura_expression)
        self.assertIn("catch(error)", obscura_expression)
        self.assertIn("geometry_probes:geometryProbes", obscura_expression)

        page = self.FakePage(self.chromium_state())
        paired_corpus.capture_chromium_state(page, selectors)
        self.assertEqual(len(page.calls), 1)
        expression, args = page.calls[0]
        self.assertEqual(args, (selectors,))
        self.assertIn("querySelectorAll(selector)", expression)
        self.assertIn("catch(error)", expression)
        self.assertIn("geometry_probes: geometryProbes", expression)

    def test_probe_comparison_reports_raw_deltas_and_invalid_errors(self):
        obscura = {
            "geometry_probes": [
                {
                    "selector": ".card",
                    "valid": True,
                    "count": 2,
                    "rects": [
                        {
                            "x": 11,
                            "y": 18,
                            "width": 100,
                            "height": 40,
                            "visible": True,
                        }
                    ],
                    "error": None,
                },
                {
                    "selector": "[",
                    "valid": False,
                    "count": None,
                    "rects": [],
                    "error": {"name": "SyntaxError", "message": "invalid selector"},
                },
            ]
        }
        chromium = {
            "geometry_probes": [
                {
                    "selector": ".card",
                    "valid": True,
                    "count": 1,
                    "rects": [
                        {
                            "x": 10,
                            "y": 20,
                            "width": 98,
                            "height": 40,
                            "visible": False,
                        }
                    ],
                    "error": None,
                },
                {
                    "selector": "[",
                    "valid": False,
                    "count": None,
                    "rects": [],
                    "error": {"name": "SyntaxError", "message": "invalid selector"},
                },
            ]
        }

        comparison = paired_corpus.compare_geometry_probes(obscura, chromium)
        self.assertEqual(comparison[0]["counts"]["delta"], 1)
        self.assertEqual(
            comparison[0]["rect_deltas"][0]["delta"],
            {"x": 1, "y": -2, "width": 2, "height": 0},
        )
        self.assertEqual(
            comparison[0]["rect_deltas"][0]["visibility"],
            {"obscura": True, "chromium": False},
        )
        self.assertEqual(comparison[1]["valid"], {"obscura": False, "chromium": False})
        self.assertIsNone(comparison[1]["counts"]["delta"])
        self.assertEqual(comparison[1]["rects_compared"], 0)


if __name__ == "__main__":
    unittest.main()

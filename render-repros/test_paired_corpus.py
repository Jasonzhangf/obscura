#!/usr/bin/env python3

import hashlib
import importlib.util
import json
import unittest
from pathlib import Path
from unittest import mock


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
        self.assertIn("preInitialActual", expression)
        self.assertIn("postInitialActual", expression)
        self.assertNotIn("behavior:'instant'", expression)

    def test_chromium_reassert_records_settled_and_final_offsets(self):
        class FakePage:
            def __init__(self):
                self.calls = []

            def evaluate(self, expression, argument):
                self.calls.append((expression, argument))
                return {
                    "requested": {"x": 4, "y": 300},
                    "pre_reassert_actual": {"x": 4, "y": 287},
                    "final_actual": {"x": 4, "y": 300},
                    "reassert_behavior": "instant",
                }

        page = FakePage()
        result = paired_corpus.reassert_chromium_controlled_scroll(
            page, (4, 300)
        )
        self.assertEqual(result["pre_reassert_actual"]["y"], 287)
        self.assertEqual(result["final_actual"]["y"], 300)
        expression, argument = page.calls[0]
        self.assertEqual(argument, [4, 300])
        self.assertIn('behavior: "instant"', expression)
        self.assertLess(
            expression.index("beforeReassert"),
            expression.index("window.scrollTo({"),
        )

    def test_capture_report_uses_post_settle_state(self):
        stdout = (
            "diagnostic\n"
            '{"evaluation":"{\\"sampled_phase\\":'
            '\\"capture-boundary-before-screenshot\\",'
            '\\"requested\\":null,\\"controlled_scroll\\":null}",'
            '"controlledScroll":{"requested":{"x":0,"y":2702},'
            '"preInitialActual":{"x":0,"y":0},'
            '"postInitialActual":{"x":0,"y":12},'
            '"initialBehavior":"authored",'
            '"initialPhase":"before-controlled-scroll-settle",'
            '"preReassertActual":{"x":0,"y":1791},'
            '"finalReassertActual":{"x":0,"y":1802},'
            '"behavior":"instant",'
            '"phase":"immediately-before-capture-state-and-screenshot"},'
            '"captureState":{"scrollX":0,"scrollY":1802,'
            '"innerWidth":1280,"innerHeight":900,'
            '"scrollWidth":1291,"scrollHeight":2702}}\n'
        )
        state = paired_corpus.parse_obscura_scroll_report(stdout)
        self.assertEqual(state["requested"], {"x": 0, "y": 2702})
        self.assertEqual(
            state["pre_reassert_actual"], {"x": 0, "y": 1791}
        )
        self.assertEqual(state["pre_initial_actual"], {"x": 0, "y": 0})
        self.assertEqual(state["post_initial_actual"], {"x": 0, "y": 12})
        self.assertEqual(
            state["final_reassert_actual"], {"x": 0, "y": 1802}
        )
        self.assertEqual(state["actual"], {"x": 0, "y": 1802})
        self.assertEqual(state["reassert_behavior"], "instant")
        self.assertEqual(state["content"]["height"], 2702)
        self.assertEqual(
            state["sampled_phase"], paired_corpus.CAPTURE_BOUNDARY_PHASE
        )

    def test_obscura_capture_exports_final_scroll_request_to_cli(self):
        environment = paired_corpus.obscura_environment(1280, 720)
        paired_corpus.with_controlled_scroll_environment(
            environment, (12, "bottom")
        )
        self.assertEqual(environment["OBSCURA_SHOT_SCROLL_X"], "12")
        self.assertEqual(environment["OBSCURA_SHOT_SCROLL_Y"], "bottom")
        self.assertEqual(environment["OBSCURA_SHOT_EVAL_AT_CAPTURE"], "1")
        self.assertEqual(environment["OBSCURA_SHOT_RESOURCE_WARMUP"], "1")

    def test_paired_state_expression_is_read_only_at_capture_boundary(self):
        expression = paired_corpus.obscura_state_eval_expression(
            None,
            [".card"],
            sampled_phase=paired_corpus.CAPTURE_BOUNDARY_PHASE,
        )
        self.assertIn(
            f'sampled_phase:"{paired_corpus.CAPTURE_BOUNDARY_PHASE}"',
            expression,
        )
        self.assertIn("geometry_probes:geometryProbes", expression)
        self.assertNotIn("window.scrollTo", expression)

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
            '{"evaluation":"{\\"sampled_phase\\":'
            '\\"capture-boundary-before-screenshot\\",'
            '\\"document\\":{\\"ready_state\\":\\"complete\\",'
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
        self.assertTrue(state["state_and_screenshot_share_capture_boundary"])

    def test_legacy_pre_settle_report_is_not_relabelled_as_capture_state(self):
        stdout = (
            '{"evaluation":"{\\"sampled_phase\\":'
            '\\"before-cli-post-eval-settle\\",'
            '\\"document\\":{},\\"geometry\\":{}}",'
            '"captureState":{"scrollX":0,"scrollY":0,'
            '"innerWidth":640,"innerHeight":480,'
            '"scrollWidth":640,"scrollHeight":480}}\n'
        )
        state = paired_corpus.parse_obscura_capture_report(stdout)
        self.assertEqual(
            state["sampled_phase"], "before-cli-post-eval-settle"
        )
        self.assertEqual(
            state["screenshot_sampled_phase"],
            paired_corpus.CAPTURE_BOUNDARY_PHASE,
        )
        self.assertFalse(state["state_and_screenshot_share_capture_boundary"])

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
        self.assertNotIn("async ", expression)
        self.assertNotIn("await ", expression)
        self.assertNotIn("crypto.subtle", expression)
        self.assertIn(
            f'sampled_phase: "{paired_corpus.CAPTURE_BOUNDARY_PHASE}"',
            expression,
        )

    def test_chromium_snapshot_paints_before_host_hashing(self):
        events = []
        first_state = {
            "_hash_sources": {
                "dom": "<html></html>",
                "normalized_dom": "<html></html>",
                "visible_text": "hello",
            },
            "document": {},
        }
        second_state = {
            "_hash_sources": {
                "dom": "<html></html>",
                "normalized_dom": "<html></html>",
                "visible_text": "hello",
            },
            "document": {},
        }

        class OrderedPage:
            def __init__(self):
                self.states = [first_state, second_state]

            def evaluate(self, expression, *args):
                events.append("evaluate")
                return self.states.pop(0)

            def screenshot(self, **kwargs):
                events.append("screenshot")

        real_sha256 = hashlib.sha256

        def ordered_sha256(value):
            events.append("sha256")
            return real_sha256(value)

        with mock.patch.object(
            paired_corpus.hashlib,
            "sha256",
            side_effect=ordered_sha256,
        ):
            captured, boundary = paired_corpus.capture_chromium_image(
                OrderedPage(), Path("/tmp/not-written.png")
            )

        self.assertEqual(
            events,
            ["evaluate", "screenshot", "evaluate", "sha256", "sha256", "sha256"],
        )
        self.assertTrue(boundary["stable"])
        self.assertNotIn("_hash_sources", captured)
        self.assertEqual(
            captured["document"]["outer_html_sha256"],
            hashlib.sha256(b"<html></html>").hexdigest(),
        )
        self.assertEqual(
            captured["document"]["normalized_outer_html_sha256"],
            hashlib.sha256(b"<html></html>").hexdigest(),
        )
        self.assertEqual(
            captured["document"]["visible_text_sha256"],
            hashlib.sha256(b"hello").hexdigest(),
        )

    def test_chromium_resource_warmup_discards_one_shot_then_yields(self):
        events = []

        class WarmupPage:
            def screenshot(self, **kwargs):
                events.append(("screenshot", kwargs))
                return b"discarded"

            def wait_for_timeout(self, timeout):
                events.append(("wait", timeout))

        report = paired_corpus.warm_chromium_capture(WarmupPage())
        self.assertEqual([event[0] for event in events], ["screenshot", "wait"])
        self.assertEqual(events[1], ("wait", 1))
        self.assertNotIn("path", events[0][1])
        self.assertEqual(report["discardedShots"], 1)
        self.assertEqual(
            report["phase"], paired_corpus.RESOURCE_WARMUP_PHASE
        )

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
        self.assertIn("font_family:style.fontFamily", expression)
        self.assertIn("line_height:style.lineHeight", expression)
        self.assertIn("white_space:style.whiteSpace", expression)
        self.assertIn(
            "grid_template_columns:style.gridTemplateColumns", expression
        )
        self.assertIn("border_left_style:style.borderLeftStyle", expression)
        self.assertIn("object_fit:style.objectFit", expression)
        self.assertIn("content_visibility:style.contentVisibility", expression)
        self.assertIn("geometry_probes: geometryProbes", expression)
        self.assertIn(
            f'sampled_phase: "{paired_corpus.CAPTURE_BOUNDARY_PHASE}"',
            expression,
        )

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
                            "computed": {
                                "display": "grid",
                                "width": "100px",
                                "align_items": "stretch",
                            },
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
                            "computed": {
                                "display": "grid",
                                "width": "98px",
                                "align_items": "normal",
                            },
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
        self.assertEqual(
            comparison[0]["rect_deltas"][0]["computed_difference_count"], 2
        )
        self.assertEqual(
            comparison[0]["rect_deltas"][0]["computed_differences"],
            {
                "align_items": {"obscura": "stretch", "chromium": "normal"},
                "width": {"obscura": "100px", "chromium": "98px"},
            },
        )
        self.assertEqual(comparison[1]["valid"], {"obscura": False, "chromium": False})
        self.assertIsNone(comparison[1]["counts"]["delta"])
        self.assertEqual(comparison[1]["rects_compared"], 0)


class AnimationSamplingTests(unittest.TestCase):
    class FakePage:
        def __init__(self):
            self.calls = []

        def evaluate(self, expression, *args):
            self.calls.append((expression, args))
            return {
                "supported": True,
                "requested_ms": args[0],
                "discovered": 3,
                "frozen": 3,
                "failures": [],
            }

    def test_explicit_sample_pauses_and_seeks_current_animations(self):
        page = self.FakePage()
        result = paired_corpus.freeze_chromium_animations(page, 0)
        self.assertEqual(result["requested_ms"], 0)
        self.assertEqual(len(page.calls), 1)
        expression, args = page.calls[0]
        self.assertEqual(args, (0,))
        self.assertIn("document.getAnimations()", expression)
        self.assertIn("animation.pause()", expression)
        self.assertIn("animation.currentTime = sampleMs", expression)
        self.assertIn("getBoundingClientRect", expression)


if __name__ == "__main__":
    unittest.main()

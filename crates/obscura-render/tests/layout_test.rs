//! Regression test: a Hacker-News-shaped nested `<table>` layout must come out
//! right using only the general engine (UA defaults + real CSS cascade), with
//! no per-site hardcoded selectors. This guards against reintroducing the
//! site-specific hacks that used to live in obscura-render for this exact markup.

use obscura_dom::tree_sink::parse_html;
use obscura_render::{layout_dom, layout_dom_with_images};
use std::collections::HashMap;

const HN_HTML: &str = r##"
    <table border="0" cellpadding="0" cellspacing="0" width="85%" bgcolor="#f6f6ef">
        <tr>
            <td bgcolor="#ff6600">
                <table border="0" cellpadding="0" cellspacing="0" width="100%" style="padding:2px">
                    <tr>
                        <td style="width:18px;padding-right:4px"><a href="https://news.ycombinator.com"><img src="y18.svg" width="18" height="18"></a></td>
                        <td style="line-height:12pt; height:10px;"><span class="pagetop"><b class="hnname"><a href="news">Hacker News</a></b> <a href="newest">new</a> | <a href="front">past</a></span></td>
                        <td style="text-align:right;padding-right:4px;"><span class="pagetop"><a href="login?goto=news">login</a></span></td>
                    </tr>
                </table>
            </td>
        </tr>
        <tr>
            <td>
                <table border="0" cellpadding="0" cellspacing="0">
                    <tr class="athing submission" id="48761229">
                        <td align="right" valign="top" class="title"><span class="rank">24.</span></td>
                        <td valign="top" class="votelinks"><center><a id="up_48761229" href="vote?id=48761229"><div class="votearrow" title="upvote"></div></a></center></td>
                        <td class="title"><span class="titleline"><a href="https://www.zachtronics.com/exapunks/">Exapunks (2018)</a></span></td>
                    </tr>
                </table>
            </td>
        </tr>
    </table>
"##;

/// Top-left of the tightest laid-out element box whose text contains
/// `needle`. Text geometry is no longer a per-word list: a pure-text
/// container collapses to a single cosmic-text inline formatting context
/// (see `inline`), and even in the word-split path the wrapping `<a>`/`<span>`
/// are flattened into their block, so the smallest element rect enclosing the
/// text is the meaningful, mode-independent anchor. Picking the smallest-area
/// match skips the giant ancestor tables that also "contain" the text.
fn find_by_text(
    tree: &obscura_dom::tree::DomTree,
    layout: &obscura_render::DomLayout,
    needle: &str,
) -> Option<(f32, f32)> {
    let mut best: Option<(f32, obscura_render::Rect)> = None;
    for (id, rect) in &layout.rects {
        if tree.text_content(*id).contains(needle) {
            let area = rect.width * rect.height;
            if best.as_ref().map(|(a, _)| area < *a).unwrap_or(true) {
                best = Some((area, *rect));
            }
        }
    }
    best.map(|(_, r)| (r.x, r.y))
}

#[test]
fn hn_shaped_table_lays_out_without_site_hardcoding() {
    let tree = parse_html(HN_HTML);
    let layout = layout_dom(&tree, (1000.0, 1000.0));

    // Every element got a rect; the tree isn't just being dropped.
    assert!(layout.rects.len() > 10, "expected many laid-out elements, got {}", layout.rects.len());

    // "Hacker News" (a bold link in the top bar) sits above the first
    // headline row ("Exapunks (2018)"): normal top-to-bottom flow, not a
    // hardcoded absolute position.
    let (brand_x, brand_y) = find_by_text(&tree, &layout, "Hacker News").expect("brand text laid out");
    let (_, headline_y) = find_by_text(&tree, &layout, "Exapunks (2018)").expect("headline text laid out");
    assert!(
        headline_y > brand_y,
        "headline should be below the header bar: brand.y={} headline.y={}",
        brand_y,
        headline_y
    );

    // Within the header row, "login" (right-aligned cell) sits to the right
    // of "Hacker News" (left cell) — plain flex/table layout, not a magic
    // per-class x-offset.
    let (login_x, _) = find_by_text(&tree, &layout, "login").expect("login text laid out");
    assert!(
        login_x > brand_x,
        "login cell should be right of the brand: brand.x={} login.x={}",
        brand_x,
        login_x
    );
}

#[test]
fn relative_units_resolve_against_viewport_and_font_size() {
    // 50vw of a 1000px viewport = 500px; 10em at the default 16px = 160px.
    // Both were previously mis-resolved (vw kept as raw px, em hardcoded to 16
    // regardless of context), so this guards the deferred-resolution pass.
    let html = r##"<div style="width:50vw;height:10em"></div>"##;
    let tree = parse_html(html);
    let layout = layout_dom(&tree, (1000.0, 800.0));
    let hit = layout.rects.values().any(|r| (r.width - 500.0).abs() < 1.0 && (r.height - 160.0).abs() < 1.0);
    assert!(hit, "expected a 500x160 box from 50vw/10em, rects: {:?}", layout.rects.values().map(|r| (r.width, r.height)).collect::<Vec<_>>());
}

#[test]
fn dynamic_viewport_units_use_the_live_viewport() {
    let tree = parse_html(
        r#"<body style="margin:0"><div style="width:25dvw;height:15dvh"></div></body>"#,
    );
    let layout = layout_dom(&tree, (1000.0, 800.0));
    assert!(
        layout
            .rects
            .values()
            .any(|rect| (rect.width - 250.0).abs() < 0.01
                && (rect.height - 120.0).abs() < 0.01),
        "dvw/dvh should resolve against the capture viewport"
    );
}

#[test]
fn fixed_inset_box_uses_viewport_not_document_height() {
    let tree = parse_html(
        r#"
        <body style="margin:0">
          <div style="height:5000px"></div>
          <div id="overlay" style="position:fixed;inset:0">
            <canvas id="surface" style="width:100%;height:100%"></canvas>
          </div>
        </body>
        "#,
    );
    let layout = layout_dom(&tree, (900.0, 1000.0));
    for id in ["overlay", "surface"] {
        let rect = layout.rects[&tree.get_element_by_id(id).unwrap()];
        assert!(
            (rect.width - 900.0).abs() < 0.01
                && (rect.height - 1000.0).abs() < 0.01,
            "{id} should fill the viewport rather than the 5000px document: {rect:?}"
        );
    }
}

#[test]
fn percentage_padding_top_reserves_aspect_ratio_box() {
    // The responsive aspect-ratio trick: an empty box with padding-top:56.25%
    // inside a 1000px-wide block reserves a 16:9 area (~562px tall), the room a
    // `position:absolute; inset:0` media child fills. Percentage padding
    // resolves against the containing block WIDTH on every side, so the box
    // gains real height instead of collapsing to zero.
    let html = r##"<div style="width:1000px"><div style="padding-top:56.25%"></div></div>"##;
    let tree = parse_html(html);
    let layout = layout_dom(&tree, (1200.0, 800.0));
    let hit = layout
        .rects
        .values()
        .any(|r| (r.width - 1000.0).abs() < 1.0 && (r.height - 562.5).abs() < 2.0);
    assert!(
        hit,
        "expected a ~1000x562 aspect-ratio box from padding-top:56.25%, rects: {:?}",
        layout.rects.values().map(|r| (r.width, r.height)).collect::<Vec<_>>()
    );
}

#[test]
fn inset_absolute_uses_nearest_positioned_ancestor() {
    let html = r##"
        <body style="margin:0">
          <div id="cb" style="position:relative;margin:40px 0 0 60px;width:400px;height:240px;padding:20px;border:10px solid black;box-sizing:border-box">
            <div style="position:static;margin:50px 0 0 70px;width:120px;height:80px">
              <span id="abs" style="position:absolute;left:15px;top:25px;width:80px;height:60px"></span>
              <div id="abs-end" style="position:absolute;right:30px;bottom:20px;width:70px;height:50px"></div>
              <div id="abs-percent" style="position:absolute;left:50%;top:50%;width:30px;height:30px"></div>
              <div id="fixed" style="position:fixed;left:520px;top:40px;width:90px;height:55px"></div>
            </div>
          </div>
        </body>
    "##;
    let tree = parse_html(html);
    let layout = layout_dom(&tree, (900.0, 700.0));
    let cb = layout.rects[&tree.get_element_by_id("cb").expect("containing block")];
    let abs = layout.rects[&tree.get_element_by_id("abs").expect("absolute child")];
    let abs_end = layout.rects[&tree.get_element_by_id("abs-end").expect("end-inset child")];
    let abs_percent = layout.rects[&tree.get_element_by_id("abs-percent").expect("percent-inset child")];
    let fixed = layout.rects[&tree.get_element_by_id("fixed").expect("fixed child")];

    // Absolute insets are measured from the positioned ancestor's padding
    // edge, not from the intervening static wrapper.
    assert!((abs.x - cb.x - 25.0).abs() < 1.0, "wrong absolute x: cb={cb:?} abs={abs:?}");
    assert!((abs.y - cb.y - 35.0).abs() < 1.0, "wrong absolute y: cb={cb:?} abs={abs:?}");
    assert_eq!(
        layout.styles[&tree.get_element_by_id("abs").unwrap()].display,
        obscura_render::Display::Block,
        "positioned inline should be blockified"
    );
    assert!((abs_end.x - 350.0).abs() < 1.0, "wrong right-inset x: {abs_end:?}");
    assert!((abs_end.y - 200.0).abs() < 1.0, "wrong bottom-inset y: {abs_end:?}");
    assert!((abs_percent.x - 260.0).abs() < 1.0, "wrong percent-inset x: {abs_percent:?}");
    assert!((abs_percent.y - 160.0).abs() < 1.0, "wrong percent-inset y: {abs_percent:?}");
    assert!((fixed.x - 520.0).abs() < 1.0, "fixed box did not use viewport x: {fixed:?}");
    assert!((fixed.y - 40.0).abs() < 1.0, "fixed box did not use viewport y: {fixed:?}");
}

#[test]
fn absolute_auto_axes_preserve_static_position_after_reparenting() {
    let tree = parse_html(include_str!("../../../render-repros/absolute-static-position.html"));
    let layout = layout_dom(&tree, (900.0, 1000.0));
    let rect = |id| layout.rects[&tree.get_element_by_id(id).unwrap()];
    let static_inline = rect("top-static-inline");
    let static_block = rect("left-static-block");
    let outer_static = rect("outer-static");
    let nested_static = rect("nested-static");
    assert!(
        (static_inline.x - 125.0).abs() < 0.01
            && (static_inline.y - 60.0).abs() < 0.01,
        "static inline axis: {static_inline:?}"
    );
    assert!(
        (static_block.x - 210.0).abs() < 0.01
            && (static_block.y - 97.0).abs() < 0.01,
        "static block axis: {static_block:?}"
    );
    assert!(
        (outer_static.x - 84.0).abs() < 0.01 && (outer_static.y - 390.0).abs() < 0.01,
        "outer static candidate: {outer_static:?}"
    );
    assert!(
        (nested_static.x - 154.0).abs() < 0.01
            && (nested_static.y - 413.0).abs() < 0.01,
        "nested static candidate: {nested_static:?}"
    );
}

#[test]
fn legacy_center_keeps_block_flow_and_centers_descendants() {
    let tree = parse_html(include_str!("../../../render-repros/legacy-center.html"));
    let layout = layout_dom(&tree, (900.0, 1000.0));
    let rect = |selector| {
        let id = tree.query_selector_all(selector).unwrap()[0];
        layout.rects[&id]
    };
    let inline = rect(".inline-box");
    let block = rect(".block-box");
    let nested = rect(".nested-inline");
    let auto = rect(".auto-block");
    let table = rect(".center-table");
    let table_inline = rect(".table-inline");
    let cell_center = rect(".cell-center");
    let overridden = rect(".override-box");
    let pure = rect("#pure-center");

    assert!(
        (inline.x - 150.0).abs() < 0.01 && (inline.y - 0.0).abs() < 0.01,
        "centered inline content: {inline:?}"
    );
    assert!(
        (block.x - 150.0).abs() < 0.01 && (block.y - 20.0).abs() < 0.01,
        "centered block descendant: {block:?}"
    );
    assert!(
        (nested.x - 170.0).abs() < 0.01 && (nested.y - 40.0).abs() < 0.01,
        "inherited nested alignment: {nested:?}"
    );
    assert!(
        (auto.x - 0.0).abs() < 0.01
            && (auto.y - 60.0).abs() < 0.01
            && (auto.width - 400.0).abs() < 0.01,
        "auto-width block remains fill-available: {auto:?}"
    );
    assert!(
        (table.x - 100.0).abs() < 0.01
            && (table.y - 80.0).abs() < 0.01
            && (table.width - 200.0).abs() < 0.01,
        "table outer box is centered: {table:?}"
    );
    assert!(
        (table_inline.x - 100.0).abs() < 0.01
            && (table_inline.y - 80.0).abs() < 0.01,
        "table contents reset legacy alignment: {table_inline:?}"
    );
    assert!(
        (cell_center.x - 100.0).abs() < 0.01
            && (cell_center.y - 100.0).abs() < 0.01
            && (cell_center.width - 200.0).abs() < 0.01,
        "center in table cell fills the cell: {cell_center:?}"
    );
    assert!(
        (overridden.x - 0.0).abs() < 0.01 && (overridden.y - 120.0).abs() < 0.01,
        "author text-align override: {overridden:?}"
    );
    assert!(
        (pure.x - 0.0).abs() < 0.01
            && (pure.y - 240.0).abs() < 0.01
            && (pure.width - 400.0).abs() < 0.01,
        "pure-text center remains fill-available: {pure:?}"
    );
}

#[test]
fn list_indentation_is_reset_from_the_container() {
    let tree = parse_html(include_str!("../../../render-repros/list-indentation.html"));
    let layout = layout_dom(&tree, (900.0, 1000.0));
    let rect = |selector| {
        let id = tree.query_selector_all(selector).unwrap()[0];
        layout.rects[&id]
    };
    let default_item = rect("#default li");
    let reset_item = rect("#reset li");
    let ordered_item = rect("#ordered-reset li");
    let ex_box = rect("#ex-box");

    assert!(
        (default_item.x - 40.0).abs() < 0.01
            && (default_item.width - 360.0).abs() < 0.01,
        "default list indentation: {default_item:?}"
    );
    assert!(
        (reset_item.x - 0.0).abs() < 0.01
            && (reset_item.y - 40.0).abs() < 0.01
            && (reset_item.width - 400.0).abs() < 0.01,
        "reset unordered list: {reset_item:?}"
    );
    assert!(
        (ordered_item.x - 0.0).abs() < 0.01
            && (ordered_item.y - 80.0).abs() < 0.01
            && (ordered_item.width - 400.0).abs() < 0.01,
        "reset ordered list: {ordered_item:?}"
    );
    assert!(
        (ex_box.x - 0.0).abs() < 0.01
            && (ex_box.y - 120.0).abs() < 0.01
            && (ex_box.width - 44.0).abs() < 0.01
            && (ex_box.height - 22.0).abs() < 0.01,
        "ex padding: {ex_box:?}"
    );
}

#[test]
fn replaced_max_height_clamps_intrinsic_aspect_transfer() {
    let tree = parse_html(include_str!(
        "../../../render-repros/max-height-replaced.html"
    ));
    let image_id = tree.get_element_by_id("image").unwrap();
    let intrinsic = HashMap::from([(image_id, (648.0, 440.0))]);
    let layout = layout_dom_with_images(&tree, (900.0, 1000.0), &intrinsic);
    let card = layout.rects[&tree.get_element_by_id("card").unwrap()];
    let image = layout.rects[&image_id];
    assert!(
        (card.x - 0.0).abs() < 0.01
            && (card.y - 0.0).abs() < 0.01
            && (card.width - 394.0).abs() < 0.01
            && (card.height - 608.0).abs() < 0.01,
        "card: {card:?}"
    );
    assert!(
        (image.x - 32.0).abs() < 0.01
            && (image.y - 64.0).abs() < 0.01
            && (image.width - 330.0).abs() < 0.01
            && (image.height - 200.0).abs() < 0.01,
        "image: {image:?}"
    );
}

#[test]
fn transforms_establish_absolute_and_fixed_containing_blocks() {
    let html = r##"
        <body style="margin:0">
          <div id="outer" style="position:relative;margin:40px 0 0 50px;width:600px;height:400px;border:10px solid black;padding:20px;box-sizing:border-box">
            <div id="transformer" style="transform:translate(30px,20px);margin:40px 0 0 70px;width:300px;height:200px;border:5px solid black;padding:10px;box-sizing:border-box">
              <div style="position:static;margin:20px;width:80px;height:50px">
                <span id="abs" style="position:absolute;left:20px;top:25px;width:70px;height:55px"></span>
                <div id="fixed-transformed" style="position:fixed;left:150px;top:100px;width:80px;height:50px"></div>
              </div>
            </div>
            <div id="identity-transform" style="transform:rotate(0deg);margin:-80px 0 0 400px;width:100px;height:80px">
              <div id="identity-abs" style="position:absolute;right:10px;bottom:10px;width:20px;height:20px"></div>
            </div>
            <div id="fixed-viewport" style="position:fixed;left:700px;top:40px;width:90px;height:60px"></div>
          </div>
        </body>
    "##;
    let tree = parse_html(html);
    let layout = layout_dom(&tree, (900.0, 700.0));
    let transformer_id = tree.get_element_by_id("transformer").unwrap();
    let transformer = layout.rects[&transformer_id];
    let abs = layout.rects[&tree.get_element_by_id("abs").unwrap()];
    let fixed_transformed = layout.rects[&tree.get_element_by_id("fixed-transformed").unwrap()];
    let identity_transform = layout.rects[&tree.get_element_by_id("identity-transform").unwrap()];
    let identity_abs = layout.rects[&tree.get_element_by_id("identity-abs").unwrap()];
    let fixed_viewport = layout.rects[&tree.get_element_by_id("fixed-viewport").unwrap()];

    // Layout rects precede paint transforms. Insets use the transformer's
    // padding box; the shared translate is recorded separately for the whole
    // DOM subtree.
    assert!((abs.x - transformer.x - 25.0).abs() < 1.0, "wrong transformed abs x: transformer={transformer:?} abs={abs:?}");
    assert!((abs.y - transformer.y - 30.0).abs() < 1.0, "wrong transformed abs y: transformer={transformer:?} abs={abs:?}");
    assert!((fixed_transformed.x - transformer.x - 155.0).abs() < 1.0, "wrong transformed fixed x: {fixed_transformed:?}");
    assert!((fixed_transformed.y - transformer.y - 105.0).abs() < 1.0, "wrong transformed fixed y: {fixed_transformed:?}");
    assert_eq!(layout.translates[&transformer_id], (30.0, 20.0));
    assert!((identity_abs.x - identity_transform.x - 70.0).abs() < 1.0, "unsupported transform did not establish abs x: {identity_abs:?}");
    assert!((identity_abs.y - identity_transform.y - 50.0).abs() < 1.0, "unsupported transform did not establish abs y: {identity_abs:?}");
    assert!((fixed_viewport.x - 700.0).abs() < 1.0, "positioned ancestor captured fixed x: {fixed_viewport:?}");
    assert!((fixed_viewport.y - 40.0).abs() < 1.0, "positioned ancestor captured fixed y: {fixed_viewport:?}");
}

#[test]
fn modern_effects_establish_containing_blocks_independently() {
    let tree = parse_html(include_str!("../../../render-repros/modern-containing-block-triggers.html"));
    let layout = layout_dom(&tree, (900.0, 700.0));
    let cases = [
        ("filter-cb", "filter-badge"),
        ("perspective-cb", "perspective-badge"),
        ("contain-cb", "contain-badge"),
        ("will-change-cb", "will-change-badge"),
        ("visibility-cb", "visibility-badge"),
    ];
    for (cb_id, badge_id) in cases {
        let cb = layout.rects[&tree.get_element_by_id(cb_id).unwrap()];
        let badge = layout.rects[&tree.get_element_by_id(badge_id).unwrap()];
        assert!((badge.x - cb.x - 75.0).abs() < 1.0, "{badge_id} wrong x: cb={cb:?} badge={badge:?}");
        assert!((badge.y - cb.y - 55.0).abs() < 1.0, "{badge_id} wrong y: cb={cb:?} badge={badge:?}");
    }

    let filter_cb = layout.rects[&tree.get_element_by_id("filter-cb").unwrap()];
    let filter_fixed = layout.rects[&tree.get_element_by_id("filter-fixed").unwrap()];
    assert!((filter_fixed.x - filter_cb.x - 5.0).abs() < 1.0);
    assert!((filter_fixed.y - filter_cb.y - 5.0).abs() < 1.0);
    let contain_cb = layout.rects[&tree.get_element_by_id("contain-cb").unwrap()];
    let contain_fixed = layout.rects[&tree.get_element_by_id("contain-fixed").unwrap()];
    assert!((contain_fixed.x - contain_cb.x - 5.0).abs() < 1.0);
    assert!((contain_fixed.y - contain_cb.y - 5.0).abs() < 1.0);

    // Pinned Chromium 145 does not make container-type:inline-size a
    // positioning containing block. Keep it as a negative control.
    let container_badge = layout.rects[&tree.get_element_by_id("container-badge").unwrap()];
    assert!((container_badge.x - 775.0).abs() < 1.0, "container-type control x: {container_badge:?}");
    assert!((container_badge.y - 175.0).abs() < 1.0, "container-type control y: {container_badge:?}");
}

#[test]
fn long_text_run_wraps_across_multiple_lines() {
    // A long single text node with no inline elements breaking it up must
    // wrap within a narrow container instead of overflowing on one line. The
    // container's height is the mode-independent proof: several wrapped lines
    // make it much taller than one line, whether text is shaped by cosmic-text
    // (paint) or split into word boxes (layout-only).
    let html = r##"<div style="width:100px">This sentence has plenty of words to wrap across several lines</div>"##;
    let tree = parse_html(html);
    let layout = layout_dom(&tree, (1000.0, 1000.0));

    // Tightest element enclosing the text: the 100px div itself.
    let mut div_rect: Option<obscura_render::Rect> = None;
    for (id, rect) in &layout.rects {
        if tree.text_content(*id).contains("several lines") {
            if div_rect.as_ref().map(|d| rect.width * rect.height < d.width * d.height).unwrap_or(true) {
                div_rect = Some(*rect);
            }
        }
    }
    let div_rect = div_rect.expect("text container laid out");
    assert!(div_rect.width <= 101.0, "container should hold its 100px width, got {}", div_rect.width);
    assert!(
        div_rect.height > 60.0,
        "text should wrap onto several lines in a 100px-wide box (tall container), got height {}",
        div_rect.height
    );
}

#[test]
fn negative_flex_margin_overlays_at_container_start() {
    let html = r##"
        <html><head><style>
          html,body{margin:0}
          #document{display:flex;width:900px;height:220px}
          #main{width:100%;height:200px}
          #body{margin-left:225px;height:200px}
          #sidebar{display:flex;width:225px;height:180px;margin-left:-100%}
        </style></head><body>
          <div id="document">
            <div id="main"><div id="body"></div></div>
            <div id="sidebar"></div>
          </div>
        </body></html>
    "##;
    let tree = parse_html(html);
    let layout = layout_dom(&tree, (900.0, 1000.0));
    let document = layout.rects[&tree.get_element_by_id("document").unwrap()];
    let main = layout.rects[&tree.get_element_by_id("main").unwrap()];
    let body = layout.rects[&tree.get_element_by_id("body").unwrap()];
    let sidebar = layout.rects[&tree.get_element_by_id("sidebar").unwrap()];
    assert!((document.x - 0.0).abs() < 0.01, "document: {document:?}");
    assert!((main.x - 0.0).abs() < 0.01, "main: {main:?}");
    assert!((body.x - 225.0).abs() < 0.01, "body: {body:?}");
    assert!((sidebar.x - 0.0).abs() < 0.01, "sidebar: {sidebar:?}");
}

#[test]
fn percentage_flex_overlay_uses_auto_width_parent_content_box() {
    let html = r##"
        <html><head><style>
          html,body{margin:0}
          body{margin-left:1em;margin-right:1em}
          #document{display:flex}
          #main{float:left;width:100%;height:200px}
          #body{margin-left:min(25vw,350px);height:200px}
          #sidebar{
            display:flex;
            width:min(25vw,350px);
            height:180px;
            margin-left:-100%;
            float:none;
            position:sticky;
            top:0
          }
        </style></head><body>
          <div id="document">
            <div id="main"><div id="body"></div></div>
            <div id="sidebar"></div>
          </div>
        </body></html>
    "##;
    let tree = parse_html(html);
    let layout = layout_dom(&tree, (1280.0, 1400.0));
    let document = layout.rects[&tree.get_element_by_id("document").unwrap()];
    let main = layout.rects[&tree.get_element_by_id("main").unwrap()];
    let body = layout.rects[&tree.get_element_by_id("body").unwrap()];
    let sidebar = layout.rects[&tree.get_element_by_id("sidebar").unwrap()];
    assert!((document.x - 16.0).abs() < 0.01, "document: {document:?}");
    assert!((document.width - 1248.0).abs() < 0.01, "document: {document:?}");
    assert!((main.x - 16.0).abs() < 0.01, "main: {main:?}");
    assert!((body.x - 336.0).abs() < 0.01, "body: {body:?}");
    assert!((sidebar.x - 16.0).abs() < 0.01, "sidebar: {sidebar:?}");
    assert!((sidebar.width - 320.0).abs() < 0.01, "sidebar: {sidebar:?}");
}

#[test]
fn float_flow_zone_preserves_blocks_inline_runs_and_clearance() {
    let tree = parse_html(include_str!("../../../render-repros/float-flow-bands.html"));
    let layout = layout_dom(&tree, (900.0, 1000.0));
    let rect = |id| layout.rects[&tree.get_element_by_id(id).unwrap()];
    let float = rect("float");
    let intro = rect("intro");
    let release = rect("release");
    let download = rect("download");
    let archive = rect("archive");
    let sponsors = rect("sponsors");
    let logos = rect("logos");
    let cleared = rect("cleared");
    assert!((float.x - 650.0).abs() < 0.01 && (float.height - 400.0).abs() < 0.01, "float: {float:?}");
    assert!((intro.x - 0.0).abs() < 0.01 && (intro.width - 650.0).abs() < 0.01, "intro: {intro:?}");
    assert!((release.y - 100.0).abs() < 0.01 && (release.width - 650.0).abs() < 0.01, "release: {release:?}");
    assert!((download.x - 0.0).abs() < 0.01 && (download.y - 150.0).abs() < 0.01, "download: {download:?}");
    assert!((archive.x - 60.0).abs() < 0.01 && (archive.y - 150.0).abs() < 0.01, "archive: {archive:?}");
    assert!((sponsors.y - 180.0).abs() < 0.01, "sponsors: {sponsors:?}");
    assert!((logos.y - 230.0).abs() < 0.01, "logos: {logos:?}");
    assert!((cleared.y - 400.0).abs() < 0.01 && (cleared.width - 900.0).abs() < 0.01, "cleared: {cleared:?}");
    assert!(
        !layout.rects.contains_key(&tree.get_element_by_id("mobile-duplicate").unwrap()),
        "display:none duplicate generated a box"
    );
}

#[test]
fn float_exclusion_continues_through_non_bfc_block_wrappers() {
    let tree = parse_html(include_str!(
        "../../../render-repros/float-bfc-continuation.html"
    ));
    let layout = layout_dom(&tree, (900.0, 1000.0));
    let rect = |name| layout.rects[&tree.get_element_by_id(name).unwrap()];
    let float = rect("intro-float");
    let lead = rect("lead");
    let heading = rect("heading");
    let beside = rect("beside");
    let after = rect("after");
    assert!(
        (float.x - 650.0).abs() < 0.01
            && (float.y - 0.0).abs() < 0.01
            && (float.width - 250.0).abs() < 0.01
            && (float.height - 400.0).abs() < 0.01,
        "float: {float:?}"
    );
    assert!(
        (lead.x - 0.0).abs() < 0.01
            && (lead.y - 0.0).abs() < 0.01
            && (lead.width - 650.0).abs() < 0.01
            && (lead.height - 250.0).abs() < 0.01,
        "lead: {lead:?}"
    );
    assert!(
        (heading.x - 0.0).abs() < 0.01
            && (heading.y - 250.0).abs() < 0.01
            && (heading.width - 650.0).abs() < 0.01
            && (heading.height - 50.0).abs() < 0.01,
        "heading: {heading:?}"
    );
    assert!(
        (beside.x - 0.0).abs() < 0.01
            && (beside.y - 300.0).abs() < 0.01
            && (beside.width - 650.0).abs() < 0.01
            && (beside.height - 100.0).abs() < 0.01,
        "beside: {beside:?}"
    );
    assert!(
        (after.x - 0.0).abs() < 0.01
            && (after.y - 400.0).abs() < 0.01
            && (after.width - 900.0).abs() < 0.01
            && (after.height - 70.0).abs() < 0.01,
        "after: {after:?}"
    );
}

#[test]
fn replaced_image_contributes_intrinsic_size_in_ordered_grid() {
    let tree = parse_html(include_str!("../../../render-repros/replaced-grid-order.html"));
    let image_id = tree.get_element_by_id("image").unwrap();
    let intrinsic = HashMap::from([(image_id, (1.0, 1.0))]);
    let layout = layout_dom_with_images(&tree, (900.0, 1000.0), &intrinsic);
    let rect = |id| layout.rects[&tree.get_element_by_id(id).unwrap()];
    let description = rect("description");
    let media = rect("media");
    let image = rect("image");
    assert!(
        (description.x - 0.0).abs() < 0.01
            && (description.y - 0.0).abs() < 0.01
            && (description.width - 300.0).abs() < 0.01,
        "description: {description:?}"
    );
    assert!(
        (media.x - 300.0).abs() < 0.01
            && (media.y - 0.0).abs() < 0.01
            && (media.height - 300.0).abs() < 0.01,
        "media: {media:?}"
    );
    assert!(
        (image.x - 320.0).abs() < 0.01
            && (image.y - 20.0).abs() < 0.01
            && (image.width - 260.0).abs() < 0.01
            && (image.height - 260.0).abs() < 0.01,
        "image: {image:?}"
    );
}

#[test]
fn auto_block_wrapper_contains_percentage_replaced_child() {
    let tree = parse_html(include_str!("../../../render-repros/replaced-block-wrapper.html"));
    let image_id = tree.get_element_by_id("image").unwrap();
    let intrinsic = HashMap::from([(image_id, (720.0, 424.0))]);
    let layout = layout_dom_with_images(&tree, (900.0, 1000.0), &intrinsic);
    let rect = |id| layout.rects[&tree.get_element_by_id(id).unwrap()];
    let media = rect("media");
    let frame = rect("frame");
    let wrapper = rect("wrapper");
    let image = rect("image");
    assert!(
        (media.x - 450.0).abs() < 0.01
            && (media.width - 450.0).abs() < 0.01,
        "media: {media:?}"
    );
    assert!(
        (frame.x - 450.0).abs() < 0.01
            && (frame.width - 450.0).abs() < 0.01,
        "frame: {frame:?}"
    );
    assert!(
        (wrapper.x - 498.0).abs() < 0.01
            && (wrapper.width - 354.0).abs() < 0.01,
        "wrapper: {wrapper:?}"
    );
    assert!(
        (image.x - 498.0).abs() < 0.01
            && (image.width - 354.0).abs() < 0.01
            && (image.height - 208.0).abs() < 0.02,
        "image: {image:?}"
    );
}

#[test]
fn text_alignment_does_not_shrink_flex_items() {
    let tree = parse_html(include_str!("../../../render-repros/text-align-flex-items.html"));
    let layout = layout_dom(&tree, (900.0, 1000.0));
    let rect = |id| layout.rects[&tree.get_element_by_id(id).unwrap()];
    let stack = rect("stack");
    let media = rect("media");
    let label = rect("label");
    let chip = rect("chip");
    assert!(
        (stack.width - 400.0).abs() < 0.01
            && (media.x - 20.0).abs() < 0.01
            && (media.width - 360.0).abs() < 0.01,
        "stack: {stack:?}, media: {media:?}"
    );
    assert!(
        (label.x - 20.0).abs() < 0.01
            && (label.width - 360.0).abs() < 0.01
            && (chip.x - 150.0).abs() < 0.01
            && (chip.width - 100.0).abs() < 0.01,
        "label: {label:?}, chip: {chip:?}"
    );
}

#[test]
fn text_alignment_does_not_shrink_block_children_or_wrap_inline_block_rows() {
    let tree = parse_html(
        r#"<html><head><style>
           html,body{margin:0}
           section{width:900px;text-align:center}
           p{margin:0}
           a{display:inline-block;width:100px;height:40px;margin-right:10px}
           </style></head><body><section>
           <p id="row"><!--[--><a id="one">Why Vue</a><!--]--><a id="two"></a><!--marker--><a id="three"></a><a id="four"></a></p>
           </section></body></html>"#,
    );
    let layout = layout_dom(&tree, (900.0, 1000.0));
    let rect = |id| layout.rects[&tree.get_element_by_id(id).unwrap()];
    let row = rect("row");
    let items = [rect("one"), rect("two"), rect("three"), rect("four")];
    assert!(
        (row.width - 900.0).abs() < 0.01,
        "text-align must not shrink-wrap an auto-width block child: {row:?}"
    );
    assert!(
        items.windows(2).all(|pair| (pair[0].y - pair[1].y).abs() < 0.01),
        "inline-block items should share one line when the block has room: {items:?}"
    );
    assert!(
        items[0].height <= 40.01,
        "auto-width inline-block text should use one max-content line: {:?}",
        items[0]
    );
}

#[test]
fn inline_svg_derives_auto_height_from_view_box() {
    let tree = parse_html(
        r#"<html><head><style>html,body{margin:0}#logo{width:112px}</style></head>
           <body><svg id="logo" viewBox="-10.5 -9.45 21 18.9"></svg></body></html>"#,
    );
    let layout = layout_dom(&tree, (900.0, 1000.0));
    let logo = layout.rects[&tree.get_element_by_id("logo").unwrap()];
    assert!((logo.width - 112.0).abs() < 0.01, "svg width: {logo:?}");
    assert!(
        (logo.height - 101.0).abs() < 0.01,
        "viewBox should supply the intrinsic auto-height (device-pixel rounded): {logo:?}"
    );
}

/// Chromium 150 resolves a ratio-only inline SVG as an atomic replaced item
/// during both intrinsic grid track sizing and the final percentage-width
/// pass. The 270px track leaves 254px after the grid item's inline padding;
/// the 320:96 viewBox then contributes 76.1875px of height.
#[test]
fn ratio_only_inline_svg_sizes_inside_an_auto_grid_row() {
    let tree = parse_html(
        r#"<html><head><style>
             html,body{margin:0}
             #grid{display:grid;grid-template-columns:270px}
             #cell{display:grid;place-content:center;padding:16px 8px}
             #logo{display:block;width:100%;max-width:320px}
           </style></head><body>
             <div id="grid"><div id="cell">
               <svg id="logo" viewBox="0 0 320 96"><path d="M0 0h320v96z"/></svg>
             </div></div>
           </body></html>"#,
    );
    let layout = layout_dom(&tree, (800.0, 600.0));
    let rect = |id| layout.rects[&tree.get_element_by_id(id).unwrap()];
    let cell = rect("cell");
    let logo = rect("logo");
    assert!((logo.width - 254.0).abs() < 0.01, "SVG width: {logo:?}");
    assert!(
        (logo.height - 76.1875).abs() < 0.25,
        "viewBox ratio must transfer the final grid width before Obscura's device-pixel rounding: {logo:?}"
    );
    assert!(
        (cell.height - 108.1875).abs() < 0.25,
        "SVG contribution plus block padding must size the auto row: {cell:?}"
    );
}

#[test]
fn inline_block_flex_items_keep_block_inner_flow() {
    let tree = parse_html(include_str!("../../../render-repros/inline-block-flex-items.html"));
    let layout = layout_dom(&tree, (900.0, 1000.0));
    let rect = |id| layout.rects[&tree.get_element_by_id(id).unwrap()];
    let brand = rect("brand");
    let title = rect("title");
    let links = rect("links");
    let one = rect("one");
    let two = rect("two");
    let three = rect("three");
    let chips = rect("chips");
    let alpha = rect("alpha");
    let beta = rect("beta");
    let gamma = rect("gamma");
    assert!(
        (brand.x - 0.0).abs() < 0.01
            && (brand.width - 382.0).abs() < 0.01
            && (title.x - 32.0).abs() < 0.01
            && (title.width - 350.0).abs() < 0.01,
        "brand: {brand:?}, title: {title:?}"
    );
    assert!(
        (links.x - 525.0).abs() < 0.01
            && (links.width - 90.0).abs() < 0.01
            && (one.y - 0.0).abs() < 0.01
            && (two.y - 20.0).abs() < 0.01
            && (three.y - 40.0).abs() < 0.01,
        "links: {links:?}, items: {one:?} {two:?} {three:?}"
    );
    assert!(
        (chips.x - 0.0).abs() < 0.01
            && (chips.y - 140.0).abs() < 0.01
            && (chips.width - 90.0).abs() < 0.01
            && (chips.height - 20.0).abs() < 0.01
            && (alpha.x - 0.0).abs() < 0.01
            && (beta.x - 30.0).abs() < 0.01
            && (gamma.x - 60.0).abs() < 0.01,
        "chips: {chips:?}, items: {alpha:?} {beta:?} {gamma:?}"
    );
}

#[test]
fn percentage_height_under_indefinite_parent_uses_content_height() {
    let tree = parse_html(include_str!(
        "../../../render-repros/percentage-height-indefinite-parent.html"
    ));
    let layout = layout_dom(&tree, (900.0, 1000.0));
    for id in ["wrapper", "stack", "editor", "pre", "code"] {
        let rect = layout.rects[&tree.get_element_by_id(id).unwrap()];
        assert!(
            rect.height >= 24.0,
            "{id} collapsed despite its indefinite percentage height: {rect:?}"
        );
    }
}

#[test]
fn item_self_alignment_places_flex_and_grid_items() {
    let tree = parse_html(include_str!("../../../render-repros/item-self-alignment.html"));
    let layout = layout_dom(&tree, (900.0, 1000.0));
    let rect = |id| layout.rects[&tree.get_element_by_id(id).unwrap()];
    let flex_center = rect("flex-center");
    let flex_end = rect("flex-end");
    let grid_end = rect("grid-end");
    let grid_center = rect("grid-center");
    let grid_place = rect("grid-place");
    let parent_one = rect("parent-one");
    let parent_two = rect("parent-two");
    let line_one = rect("line-one");
    let line_two = rect("line-two");
    let track_one = rect("track-one");
    let track_two = rect("track-two");
    assert!(
        (flex_center.x - 0.0).abs() < 0.01
            && (flex_center.y - 40.0).abs() < 0.01
            && (flex_end.x - 40.0).abs() < 0.01
            && (flex_end.y - 70.0).abs() < 0.01,
        "flex items: {flex_center:?} {flex_end:?}"
    );
    assert!(
        (grid_end.x - 110.0).abs() < 0.01
            && (grid_end.y - 165.0).abs() < 0.01
            && (grid_center.x - 200.0).abs() < 0.01
            && (grid_center.y - 220.0).abs() < 0.01
            && (grid_place.x - 420.0).abs() < 0.01
            && (grid_place.y - 160.0).abs() < 0.01,
        "grid items: {grid_end:?} {grid_center:?} {grid_place:?}"
    );
    assert!(
        (parent_one.x - 55.0).abs() < 0.01
            && (parent_one.y - 340.0).abs() < 0.01
            && (parent_two.x - 195.0).abs() < 0.01
            && (parent_two.y - 330.0).abs() < 0.01,
        "parent-aligned grid items: {parent_one:?} {parent_two:?}"
    );
    assert!(
        (line_one.x - 0.0).abs() < 0.01
            && (line_one.y - 380.0).abs() < 0.01
            && (line_two.x - 0.0).abs() < 0.01
            && (line_two.y - 480.0).abs() < 0.01,
        "aligned flex lines: {line_one:?} {line_two:?}"
    );
    assert!(
        (track_one.x - 100.0).abs() < 0.01
            && (track_one.y - 570.0).abs() < 0.01
            && (track_two.x - 100.0).abs() < 0.01
            && (track_two.y - 620.0).abs() < 0.01,
        "aligned grid tracks: {track_one:?} {track_two:?}"
    );
}

#[test]
fn non_stretched_auto_grid_item_shrink_wraps_inside_grid_area() {
    let tree = parse_html(
        r#"
        <style>
          body { margin: 0 }
          .panel {
            display: grid;
            align-items: start;
            justify-items: center;
            box-sizing: border-box;
          }
          demo-wrap { display: block }
          #wide-panel {
            margin-left: 112px;
            width: 1168px;
            padding: 16px 48px;
          }
          #wide-inner {
            display: grid;
            grid-template-columns: 1fr 1fr;
            box-sizing: border-box;
            width: 100%;
            max-width: 1200px;
            padding: 32px;
          }
          #wide-inner span {
            display: inline-block;
            width: 56px;
            height: 20px;
          }
          #narrow-panel { width: 400px }
          #narrow-inner { width: 200px; height: 20px }
        </style>
        <div id="wide-panel" class="panel">
          <demo-wrap id="wide-wrapper">
            <div id="wide-inner">
              <div>
                <span></span><span></span><span></span><span></span><span></span><span></span>
                <span></span><span></span><span></span><span></span><span></span>
              </div>
              <div>
                <span></span><span></span><span></span><span></span><span></span><span></span>
                <span></span><span></span><span></span><span></span><span></span>
              </div>
            </div>
          </demo-wrap>
        </div>
        <div id="narrow-panel" class="panel">
          <demo-wrap id="narrow-wrapper"><div id="narrow-inner"></div></demo-wrap>
        </div>
        "#,
    );
    let layout = layout_dom(&tree, (1280.0, 1000.0));
    let rect = |id| layout.rects[&tree.get_element_by_id(id).unwrap()];
    let wide_panel = rect("wide-panel");
    let wide_wrapper = rect("wide-wrapper");
    let wide_inner = rect("wide-inner");
    let narrow_wrapper = rect("narrow-wrapper");
    assert!(
        (wide_wrapper.x - 160.0).abs() < 0.01
            && (wide_wrapper.width - 1072.0).abs() < 0.01
            && (wide_inner.x - 160.0).abs() < 0.01
            && (wide_inner.width - 1072.0).abs() < 0.01,
        "wide shrink-wrapped item should be clamped to the grid area: \
         panel={wide_panel:?} wrapper={wide_wrapper:?} inner={wide_inner:?}"
    );
    assert!(
        (narrow_wrapper.x - 100.0).abs() < 0.01
            && (narrow_wrapper.width - 200.0).abs() < 0.01,
        "a narrower item should keep its intrinsic width and remain centered: {narrow_wrapper:?}"
    );
}

#[test]
fn percentage_max_width_clamps_grid_item_intrinsic_size() {
    let tree = parse_html(
        r#"
        <style>
          body { margin: 0 }
          #grid { display: grid; width: 1200px }
          #item { max-width: 100% }
          #gallery { display: flex; gap: 8px }
          .column { flex: none; height: 20px }
        </style>
        <div id="grid">
          <div id="item">
            <div id="gallery">
              <div class="column" style="width:303px"></div>
              <div class="column" style="width:200px"></div>
              <div class="column" style="width:500px"></div>
              <div class="column" style="width:200px"></div>
              <div class="column" style="width:293px"></div>
            </div>
          </div>
        </div>
        "#,
    );
    let layout = layout_dom(&tree, (1280.0, 200.0));
    let grid = layout.rects[&tree.get_element_by_id("grid").unwrap()];
    let item = layout.rects[&tree.get_element_by_id("item").unwrap()];
    let gallery = layout.rects[&tree.get_element_by_id("gallery").unwrap()];
    assert!(
        (grid.width - 1200.0).abs() < 0.01
            && (item.width - 1200.0).abs() < 0.01
            && (gallery.width - 1200.0).abs() < 0.01,
        "max-width:100% must clamp the grid item while its flex contents overflow: \
         grid={grid:?} item={item:?} gallery={gallery:?}"
    );
}

#[test]
fn named_area_shorthand_survives_end_longhand_override() {
    let tree = parse_html(
        r#"
        <style>
          body { margin: 0 }
          #grid {
            display: grid;
            width: 1200px;
            grid-template-columns:
              [extended-full-start] 16px
              [full-start] 200px
              [content-start] 768px
              [content-end] 200px
              [full-end] 16px
              [extended-full-end];
          }
          #grid > * { grid-column: content }
          #hero { grid-column-end: extended-full-end }
        </style>
        <div id="grid"><div id="hero"></div></div>
        "#,
    );
    let layout = layout_dom(&tree, (1280.0, 200.0));
    let hero = layout.rects[&tree.get_element_by_id("hero").unwrap()];
    assert!(
        (hero.x - 216.0).abs() < 0.01 && (hero.width - 984.0).abs() < 0.01,
        "the end longhand must retain the shorthand's named content start: {hero:?}"
    );
}

#[test]
fn auto_width_flex_button_keeps_native_intrinsic_sizing() {
    let tree = parse_html(
        r#"
        <style>
          body { margin: 0 }
          #box { width: 600px }
          button {
            display: flex;
            margin: 0 auto;
            padding: 19px 24px;
            border: 2px solid;
            gap: 6px;
            font-size: 24px;
          }
          button::before { content: ""; width: 1em; height: 1em }
        </style>
        <div id="box"><button id="search">Search</button></div>
        "#,
    );
    let layout = layout_dom(&tree, (800.0, 200.0));
    let button = layout.rects[&tree.get_element_by_id("search").unwrap()];
    assert!(
        button.width > 120.0
            && button.width < 220.0
            && (button.x - (600.0 - button.width) / 2.0).abs() < 0.01,
        "the auto-width flex button should shrink-wrap and center: {button:?}"
    );
}

#[test]
fn flex_auto_margin_absorbs_space_before_justify_content() {
    let tree = parse_html(
        r#"
        <style>
          body { margin: 0 }
          #bar {
            display: flex;
            width: 1280px;
            gap: 16px;
            justify-content: flex-end;
          }
          #crumb { width: 42px; height: 20px; margin: 0 auto 0 0 }
          #theme { width: 91px; height: 20px }
          #language { width: 131px; height: 20px }
        </style>
        <div id="bar">
          <div id="crumb"></div>
          <div id="theme"></div>
          <div id="language"></div>
        </div>
        "#,
    );
    let layout = layout_dom(&tree, (1280.0, 200.0));
    let rect = |id| layout.rects[&tree.get_element_by_id(id).unwrap()];
    let crumb = rect("crumb");
    let theme = rect("theme");
    let language = rect("language");
    assert!(
        (crumb.x - 0.0).abs() < 0.01
            && (theme.x - 1042.0).abs() < 0.01
            && (language.x - 1149.0).abs() < 0.01,
        "main-axis auto margin must consume free space before justify-content: \
         crumb={crumb:?} theme={theme:?} language={language:?}"
    );
}

#[test]
fn nested_is_descendant_utility_blockifies_matching_lines() {
    let tree = parse_html(
        r#"
        <style>
          pre { display: flex }
          code code { position: relative }
          :is(.\*\*\:\[\.line\]\:block *).line { display: block }
        </style>
        <div class="**:[.line]:block">
          <pre><code><code><span id="one" class="line">one</span><span id="two" class="line">two</span></code></code></pre>
        </div>
        "#,
    );
    let layout = layout_dom(&tree, (500.0, 200.0));
    let one = tree.get_element_by_id("one").unwrap();
    let two = tree.get_element_by_id("two").unwrap();
    assert_eq!(layout.styles[&one].display, obscura_render::Display::Block);
    assert_eq!(layout.styles[&two].display, obscura_render::Display::Block);
    assert!(layout.rects[&two].y > layout.rects[&one].y);
    assert_eq!(layout.rects[&one].width, layout.rects[&two].width);
}

#[test]
fn opposing_floats_share_header_band_through_inline_wrapper() {
    let tree = parse_html(include_str!("../../../render-repros/opposing-header-floats.html"));
    let layout = layout_dom(&tree, (900.0, 1000.0));
    let rect = |id| layout.rects[&tree.get_element_by_id(id).unwrap()];
    let logo = rect("logo");
    let tagline = rect("tagline");
    let menu = rect("menu");
    assert!(
        (logo.x - 0.0).abs() < 0.01
            && (logo.y - 0.0).abs() < 0.01
            && (logo.width - 200.0).abs() < 0.01
            && (logo.height - 100.0).abs() < 0.01,
        "logo: {logo:?}"
    );
    assert!(
        (tagline.x - 600.0).abs() < 0.01
            && (tagline.y - 60.0).abs() < 0.01
            && (tagline.width - 300.0).abs() < 0.01
            && (tagline.height - 40.0).abs() < 0.01,
        "tagline: {tagline:?}"
    );
    assert!(
        (menu.x - 0.0).abs() < 0.01
            && (menu.y - 100.0).abs() < 0.01
            && (menu.width - 900.0).abs() < 0.01,
        "menu: {menu:?}"
    );
}

#[test]
fn mixed_float_run_shares_one_band() {
    let tree = parse_html(include_str!("../../../render-repros/mixed-float-run.html"));
    let layout = layout_dom(&tree, (900.0, 1000.0));
    let rect = |id| layout.rects[&tree.get_element_by_id(id).unwrap()];
    let left_one = rect("left-one");
    let left_two = rect("left-two");
    let right_one = rect("right-one");
    assert!(
        (left_one.x - 0.0).abs() < 0.01
            && (left_one.y - 0.0).abs() < 0.01
            && (left_one.width - 80.0).abs() < 0.01
            && (left_one.height - 30.0).abs() < 0.01,
        "first left float: {left_one:?}"
    );
    assert!(
        (left_two.x - 80.0).abs() < 0.01
            && (left_two.y - 0.0).abs() < 0.01
            && (left_two.width - 80.0).abs() < 0.01
            && (left_two.height - 30.0).abs() < 0.01,
        "second left float: {left_two:?}"
    );
    assert!(
        (right_one.x - 340.0).abs() < 0.01
            && (right_one.y - 0.0).abs() < 0.01
            && (right_one.width - 60.0).abs() < 0.01
            && (right_one.height - 30.0).abs() < 0.01,
        "right float: {right_one:?}"
    );
}

#[test]
fn right_float_navigation_shares_inline_band() {
    let tree = parse_html(include_str!(
        "../../../render-repros/right-float-navigation.html"
    ));
    let layout = layout_dom(&tree, (900.0, 1000.0));
    let rect = |id| layout.rects[&tree.get_element_by_id(id).unwrap()];
    let left_one = rect("left-one");
    let left_two = rect("left-two");
    let right_one = rect("right-one");
    let right_two = rect("right-two");
    let right_three = rect("right-three");
    assert!(
        (left_one.x - 0.0).abs() < 0.01
            && (left_one.y - 0.0).abs() < 0.01
            && (left_two.x - 64.0).abs() < 0.01
            && (left_two.y - 0.0).abs() < 0.01,
        "inline flow: {left_one:?} {left_two:?}"
    );
    assert!(
        (right_three.x - 280.0).abs() < 0.01
            && (right_three.y - 0.0).abs() < 0.01
            && (right_two.x - 310.0).abs() < 0.01
            && (right_two.y - 0.0).abs() < 0.01
            && (right_one.x - 360.0).abs() < 0.01
            && (right_one.y - 0.0).abs() < 0.01,
        "right float order: {right_three:?} {right_two:?} {right_one:?}"
    );
}

#[test]
fn functional_font_sizes_resolve_against_live_viewport() {
    let tree = parse_html(
        r#"
        <style>
          #feature { font-size: clamp(1rem, 9vw, 38pt) }
          #subtitle { font-size: clamp(.5rem, 4vw, 1rem) }
        </style>
        <h2 id="feature">Features</h2>
        <h3 id="subtitle">Subtitle</h3>
        "#,
    );
    let wide = layout_dom(&tree, (900.0, 1000.0));
    let feature = tree.get_element_by_id("feature").unwrap();
    let subtitle = tree.get_element_by_id("subtitle").unwrap();
    assert!(
        (wide.styles[&feature].font_size.unwrap() - 50.654).abs() < 0.05,
        "wide feature font: {:?}",
        wide.styles[&feature].font_size
    );
    assert_eq!(wide.styles[&subtitle].font_size, Some(16.0));

    let narrow = layout_dom(&tree, (320.0, 640.0));
    assert!(
        (narrow.styles[&feature].font_size.unwrap() - 28.8).abs() < 0.05,
        "narrow feature font: {:?}",
        narrow.styles[&feature].font_size
    );
    assert!(
        (narrow.styles[&subtitle].font_size.unwrap() - 12.8).abs() < 0.05,
        "narrow subtitle font: {:?}",
        narrow.styles[&subtitle].font_size
    );
}

#[test]
fn responsive_grid_child_spans_all_tracks_via_end_longhand() {
    let tree = parse_html(
        r#"
        <style>
          .layout {
            display: grid;
            width: 800px;
            grid-template-columns: repeat(4, 1fr);
            column-gap: 20px;
          }
          @media (max-width: 1007px) {
            .layout > * { grid-column-end: span 4 }
          }
        </style>
        <div class="layout"><section id="hero">Full-width content</section></div>
        "#,
    );
    let layout = layout_dom(&tree, (900.0, 1000.0));
    let hero = layout.rects[&tree.get_element_by_id("hero").unwrap()];
    assert!(
        (hero.width - 800.0).abs() < 0.01,
        "span-4 child should fill all four tracks: {hero:?}"
    );
}

#[test]
fn length_line_height_computes_before_inheritance() {
    let tree = parse_html(
        r#"
        <style>
          #parent { font-size: 64px; line-height: 5rem }
          #child { font-size: 20px }
          #ratio { font-size: 20px; line-height: 2 }
        </style>
        <div id="parent"><span id="child">Inherited length</span></div>
        <div id="ratio">Unitless ratio</div>
        "#,
    );
    let layout = layout_dom(&tree, (900.0, 1000.0));
    let parent = tree.get_element_by_id("parent").unwrap();
    let child = tree.get_element_by_id("child").unwrap();
    let ratio = tree.get_element_by_id("ratio").unwrap();
    assert_eq!(
        layout.styles[&parent].line_height,
        Some(obscura_render::LineHeight::Px(80.0))
    );
    assert_eq!(
        layout.styles[&child].line_height,
        Some(obscura_render::LineHeight::Px(80.0))
    );
    assert_eq!(
        layout.styles[&ratio].line_height,
        Some(obscura_render::LineHeight::Ratio(2.0))
    );
}

#[test]
fn font_weight_computes_numerically_through_inheritance() {
    let tree = parse_html(
        r#"
        <div id="medium" style="font-weight:500">
          <span id="inherited">medium</span>
          <strong id="normal" style="font-weight:normal">normal</strong>
          <span id="bolder" style="font-weight:bolder">bolder</span>
          <span id="lighter" style="font-weight:lighter">lighter</span>
        </div>
        "#,
    );
    let layout = layout_dom(&tree, (900.0, 1000.0));
    let weight = |id| {
        layout.styles[&tree.get_element_by_id(id).unwrap()]
            .font_weight
            .as_deref()
    };
    assert_eq!(weight("medium"), Some("500"));
    assert_eq!(weight("inherited"), Some("500"));
    assert_eq!(weight("normal"), Some("400"));
    assert_eq!(weight("bolder"), Some("700"));
    assert_eq!(weight("lighter"), Some("100"));
}

#[test]
fn contextual_right_inset_uses_root_font_tokens() {
    let tree = parse_html(
        r#"
        <style>
          :root { --spacing: .25rem }
          body { margin: 0 }
          #header { position: relative; width: 900px; height: 80px }
          #search {
            position: absolute;
            right: calc(var(--spacing)*14);
            width: 100px;
            height: 36px;
          }
        </style>
        <div id="header"><button id="search">Search</button></div>
        "#,
    );
    let layout = layout_dom(&tree, (900.0, 1000.0));
    let search = tree.get_element_by_id("search").unwrap();
    assert_eq!(
        layout.styles[&search].inset[1],
        Some(obscura_render::Dimension::Px(56.0))
    );
    let rect = layout.rects[&search];
    assert!(
        (rect.x - 744.0).abs() < 0.01,
        "right inset should place 100px box at 900 - 56 - 100: {rect:?}"
    );
}

#[test]
fn false_legacy_supports_probe_does_not_override_modern_line_height() {
    // Tailwind v4 uses this old-browser probe around a leading-variable reset.
    // Chromium rejects the condition. Flattening every @supports body made
    // the reset win, invalidated the var(), and inflated three 96px lines from
    // 288px to an inherited 432px.
    let tree = parse_html(
        r#"
        <style>
          @supports (((-webkit-hyphens:none)) and
                    (not (margin-trim:inline))) or
                    ((-moz-orient:inline) and
                    (not (color:rgb(from red r g b)))) {
            * { --leading:initial }
          }
          :root { --hero-line-height:1 }
          body { margin:0; line-height:1.5 }
          #hero {
            margin:0;
            width:600px;
            font-size:96px;
            line-height:var(--leading,var(--hero-line-height))
          }
        </style>
        <h1 id="hero">one<br>two<br>three</h1>
        "#,
    );
    let layout = layout_dom(&tree, (1280.0, 720.0));
    let hero = layout.rects[&tree.get_element_by_id("hero").unwrap()];
    assert!(
        (hero.height - 288.0).abs() < 0.01,
        "three 96px modern lines should be 288px tall: {hero:?}"
    );
}

#[test]
fn custom_property_css_wide_keywords_preserve_var_fallback_semantics() {
    let tree = parse_html(
        r#"
        <style>
          :root {
            --tone:#112233;
            --light:initial;
            --toggle:var(--light) #18191b;
            --page:var(--toggle,#ffffff)
          }
          #invalid { --tone:initial; color:var(--tone,#445566) }
          #inherited { --tone:unset; color:var(--tone,#445566) }
          #nested { background-color:var(--page) }
        </style>
        <div id="invalid">fallback</div>
        <div id="inherited">inherit</div>
        <div id="nested">nested fallback</div>
        "#,
    );
    let layout = layout_dom(&tree, (800.0, 600.0));
    let invalid = tree.get_element_by_id("invalid").unwrap();
    let inherited = tree.get_element_by_id("inherited").unwrap();
    let nested = tree.get_element_by_id("nested").unwrap();
    assert_eq!(
        layout.styles[&invalid].color,
        Some([0x44, 0x55, 0x66, 0xff]),
        "`initial` is guaranteed-invalid and var() must use its fallback"
    );
    assert_eq!(
        layout.styles[&inherited].color,
        Some([0x11, 0x22, 0x33, 0xff]),
        "`unset` inherits a custom property's parent value"
    );
    assert_eq!(
        layout.styles[&nested].background_color,
        Some([0xff, 0xff, 0xff, 0xff]),
        "an invalid nested custom property must activate the outer fallback"
    );
}

#[test]
fn stable_root_scrollbar_gutter_reduces_the_initial_containing_block() {
    let tree = parse_html(
        r#"
        <style>
          html { overflow-y:auto; scrollbar-gutter:stable }
          body { margin:0 }
          #fill { width:100%; height:40px }
          #center { width:401px; height:40px; margin-inline:auto }
        </style>
        <div id="fill"></div>
        <div id="center"></div>
        "#,
    );
    let layout = layout_dom(&tree, (900.0, 600.0));
    let fill = layout.rects[&tree.get_element_by_id("fill").unwrap()];
    let center = layout.rects[&tree.get_element_by_id("center").unwrap()];
    assert!(
        (fill.width - 885.0).abs() < 0.01,
        "stable classic gutter should reserve 15px: {fill:?}"
    );
    assert!(
        (center.x - 242.0).abs() < 0.01,
        "401px box should center in the 885px ICB: {center:?}"
    );
}

#[test]
fn auto_grid_track_freezes_at_growth_limit_during_distribution() {
    // The first auto track reaches its max-content growth limit before the
    // second. The remaining free space belongs only to the second track.
    // Giving the frozen track that remainder again makes the grid overflow
    // even though the two automatic minimums fit the definite container.
    let tree = parse_html(
        r#"
        <style>
          * { box-sizing:border-box }
          html, body { margin:0 }
          #grid {
            display:grid;
            width:1016px;
            grid-template-columns:auto auto;
            grid-template-rows:320px;
            column-gap:16px;
          }
          #copy {
            grid-column:1;
            font:400 48px/57.6px sans-serif;
          }
          #art {
            grid-column:2;
            overflow:hidden;
          }
          #art > div { width:560px; height:560px }
        </style>
        <div id="grid">
          <h1 id="copy">Resources for Developers,<br> by Developers</h1>
          <div id="art"><div></div></div>
        </div>
        "#,
    );
    let layout = layout_dom(&tree, (1280.0, 720.0));
    let rect = |id| layout.rects[&tree.get_element_by_id(id).unwrap()];
    let grid = rect("grid");
    let copy = rect("copy");
    let art = rect("art");
    assert!(
        ((copy.x + copy.width + 16.0) - art.x).abs() < 0.01,
        "the authored column gap must separate the final tracks: {copy:?} {art:?}"
    );
    assert!(
        ((art.x + art.width) - (grid.x + grid.width)).abs() < 0.01,
        "free-space redistribution must not grow a track after it freezes: {grid:?} {copy:?} {art:?}"
    );
}

#[test]
fn empty_block_before_participates_in_normal_flow_geometry() {
    let tree = parse_html(
        r#"
        <style>
          html, body { margin:0 }
          body { font-size:20px; line-height:20px }
          #host { width:200px; background:#eee }
          #host::before {
            content:"";
            display:block;
            width:80px;
            height:40px;
            margin-bottom:10px;
            background:#06c;
          }
          #next { width:20px; height:10px; background:#0a0 }
        </style>
        <div id="host">TEXT</div><div id="next"></div>
        "#,
    );
    let layout = layout_dom(&tree, (800.0, 600.0));
    let host = layout.rects[&tree.get_element_by_id("host").unwrap()];
    let next = layout.rects[&tree.get_element_by_id("next").unwrap()];
    assert!(
        (host.height - 70.0).abs() < 0.01,
        "40px generated block + 10px margin + 20px line must size host: {host:?}"
    );
    assert!(
        (next.y - 70.0).abs() < 0.01,
        "following flow content must start after generated geometry: {next:?}"
    );
}

#[test]
fn empty_inline_block_pseudo_is_an_atomic_flex_item() {
    let tree = parse_html(
        r#"
        <style>
          html, body { margin:0 }
          #host { display:flex; width:100px; height:20px }
          #host::before {
            content:"";
            display:inline-block;
            width:.5em;
            height:.5em;
            margin-right:5px;
            border:2px solid #06c;
          }
          #label { width:20px; height:10px }
        </style>
        <div id="host"><span id="label"></span></div>
        "#,
    );
    let layout = layout_dom(&tree, (800.0, 600.0));
    let label = layout.rects[&tree.get_element_by_id("label").unwrap()];
    assert!(
        (label.x - 17.0).abs() < 0.01,
        "8px pseudo content width + 2px borders + 5px margin must precede label: {label:?}"
    );
}

#[test]
fn light_dark_uses_the_final_inherited_color_scheme_across_the_cascade() {
    let tree = parse_html(
        r#"
        <style>
          :root {
            --surface:light-dark(#f5f5f5,#18191b);
          }
          #dark {
            background-color:var(--surface);
            color:light-dark(#111111,#eeeeee);
            border:1px solid light-dark(#dddddd,#333333);
          }
          #dark {
            color-scheme:dark;
          }
          #dark::before {
            content:"";
            display:block;
            width:10px;
            height:10px;
            background:light-dark(#ff0000,#0000ff);
          }
          #child {
            background-image:linear-gradient(
              light-dark(#ffffff,#101010),
              light-dark(#eeeeee,#202020)
            );
          }
          #icon rect {
            fill:light-dark(#c3c7cb,#51565d);
            stroke:light-dark(#ffffff,#000000);
          }
          #mixed {
            background:light-dark(#abcdef,#123456);
            color-scheme:light dark;
          }
        </style>
        <section id="dark">
          <div id="child"></div>
          <svg id="icon"><rect width="10" height="10"/></svg>
        </section>
        <div id="mixed"></div>
        "#,
    );
    let layout = layout_dom(&tree, (800.0, 600.0));
    let dark = tree.get_element_by_id("dark").unwrap();
    let child = tree.get_element_by_id("child").unwrap();
    let mixed = tree.get_element_by_id("mixed").unwrap();
    let rect = tree.query_selector("#icon rect").unwrap().unwrap();

    let dark_style = &layout.styles[&dark];
    assert!(dark_style.color_scheme_dark);
    assert_eq!(dark_style.background_color, Some([0x18, 0x19, 0x1b, 0xff]));
    assert_eq!(dark_style.color, Some([0xee, 0xee, 0xee, 0xff]));
    assert_eq!(dark_style.border_color, Some([0x33, 0x33, 0x33, 0xff]));
    assert_eq!(
        dark_style
            .before_pseudo
            .as_deref()
            .and_then(|pseudo| pseudo.background_color),
        Some([0x00, 0x00, 0xff, 0xff]),
        "pseudo must use its host's inherited dark scheme"
    );

    let child_style = &layout.styles[&child];
    assert!(child_style.color_scheme_dark);
    let (_, stops) = child_style
        .background_gradient
        .as_ref()
        .expect("dark gradient");
    assert_eq!(stops[0].0, [0x10, 0x10, 0x10, 0xff]);
    assert_eq!(stops[1].0, [0x20, 0x20, 0x20, 0xff]);

    let rect_style = &layout.styles[&rect];
    assert!(rect_style.color_scheme_dark);
    assert_eq!(rect_style.svg_fill.as_deref(), Some("#51565dff"));
    assert_eq!(rect_style.svg_stroke.as_deref(), Some("#000000ff"));

    let mixed_style = &layout.styles[&mixed];
    assert!(
        !mixed_style.color_scheme_dark,
        "a scheme list admitting light uses the current light preference"
    );
    assert_eq!(mixed_style.background_color, Some([0xab, 0xcd, 0xef, 0xff]));
}

/// Chromium 150 maps width/height from the selected <picture><source> onto
/// the associated image as presentation hints. The selected 800/400 source
/// therefore reserves a 500x250 box before it loads; below the media
/// breakpoint, the fallback img's 600/600 ratio reserves 500x500.
#[test]
fn selected_picture_source_dimensions_override_fallback_image_ratio() {
    let tree = parse_html(
        r#"
        <style>
          html, body { margin:0 }
          img { display:block; width:500px; height:auto }
        </style>
        <picture>
          <source
            media="(min-width:800px)"
            srcset="chosen.png"
            width="800"
            height="400">
          <img id="image" src="fallback.png" width="600" height="600">
        </picture>
        "#,
    );
    let image = tree.get_element_by_id("image").unwrap();

    let wide = layout_dom(&tree, (1000.0, 600.0));
    let wide_rect = wide.rects[&image];
    assert!((wide_rect.width - 500.0).abs() < 0.01, "{wide_rect:?}");
    assert!((wide_rect.height - 250.0).abs() < 0.01, "{wide_rect:?}");
    assert_eq!(wide.styles[&image].aspect_ratio, Some(2.0));

    let narrow = layout_dom(&tree, (700.0, 600.0));
    let narrow_rect = narrow.rects[&image];
    assert!((narrow_rect.width - 500.0).abs() < 0.01, "{narrow_rect:?}");
    assert!((narrow_rect.height - 500.0).abs() < 0.01, "{narrow_rect:?}");
    assert_eq!(narrow.styles[&image].aspect_ratio, Some(1.0));
}

/// HTML width/height attributes are provisional presentational hints. Once
/// the image is decoded, its natural dimensions replace that mapped ratio.
#[test]
fn decoded_image_ratio_replaces_mapped_html_ratio() {
    let tree = parse_html(
        r#"
        <style>
          html, body { margin:0 }
          img { display:block; width:500px; height:auto }
        </style>
        <img id="image" src="image.png" width="600" height="600">
        "#,
    );
    let image = tree.get_element_by_id("image").unwrap();
    let intrinsic = HashMap::from([(image, (800.0, 400.0))]);
    let layout = layout_dom_with_images(&tree, (1000.0, 600.0), &intrinsic);
    let rect = layout.rects[&image];

    assert!((rect.width - 500.0).abs() < 0.01, "{rect:?}");
    assert!((rect.height - 250.0).abs() < 0.01, "{rect:?}");
    assert_eq!(layout.styles[&image].aspect_ratio, Some(2.0));
}

/// Unlike HTML presentation hints, an authored `aspect-ratio` declaration
/// remains authoritative after the resource's natural dimensions are known.
#[test]
fn authored_aspect_ratio_wins_over_decoded_image_ratio() {
    let tree = parse_html(
        r#"
        <style>
          html, body { margin:0 }
          img {
            display:block;
            width:500px;
            height:auto;
            aspect-ratio:4 / 1;
          }
        </style>
        <img id="image" src="image.png" width="600" height="600">
        "#,
    );
    let image = tree.get_element_by_id("image").unwrap();
    let intrinsic = HashMap::from([(image, (800.0, 400.0))]);
    let layout = layout_dom_with_images(&tree, (1000.0, 600.0), &intrinsic);
    let rect = layout.rects[&image];

    assert!((rect.width - 500.0).abs() < 0.01, "{rect:?}");
    assert!((rect.height - 125.0).abs() < 0.01, "{rect:?}");
    assert_eq!(layout.styles[&image].aspect_ratio, Some(4.0));
}

//! Regression test: a Hacker-News-shaped nested `<table>` layout must come out
//! right using only the general engine (UA defaults + real CSS cascade), with
//! no per-site hardcoded selectors. This guards against reintroducing the
//! site-specific hacks that used to live in obscura-render for this exact markup.

use obscura_dom::tree_sink::parse_html;
use obscura_render::layout_dom;

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

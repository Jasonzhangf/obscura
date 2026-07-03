//! Regression test: a Hacker-News-shaped nested `<table>` layout must come out
//! right using only the general engine (UA defaults + real CSS cascade), with
//! no per-site hardcoded selectors. This guards against reintroducing the
//! site-specific hacks that used to live in obscura-render for this exact markup.

use obscura_dom::tree::NodeData;
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

/// The top-left corner of a text node's overall bounding box: a text node
/// lays out as one taffy leaf per word (see `dom::build_text_words`), so its
/// geometry lives in `text_runs` as several (box, word) pairs rather than a
/// single rect. `needle` is matched against the node's whole original
/// content, not a single word.
fn find_by_text(
    tree: &obscura_dom::tree::DomTree,
    layout: &obscura_render::DomLayout,
    needle: &str,
) -> Option<(f32, f32)> {
    for (id, runs) in &layout.text_runs {
        if let Some(node) = tree.get_node(*id) {
            if let NodeData::Text { contents } = &node.data {
                if contents.trim() == needle {
                    let x = runs.iter().map(|(r, _)| r.x).fold(f32::INFINITY, f32::min);
                    let y = runs.iter().map(|(r, _)| r.y).fold(f32::INFINITY, f32::min);
                    return Some((x, y));
                }
            }
        }
    }
    None
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
fn long_text_run_wraps_across_multiple_lines() {
    // A single text node with no inline elements breaking it up must still
    // wrap word by word within a narrow container: this is the regression
    // case for treating a whole text node as one indivisible layout box,
    // which cannot wrap internally and instead overflows straight past the
    // container's edge.
    let html = r##"<div style="width:100px">This sentence has plenty of words to wrap across several lines</div>"##;
    let tree = parse_html(html);
    let layout = layout_dom(&tree, (1000.0, 1000.0));

    let text_id = tree
        .descendants(tree.document())
        .into_iter()
        .find(|id| matches!(tree.get_node(*id).map(|n| n.data.clone()), Some(NodeData::Text { .. })))
        .expect("text node exists");
    let runs = layout.text_runs.get(&text_id).expect("text node has word runs");
    assert!(runs.len() > 5, "expected the sentence to split into several word leaves, got {}", runs.len());

    let distinct_y: std::collections::BTreeSet<i32> = runs.iter().map(|(r, _)| r.y.round() as i32).collect();
    assert!(
        distinct_y.len() > 1,
        "words should wrap onto more than one line within a 100px-wide container, got y positions {:?}",
        distinct_y
    );
}

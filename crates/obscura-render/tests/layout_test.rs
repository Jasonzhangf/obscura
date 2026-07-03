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

fn find_by_text<'a>(
    tree: &'a obscura_dom::tree::DomTree,
    layout: &'a obscura_render::DomLayout,
    needle: &str,
) -> Option<(obscura_dom::tree::NodeId, &'a obscura_render::Rect)> {
    for (id, rect) in &layout.rects {
        if let Some(node) = tree.get_node(*id) {
            if let NodeData::Text { contents } = &node.data {
                if contents.trim() == needle {
                    return Some((*id, rect));
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
    let (_, brand_rect) = find_by_text(&tree, &layout, "Hacker News").expect("brand text laid out");
    let (_, headline_rect) = find_by_text(&tree, &layout, "Exapunks (2018)").expect("headline text laid out");
    assert!(
        headline_rect.y > brand_rect.y,
        "headline should be below the header bar: brand.y={} headline.y={}",
        brand_rect.y,
        headline_rect.y
    );

    // Within the header row, "login" (right-aligned cell) sits to the right
    // of "Hacker News" (left cell) — plain flex/table layout, not a magic
    // per-class x-offset.
    let (_, login_rect) = find_by_text(&tree, &layout, "login").expect("login text laid out");
    assert!(
        login_rect.x > brand_rect.x,
        "login cell should be right of the brand: brand.x={} login.x={}",
        brand_rect.x,
        login_rect.x
    );
}

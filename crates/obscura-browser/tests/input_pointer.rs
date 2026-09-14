#![cfg(feature = "render")]

use std::sync::Arc;

use obscura_browser::input::{perform, Input};
use obscura_browser::{BrowserContext, Page};

fn json_value(page: &mut Page, expression: &str) -> serde_json::Value {
    let value = page.evaluate(expression);
    serde_json::from_str(value.as_str().expect("JSON expression returns a string")).unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn typed_pointer_stream_projects_events_and_cancel_clears_the_held_state() {
    let context = Arc::new(BrowserContext::with_storage_and_network(
        "input-pointer".to_owned(),
        None,
        false,
        None,
        None,
        true,
    ));
    let mut page = Page::new("input-pointer-page".to_owned(), context);
    page.set_viewport((200.0, 120.0));
    page.navigate("data:text/html,<div id='target' style='width:100px;height:80px'>pointer</div>")
        .await
        .unwrap();
    page.evaluate(
        r#"(function () {
            const target = document.getElementById('target');
            document.elementFromPoint = () => target;
            window.pointerLog = [];
            for (const type of ['pointerdown','mousedown','pointermove','mousemove','pointerup','mouseup','click','pointercancel']) {
                target.addEventListener(type, event => pointerLog.push({
                    type: event.type,
                    pointerId: event.pointerId,
                    x: event.clientX,
                    y: event.clientY,
                    buttons: event.buttons,
                    shift: event.shiftKey,
                    trusted: event.isTrusted,
                }));
            }
            return true;
        })()"#,
    );

    perform(
        &mut page,
        Input::PointerDown {
            pointer_id: 7,
            x: 10.0,
            y: 20.0,
            button: "left",
            buttons: 1,
            modifiers: 8,
        },
    )
    .await
    .unwrap_or_else(|_| panic!("pointer input dispatch failed"));
    perform(
        &mut page,
        Input::PointerMove {
            pointer_id: 7,
            x: 12.0,
            y: 22.0,
            button: "left",
            buttons: 1,
            modifiers: 8,
        },
    )
    .await
    .unwrap_or_else(|_| panic!("pointer input dispatch failed"));
    perform(
        &mut page,
        Input::PointerUp {
            pointer_id: 7,
            x: 12.0,
            y: 22.0,
            button: "left",
            buttons: 0,
            modifiers: 8,
        },
    )
    .await
    .unwrap_or_else(|_| panic!("pointer input dispatch failed"));

    let log = json_value(&mut page, "JSON.stringify(pointerLog)");
    let types: Vec<&str> = log
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, ["pointerdown", "mousedown", "pointermove", "mousemove", "pointerup", "mouseup", "click"]);
    assert_eq!(log[0]["pointerId"], 7);
    assert_eq!(log[0]["x"], 10.0);
    assert_eq!(log[0]["y"], 20.0);
    assert_eq!(log[0]["buttons"], 1);
    assert_eq!(log[0]["shift"], true);
    assert!(log.as_array().unwrap().iter().all(|event| event["trusted"] == true));
    assert_eq!(json_value(&mut page, "JSON.stringify({pointer: globalThis.__obscura_pointer_down, mouse: globalThis.__obscura_mouse_down})"), serde_json::json!({"pointer": null, "mouse": null}));

    page.evaluate("pointerLog = []");
    perform(
        &mut page,
        Input::PointerDown {
            pointer_id: 8,
            x: 15.0,
            y: 25.0,
            button: "left",
            buttons: 1,
            modifiers: 0,
        },
    )
    .await
    .unwrap_or_else(|_| panic!("pointer input dispatch failed"));
    perform(
        &mut page,
        Input::PointerCancel {
            pointer_id: 8,
            x: 15.0,
            y: 25.0,
            button: "none",
            buttons: 0,
            modifiers: 0,
        },
    )
    .await
    .unwrap_or_else(|_| panic!("pointer input dispatch failed"));
    let cancelled = json_value(&mut page, "JSON.stringify(pointerLog)");
    let cancelled_types: Vec<&str> = cancelled
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect();
    assert_eq!(cancelled_types, ["pointerdown", "mousedown", "pointercancel"]);
    assert_eq!(json_value(&mut page, "JSON.stringify({pointer: globalThis.__obscura_pointer_down, mouse: globalThis.__obscura_mouse_down, click: pointerLog.some(e => e.type === 'click')})"), serde_json::json!({"pointer": null, "mouse": null, "click": false}));
}

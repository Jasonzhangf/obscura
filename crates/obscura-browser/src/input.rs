use serde_json::{json, Value};
use crate::Page;

// Unlike Page::evaluate, input dispatch must not convert a failed injection to
// successful null. The owning caller supplies its watchdog/deadline policy.
fn evaluate(page: &mut Page, expression: &str) -> Result<(), String> {
    page.js.as_mut().ok_or("Input runtime unavailable")?.evaluate(expression)?;
    Ok(())
}

pub enum Input<'a> {
    Click { x: f64, y: f64 },
    Text(&'a str),
    Scroll { x: f64, y: f64, delta_x: f64, delta_y: f64 },
    Key { key: &'a str, code: &'a str, modifiers: u64, text: &'a str },
    PointerDown { pointer_id: u64, x: f64, y: f64, button: &'a str, buttons: u64, modifiers: u64 },
    PointerMove { pointer_id: u64, x: f64, y: f64, button: &'a str, buttons: u64, modifiers: u64 },
    PointerUp { pointer_id: u64, x: f64, y: f64, button: &'a str, buttons: u64, modifiers: u64 },
    PointerCancel { pointer_id: u64, x: f64, y: f64, button: &'a str, buttons: u64, modifiers: u64 },
}
pub enum InputError { Rejected(String), Unknown(String) }

/// Complete input injection only. Navigation and page tasks are owned by the
/// caller's event loop; they are not part of a held pointer/text operation.
pub async fn perform(page: &mut Page, input: Input<'_>) -> Result<(), InputError> {
    match input {
        Input::Click { x, y } => {
            dispatch("dispatchMouseEvent", &json!({"type":"mousePressed","x":x,"y":y,"button":"left"}), page).await.map_err(InputError::Unknown)?;
            dispatch("dispatchMouseEvent", &json!({"type":"mouseReleased","x":x,"y":y,"button":"left"}), page).await.map_err(InputError::Unknown)?;
        }
        Input::Text(text) => {
            let js = page.js.as_mut().ok_or_else(|| InputError::Rejected("Input runtime unavailable".into()))?;
            let editable = js.evaluate("(function(){var t=document.activeElement;return !!(t && (t.localName==='input'||t.localName==='textarea') && !t.readOnly && !t.hasAttribute('readonly') && !globalThis.__obscura_isDisabled(t) && (t.localName==='textarea'||['text','search','email','url','tel','password','number'].includes((t.type||'text').toLowerCase())))})()")
                .map_err(InputError::Unknown)?;
            if editable != Value::Bool(true) { return Err(InputError::Rejected("Focus an enabled, writable text input or textarea".into())); }
            dispatch("insertText", &json!({"text":text}), page).await.map_err(InputError::Unknown)?;
        }
        Input::Scroll { x, y, delta_x, delta_y } => {
            dispatch("dispatchMouseEvent", &json!({"type":"mouseWheel","x":x,"y":y,"deltaX":delta_x,"deltaY":delta_y}), page).await.map_err(InputError::Unknown)?;
        }
        Input::Key { key, code, modifiers, text } => {
            dispatch("dispatchKeyEvent", &json!({"type":"keyDown","key":key,"code":code,"modifiers":modifiers,"text":text}), page)
                .await.map_err(InputError::Unknown)?;
            dispatch("dispatchKeyEvent", &json!({"type":"keyUp","key":key,"code":code,"modifiers":modifiers}), page)
                .await.map_err(InputError::Unknown)?;
        }
        Input::PointerDown { pointer_id, x, y, button, buttons, modifiers } => {
            dispatch("dispatchPointerEvent", &json!({
                "type":"pointerdown", "pointerId":pointer_id, "x":x, "y":y,
                "button":button, "buttons":buttons, "modifiers":modifiers,
            }), page).await.map_err(InputError::Unknown)?;
            dispatch("dispatchMouseEvent", &json!({
                "type":"mousePressed", "x":x, "y":y, "button":button,
                "buttons":buttons, "modifiers":modifiers,
            }), page).await.map_err(InputError::Unknown)?;
        }
        Input::PointerMove { pointer_id, x, y, button, buttons, modifiers } => {
            dispatch("dispatchPointerEvent", &json!({
                "type":"pointermove", "pointerId":pointer_id, "x":x, "y":y,
                "button":button, "buttons":buttons, "modifiers":modifiers,
            }), page).await.map_err(InputError::Unknown)?;
            dispatch("dispatchMouseEvent", &json!({
                "type":"mouseMoved", "x":x, "y":y, "button":button,
                "buttons":buttons, "modifiers":modifiers,
            }), page).await.map_err(InputError::Unknown)?;
        }
        Input::PointerUp { pointer_id, x, y, button, buttons, modifiers } => {
            dispatch("dispatchPointerEvent", &json!({
                "type":"pointerup", "pointerId":pointer_id, "x":x, "y":y,
                "button":button, "buttons":buttons, "modifiers":modifiers,
            }), page).await.map_err(InputError::Unknown)?;
            dispatch("dispatchMouseEvent", &json!({
                "type":"mouseReleased", "x":x, "y":y, "button":button,
                "buttons":buttons, "modifiers":modifiers,
            }), page).await.map_err(InputError::Unknown)?;
        }
        Input::PointerCancel { pointer_id, x, y, button, buttons, modifiers } => {
            dispatch("dispatchPointerEvent", &json!({
                "type":"pointercancel", "pointerId":pointer_id, "x":x, "y":y,
                "button":button, "buttons":buttons, "modifiers":modifiers,
            }), page).await.map_err(InputError::Unknown)?;
        }
    }
    Ok(())
}

/// Embed a string as a JS string literal (double-quoted, with backslash,
/// quotes, and control characters escaped) for interpolation into generated
/// KeyboardEvent scripts. A plain `replace('\'', ...)` misses newline / NUL /
/// U+2028-29, which terminate the literal and silently drop the event.
fn js_str(s: &str) -> String {
    serde_json::to_string(s).expect("String serialization is infallible")
}

// Insert `text` at the caret, replacing any non-collapsed selection the way a
// real browser does when you type over selected text (for example after a
// triple-click select-all). selectionStart is null during ordinary typing, so
// the legacy append path is kept when no selection is tracked.
//
// The text is embedded as a JSON string literal rather than escaped by hand
// into single quotes. JSON string syntax is a subset of JavaScript's, so this
// covers the quote and the backslash of issue #433 and the control characters
// they left out: a newline inside a single-quoted literal is a syntax error,
// so the whole snippet was dropped and nothing was inserted. obscura-mcp
// already builds its typing snippet this way.
fn insert_text_js(text: &str) -> String {
    let literal = serde_json::to_string(text).expect("String serialization is infallible");
    format!(
        "(function() {{\
            var t = document.activeElement;\
            if (!t || (t.localName !== 'input' && t.localName !== 'textarea')) return;\
            var ins = {text};\
            var v = t.value || '';\
            var s = t.selectionStart, e = t.selectionEnd;\
            if (s == null) {{\
                globalThis.__obscura_setFieldValue(t, 'value', v + ins);\
            }} else {{\
                s = Math.max(0, Math.min(s, v.length));\
                e = (e == null) ? s : Math.max(0, Math.min(e, v.length));\
                var lo = Math.min(s, e), hi = Math.max(s, e);\
                globalThis.__obscura_setFieldValue(t, 'value', v.slice(0, lo) + ins + v.slice(hi));\
                var caret = lo + ins.length;\
                t.setSelectionRange(caret, caret);\
            }}\
            t.dispatchEvent(globalThis.__obscura_markTrusted(new Event('input', {{bubbles:true}})));\
        }})()",
        text = literal,
    )
}

// Backspace deletes the selected range when there is one, so the common
// "triple-click to select-all, then Backspace to clear" pattern works. With a
// collapsed caret it removes the character before the caret, and with no
// selection tracked it falls back to trimming the last character (legacy).
const BACKSPACE_JS: &str = "(function() {\
    var t = document.activeElement;\
    if (!t || (t.localName !== 'input' && t.localName !== 'textarea')) return;\
    var v = t.value || '';\
    var s = t.selectionStart, e = t.selectionEnd;\
    if (s == null) {\
        globalThis.__obscura_setFieldValue(t, 'value', v.slice(0, -1));\
    } else {\
        s = Math.max(0, Math.min(s, v.length));\
        e = (e == null) ? s : Math.max(0, Math.min(e, v.length));\
        if (s !== e) {\
            var lo = Math.min(s, e), hi = Math.max(s, e);\
            globalThis.__obscura_setFieldValue(t, 'value', v.slice(0, lo) + v.slice(hi));\
            t.setSelectionRange(lo, lo);\
        } else if (s > 0) {\
            globalThis.__obscura_setFieldValue(t, 'value', v.slice(0, s - 1) + v.slice(s));\
            t.setSelectionRange(s - 1, s - 1);\
        }\
    }\
    t.dispatchEvent(globalThis.__obscura_markTrusted(new Event('input', {bubbles:true})));\
})()";

fn mouse_button_code(button: &str) -> u8 {
    match button {
        "middle" => 1,
        "right" => 2,
        "back" => 3,
        "forward" => 4,
        _ => 0,
    }
}

fn mouse_button_mask(button: &str) -> u64 {
    match button {
        "right" => 2,
        "middle" => 4,
        "back" => 8,
        "forward" => 16,
        "none" => 0,
        _ => 1,
    }
}

fn modifier_flags(modifiers: u64) -> (bool, bool, bool, bool) {
    // CDP Input.Modifier: Alt=1, Ctrl=2, Meta=4, Shift=8.
    (
        modifiers & 1 != 0,
        modifiers & 2 != 0,
        modifiers & 4 != 0,
        modifiers & 8 != 0,
    )
}

pub async fn dispatch(
    method: &str,
    params: &Value,
    page: &mut Page,
) -> Result<(), String> {
    match method {
        "dispatchPointerEvent" => {
            let event_type = params.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let event_name = match event_type {
                "pointerdown" | "pointermove" | "pointerup" | "pointercancel" => event_type,
                _ => return Err(format!("Unknown pointer event type: {event_type}")),
            };
            let pointer_id = params.get("pointerId").and_then(|v| v.as_u64()).unwrap_or(0);
            let x = params.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let y = params.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let button = params.get("button").and_then(|v| v.as_str()).unwrap_or("none");
            let button_code = mouse_button_code(button);
            let buttons = params.get("buttons").and_then(|v| v.as_u64()).unwrap_or(0);
            let modifiers = params.get("modifiers").and_then(|v| v.as_u64()).unwrap_or(0);
            let (alt_key, ctrl_key, meta_key, shift_key) = modifier_flags(modifiers);
            let target = match event_name {
                "pointerdown" => format!(
                    "(document.elementFromPoint && document.elementFromPoint({x},{y})) || document.body || document.documentElement",
                    x = x,
                    y = y,
                ),
                "pointercancel" => format!(
                    "(state && state.pointerId === {pointer_id} && state.target) || (document.elementFromPoint && document.elementFromPoint({x},{y})) || document.body || document.documentElement",
                    pointer_id = pointer_id,
                    x = x,
                    y = y,
                ),
                _ => format!(
                    "(document.elementFromPoint && document.elementFromPoint({x},{y})) || (state && state.pointerId === {pointer_id} && state.target) || document.body || document.documentElement",
                    pointer_id = pointer_id,
                    x = x,
                    y = y,
                ),
            };
            let terminal = event_name == "pointerup" || event_name == "pointercancel";
            let cancelled = event_name == "pointercancel";
            let code = format!(
                "(function() {{\
                    var state = globalThis.__obscura_pointer_down;\
                    var target = {target};\
                    if (!target) return;\
                    if ('{event_name}' === 'pointerdown') {{\
                        globalThis.__obscura_pointer_down = {{pointerId:{pointer_id},target:target,x:{x},y:{y}}};\
                        globalThis.__obscura_click_target = target;\
                    }}\
                    var evt = globalThis.__obscura_markTrusted(new PointerEvent('{event_name}', {{bubbles:true,cancelable:true,view:globalThis,clientX:{x},clientY:{y}}}));\
                    evt.pointerId = {pointer_id};\
                    evt.pointerType = 'mouse';\
                    evt.isPrimary = true;\
                    evt.clientX = {x};\
                    evt.clientY = {y};\
                    evt.button = {button_code};\
                    evt.buttons = {buttons};\
                    evt.altKey = {alt_key};\
                    evt.ctrlKey = {ctrl_key};\
                    evt.metaKey = {meta_key};\
                    evt.shiftKey = {shift_key};\
                    target.dispatchEvent(evt);\
                    try {{                        var touchType = '{event_name}' === 'pointerdown' ? 'touchstart' : '{event_name}' === 'pointermove' ? 'touchmove' : '{event_name}' === 'pointerup' ? 'touchend' : null;                        if (touchType && typeof TouchEvent === 'function') {{                            var t = new Touch({{identifier:{pointer_id},target:target,clientX:{x},clientY:{y},pageX:{x},pageY:{y},radiusX:1,radiusY:1,rotationAngle:0,force:0}});                            var touchEvt = globalThis.__obscura_markTrusted(new TouchEvent(touchType, {{bubbles:true,cancelable:true,view:globalThis,clientX:{x},clientY:{y},altKey:{alt_key},ctrlKey:{ctrl_key},metaKey:{meta_key},shiftKey:{shift_key}}}));                            Object.defineProperty(touchEvt, 'touches', {{value: touchType === 'touchend' ? [] : [t]}});                            Object.defineProperty(touchEvt, 'targetTouches', {{value: touchType === 'touchend' ? [] : [t]}});                            Object.defineProperty(touchEvt, 'changedTouches', {{value: [t]}});                            target.dispatchEvent(touchEvt);                        }}                    }} catch(_e) {{}}\
                    if ({terminal} && state && state.pointerId === {pointer_id}) {{\
                        globalThis.__obscura_pointer_down = null;\
                        if ({cancelled}) {{\
                            globalThis.__obscura_mouse_down = null;\
                            globalThis.__obscura_click_target = null;\
                        }}\
                    }}\
                    if ('{event_name}' === 'pointermove' && {buttons} > 0) {{\
                        var down = globalThis.__obscura_pointer_down;\
                        if (down && down.pointerId === {pointer_id} && down.x != null) {{\
                            var dragX = {x} - down.x;\
                            var dragY = {y} - down.y;\
                            var target = (document.elementFromPoint && document.elementFromPoint({x},{y})) || down.target || document.body || document.documentElement;\
                            var scrollEl = null;\
                            var el = target;\
                            while (el && el.nodeType === 1) {{\
                                var overflowX = '', overflowY = '', scrollWidth = 0, clientWidth = 0, scrollHeight = 0, clientHeight = 0;\
                                try {{\
                                    var style = getComputedStyle(el);\
                                    overflowX = style.overflowX || style.overflow || '';\
                                    overflowY = style.overflowY || style.overflow || '';\
                                    scrollWidth = el.scrollWidth || 0;\
                                    clientWidth = el.clientWidth || 0;\
                                    scrollHeight = el.scrollHeight || 0;\
                                    clientHeight = el.clientHeight || 0;\
                                }} catch (_e) {{ break; }}\
                                var canX = (overflowX === 'auto' || overflowX === 'scroll' || overflowX === 'overlay') && scrollWidth > clientWidth;\
                                var canY = (overflowY === 'auto' || overflowY === 'scroll' || overflowY === 'overlay') && scrollHeight > clientHeight;\
                                if (canX || canY) {{ scrollEl = el; break; }}\
                                el = el.parentElement;\
                            }}\
                            if (!scrollEl) {{\
                                var root = document.scrollingElement || document.documentElement || document.body;\
                                if (root && ((root.scrollWidth || 0) > (root.clientWidth || 0) || (root.scrollHeight || 0) > (root.clientHeight || 0))) scrollEl = root;\
                            }}\
                            if (scrollEl && typeof scrollEl.scrollBy === 'function') scrollEl.scrollBy(-dragX, -dragY);\
                            down.x = {x};\
                            down.y = {y};\
                        }}\
                    }}\
                }})()",
                target = target,
                event_name = event_name,
                pointer_id = pointer_id,
                x = x,
                y = y,
                button_code = button_code,
                buttons = buttons,
                alt_key = alt_key,
                ctrl_key = ctrl_key,
                meta_key = meta_key,
                shift_key = shift_key,
                terminal = terminal,
                cancelled = cancelled,
            );
            evaluate(page, &code)?;
            Ok(())
        }
        "dispatchMouseEvent" => {
            let event_type = params.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let x = params.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let y = params.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let button = params.get("button").and_then(|v| v.as_str()).unwrap_or("left");
            let button_code = mouse_button_code(button);
            let buttons = params
                .get("buttons")
                .and_then(|v| v.as_u64())
                .unwrap_or_else(|| mouse_button_mask(button));
            let click_count = params.get("clickCount").and_then(|v| v.as_u64()).unwrap_or(1);
            let modifiers = params.get("modifiers").and_then(|v| v.as_u64()).unwrap_or(0);
            let (alt_key, ctrl_key, meta_key, shift_key) = modifier_flags(modifiers);

            if event_type == "mousePressed" {
                {
                    let code = format!(
                        "(function() {{\
                            var target = (document.elementFromPoint && document.elementFromPoint({x},{y})) || globalThis.__obscura_click_target || document.activeElement || document.body;\
                            if (!target) return;\
                            globalThis.__obscura_click_target = target;\
                            globalThis.__obscura_mouse_down = {{target:target,button:{button_code},clickCount:{click_count}}};\
                            var evt = globalThis.__obscura_markTrusted(new MouseEvent('mousedown', {{bubbles:true,cancelable:true,view:globalThis,clientX:{x},clientY:{y},button:{button_code},buttons:{buttons},detail:{click_count},altKey:{alt_key},ctrlKey:{ctrl_key},metaKey:{meta_key},shiftKey:{shift_key}}}));\
                            var allowed = target.dispatchEvent(evt);\
                            var focusTarget = target.closest ? target.closest('input,textarea,button,select,a[href],[tabindex]') : null;\
                            if (allowed && focusTarget && !globalThis.__obscura_isDisabled(focusTarget)) focusTarget.focus();\
                        }})()",
                        x = x,
                        y = y,
                        button_code = button_code,
                        buttons = buttons,
                        click_count = click_count,
                        alt_key = alt_key,
                        ctrl_key = ctrl_key,
                        meta_key = meta_key,
                        shift_key = shift_key,
                    );
                    evaluate(page, &code)?;
                }
            } else if event_type == "mouseReleased" {
                {
                    let code = format!(
                        "(function() {{\
                            var target = (document.elementFromPoint && document.elementFromPoint({x},{y})) || globalThis.__obscura_click_target || document.activeElement || document.body;\
                            if (!target) return;\
                            var down = globalThis.__obscura_mouse_down;\
                            globalThis.__obscura_mouse_down = null;\
                            var evt = globalThis.__obscura_markTrusted(new MouseEvent('mouseup', {{bubbles:true,cancelable:true,view:globalThis,clientX:{x},clientY:{y},button:{button_code},buttons:0,detail:{click_count},altKey:{alt_key},ctrlKey:{ctrl_key},metaKey:{meta_key},shiftKey:{shift_key}}}));\
                            target.dispatchEvent(evt);\
                            if (!down || down.button !== {button_code} || {button_code} !== 0) return;\
                            var clickTarget = down.target;\
                            while (clickTarget && clickTarget !== target && !(clickTarget.contains && clickTarget.contains(target))) {{\
                                clickTarget = clickTarget.parentElement;\
                            }}\
                            if (!clickTarget) return;\
                            var tag = clickTarget.tagName;\
                            var type = (clickTarget.getAttribute && clickTarget.getAttribute('type') || '').toLowerCase();\
                            if (globalThis.__obscura_isDisabled(clickTarget)) return;\
                            var checkable = tag === 'INPUT' && (type === 'checkbox' || type === 'radio');\
                            var oldChecked = checkable ? !!clickTarget.checked : false;\
                            var oldIndeterminate = checkable ? !!clickTarget.indeterminate : false;\
                            var radioStates = null;\
                            if (checkable && type === 'radio') {{\
                                var radioName = clickTarget.getAttribute('name') || '';\
                                if (radioName) {{\
                                    var candidates = document.querySelectorAll('input');\
                                    radioStates = [];\
                                    for (var ri = 0; ri < candidates.length; ri++) {{\
                                        var radio = candidates[ri];\
                                        if ((radio.getAttribute('type') || '').toLowerCase() !== 'radio' || (radio.getAttribute('name') || '') !== radioName || radio.form !== clickTarget.form) continue;\
                                        radioStates.push([radio, !!radio.checked]);\
                                        if (radio !== clickTarget) radio.checked = false;\
                                    }}\
                                }}\
                                clickTarget.checked = true;\
                            }} else if (checkable) {{\
                                clickTarget.checked = !oldChecked;\
                                clickTarget.indeterminate = false;\
                            }}\
                            var click = globalThis.__obscura_markTrusted(new MouseEvent('click', {{bubbles:true,cancelable:true,view:globalThis,clientX:{x},clientY:{y},button:0,buttons:0,detail:{click_count},altKey:{alt_key},ctrlKey:{ctrl_key},metaKey:{meta_key},shiftKey:{shift_key}}}));\
                            var cancelled = !clickTarget.dispatchEvent(click);\
                            if (cancelled) {{\
                                if (radioStates) {{\
                                    for (var rr = 0; rr < radioStates.length; rr++) radioStates[rr][0].checked = radioStates[rr][1];\
                                }} else if (checkable) {{ clickTarget.checked = oldChecked; clickTarget.indeterminate = oldIndeterminate; }}\
                                return;\
                            }}\
                            if (checkable && clickTarget.checked !== oldChecked) {{\
                                try {{ clickTarget.dispatchEvent(globalThis.__obscura_markTrusted(new Event('input', {{bubbles:true}}))); }} catch(e) {{}}\
                                try {{ clickTarget.dispatchEvent(globalThis.__obscura_markTrusted(new Event('change', {{bubbles:true}}))); }} catch(e) {{}}\
                                return;\
                            }}\
                            var labelHost = tag === 'LABEL' ? clickTarget : (clickTarget.closest ? clickTarget.closest('label') : null);\
                            var interactiveHost = globalThis.__obscura_interactiveHost(clickTarget);\
                            if (labelHost && !(interactiveHost && labelHost.contains(interactiveHost))) {{\
                                var ctl = globalThis.__obscura_labeledControl(labelHost);\
                                if (ctl && ctl !== clickTarget && globalThis.__obscura_activateLabel(labelHost, ctl, true)) {{ return; }}\
                            }}\
                            var link = clickTarget.closest ? clickTarget.closest('a[href]') : null;\
                            if (!link && tag === 'A' && clickTarget.getAttribute('href')) link = clickTarget;\
                            if (link) {{\
                                var href = link.getAttribute('href');\
                                if (href && !href.startsWith('#') && !href.startsWith('javascript:')) location.assign(href);\
                            }} else if (tag === 'BUTTON' && type !== 'button' && type !== 'reset') {{\
                                var form = clickTarget.closest ? clickTarget.closest('form') : null;\
                                if (form) {{ try {{ if (typeof form.requestSubmit === 'function') {{ form.requestSubmit(clickTarget); }} else {{ form.submit(clickTarget); }} }} catch(e) {{}} }}\
                            }} else if (tag === 'INPUT' && (type === 'submit' || type === 'image')) {{\
                                var form2 = clickTarget.closest ? clickTarget.closest('form') : null;\
                                if (form2) {{ try {{ if (typeof form2.requestSubmit === 'function') {{ form2.requestSubmit(clickTarget); }} else {{ form2.submit(clickTarget); }} }} catch(e) {{}} }}\
                            }} else if ({click_count} >= 3 && (tag === 'INPUT' || tag === 'TEXTAREA')) {{\
                                var len = clickTarget.value ? clickTarget.value.length : 0;\
                                if (clickTarget.setSelectionRange) clickTarget.setSelectionRange(0, len);\
                                else {{ clickTarget.selectionStart = 0; clickTarget.selectionEnd = len; }}\
                            }}\
                        }})()",
                        x = x,
                        y = y,
                        button_code = button_code,
                        click_count = click_count,
                        alt_key = alt_key,
                        ctrl_key = ctrl_key,
                        meta_key = meta_key,
                        shift_key = shift_key,
                    );
                    evaluate(page, &code)?;
                }
            } else if event_type == "mouseMoved" {
                {
                    let code = format!(
                        "(function() {{\
                            var target = (document.elementFromPoint && document.elementFromPoint({x},{y})) || globalThis.__obscura_click_target || document.activeElement || document.body;\
                            if (!target) return;\
                            var evt = globalThis.__obscura_markTrusted(new MouseEvent('mousemove', {{bubbles:true,cancelable:true,view:globalThis,clientX:{x},clientY:{y},button:{button_code},buttons:{buttons},altKey:{alt_key},ctrlKey:{ctrl_key},metaKey:{meta_key},shiftKey:{shift_key}}}));\
                            target.dispatchEvent(evt);\
                        }})()",
                        x = x,
                        y = y,
                        button_code = button_code,
                        buttons = buttons,
                        alt_key = alt_key,
                        ctrl_key = ctrl_key,
                        meta_key = meta_key,
                        shift_key = shift_key,
                    );
                    evaluate(page, &code)?;
                }
            } else if event_type == "mouseWheel" {
                let delta_x = params.get("deltaX").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let delta_y = params.get("deltaY").and_then(|v| v.as_f64()).unwrap_or(0.0);
                {
                    let code = format!(
                        "(function() {{\
                            var target = (document.elementFromPoint && document.elementFromPoint({x},{y})) || document.body || document.documentElement;\
                            if (!target) return;\
                            var wheel = globalThis.__obscura_markTrusted(new WheelEvent('wheel', {{bubbles:true,cancelable:true,view:globalThis,clientX:{x},clientY:{y},deltaX:{delta_x},deltaY:{delta_y},deltaMode:0,altKey:{alt_key},ctrlKey:{ctrl_key},metaKey:{meta_key},shiftKey:{shift_key}}}));\
                            if (!target.dispatchEvent(wheel)) return;\
                            var dx = {delta_x}, dy = {delta_y};\
                            var root = document.scrollingElement || document.documentElement || document.body;\
                            var scrollTarget = null;\
                            var el = target;\
                            while (el && el.nodeType === 1 && el !== root && el !== document.body && el !== document.documentElement) {{\
                                var maxX = Math.max(0, (el.scrollWidth || 0) - (el.clientWidth || 0));\
                                var maxY = Math.max(0, (el.scrollHeight || 0) - (el.clientHeight || 0));\
                                var style = null;\
                                try {{ style = getComputedStyle(el); }} catch (_e) {{}}\
                                var ox = style ? (style.overflowX || style.overflow || '') : '';\
                                var oy = style ? (style.overflowY || style.overflow || '') : '';\
                                var allowX = ox === 'auto' || ox === 'scroll' || ox === 'overlay';\
                                var allowY = oy === 'auto' || oy === 'scroll' || oy === 'overlay';\
                                var consumesX = allowX && ((dx > 0 && el.scrollLeft < maxX) || (dx < 0 && el.scrollLeft > 0));\
                                var consumesY = allowY && ((dy > 0 && el.scrollTop < maxY) || (dy < 0 && el.scrollTop > 0));\
                                if (consumesX || consumesY) {{ scrollTarget = el; break; }}\
                                el = el.parentElement;\
                            }}\
                            if (!scrollTarget) scrollTarget = root;\
                            if (scrollTarget === root && root && typeof root.scrollBy === 'function') {{\
                                var beforeX = root.scrollLeft, beforeY = root.scrollTop;\
                                root.scrollBy(dx, dy);\
                                if (root.scrollLeft !== beforeX || root.scrollTop !== beforeY) setTimeout(function() {{\
                                    try {{ document.dispatchEvent(new Event('scroll', {{bubbles:false}})); }} catch (_e) {{}}\
                                    try {{ globalThis.dispatchEvent(new Event('scroll', {{bubbles:false}})); }} catch (_e) {{}}\
                                }}, 0);\
                            }} else if (scrollTarget && typeof scrollTarget.scrollBy === 'function') scrollTarget.scrollBy(dx, dy);\
                        }})()",
                        x = x,
                        y = y,
                        delta_x = delta_x,
                        delta_y = delta_y,
                        alt_key = alt_key,
                        ctrl_key = ctrl_key,
                        meta_key = meta_key,
                        shift_key = shift_key,
                    );
                    evaluate(page, &code)?;
                }
            }

            Ok(())
        }
        // Chrome's Input.insertText: Playwright's fill() focuses the field in
        // page and then types the whole value through this one call (#577).
        "insertText" => {
            let text = params.get("text").and_then(|v| v.as_str()).unwrap_or("");
            evaluate(page, &insert_text_js(text))?;
            Ok(())
        }
        "dispatchKeyEvent" => {
            let event_type = params.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let key = params.get("key").and_then(|v| v.as_str()).unwrap_or("");
            let code = params.get("code").and_then(|v| v.as_str()).unwrap_or("");
            let text = params.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let modifiers = params.get("modifiers").and_then(|v| v.as_u64()).unwrap_or(0);
            let (alt_key, ctrl_key, meta_key, shift_key) = modifier_flags(modifiers);

            {
                match event_type {
                    "keyDown" | "rawKeyDown" => {
                        let js = format!(
                            "(function() {{\
                                var target = document.activeElement || document.body;\
                                var evt = globalThis.__obscura_markTrusted(new KeyboardEvent('keydown', {{bubbles:true,cancelable:true,key:{key},code:{code},altKey:{alt_key},ctrlKey:{ctrl_key},metaKey:{meta_key},shiftKey:{shift_key}}}));\
                                target.dispatchEvent(evt);\
                            }})()",
                            // Escape backslash BEFORE single-quote (as the text
                            // path below does) so a key like "\" — Chrome's
                            // backslash key — doesn't escape the closing quote
                            // and produce a syntax error that drops the event.
                            key = js_str(key),
                            code = js_str(code),
                            alt_key = alt_key,
                            ctrl_key = ctrl_key,
                            meta_key = meta_key,
                            shift_key = shift_key,
                        );
                        evaluate(page, &js)?;

                        if !text.is_empty() && text != "\r" && text != "\n" {
                            evaluate(page, &insert_text_js(text))?;
                        }

                        if key == "Enter" {
                            // In a textarea Enter inserts a newline; in input fields
                            // it submits the containing form. Real Chrome distinguishes
                            // these two and we should too: previously every Enter tried
                            // to submit the nearest form even from a textarea.
                            let js = format!("(function() {{\
                                var target = document.activeElement;\
                                if (!target) return;\
                                target.dispatchEvent(globalThis.__obscura_markTrusted(new KeyboardEvent('keypress', {{bubbles:true,key:'Enter',code:'Enter',altKey:{alt_key},ctrlKey:{ctrl_key},metaKey:{meta_key},shiftKey:{shift_key}}})));\
                                if (target.localName === 'textarea') {{\
                                    globalThis.__obscura_setFieldValue(target, 'value', (target.value || '') + '\\n');\
                                    target.dispatchEvent(globalThis.__obscura_markTrusted(new Event('input', {{bubbles:true}})));\
                                }} else {{\
                                    var form = target.form || (target.closest && target.closest('form'));\
                                    if (form) {{ try {{ if (typeof form.requestSubmit === 'function') {{ form.requestSubmit(); }} else {{ form.submit(); }} }} catch(e) {{}} }}\
                                }}\
                            }})()", alt_key = alt_key, ctrl_key = ctrl_key, meta_key = meta_key, shift_key = shift_key);
                            evaluate(page, &js)?;
                        }

                        if key == "Backspace" {
                            evaluate(page, BACKSPACE_JS)?;
                        }
                    }
                    "keyUp" => {
                        let js = format!(
                            "(function() {{\
                                var target = document.activeElement || document.body;\
                                var evt = globalThis.__obscura_markTrusted(new KeyboardEvent('keyup', {{bubbles:true,key:{key},code:{code},altKey:{alt_key},ctrlKey:{ctrl_key},metaKey:{meta_key},shiftKey:{shift_key}}}));\
                                target.dispatchEvent(evt);\
                            }})()",
                            key = js_str(key),
                            code = js_str(code),
                            alt_key = alt_key,
                            ctrl_key = ctrl_key,
                            meta_key = meta_key,
                            shift_key = shift_key,
                        );
                        evaluate(page, &js)?;
                    }
                    "char" => {
                        if !text.is_empty() {
                            evaluate(page, &insert_text_js(text))?;
                            // Pump event loop so Angular change detection picks up the input
                            page.settle(50).await;
                        }
                    }
                    _ => {}
                }
            }

            Ok(())
        }
        _ => Err(format!("Unknown Input method: {}", method)),
    }
}

#[cfg(test)]
mod tests {
    use super::js_str;

    // SEC-501 / #819 — key/code are embedded via js_str; it must escape control
    // characters (newline/CR/tab/NUL/U+2028-29), not just backslash and quote,
    // so a control char cannot terminate the literal and drop the event.
    #[test]
    fn js_str_escapes_control_characters() {
        let lit = js_str("a\nb\r\t'c\\d\"e");
        assert!(
            !lit.contains('\n') && !lit.contains('\r') && !lit.contains('\t'),
            "control characters must be escaped, not left raw: {lit:?}"
        );
        // The result must be a valid JS/JSON string literal that round-trips.
        let decoded: String =
            serde_json::from_str(&lit).expect("the literal must be valid JSON");
        assert_eq!(decoded, "a\nb\r\t'c\\d\"e");
    }
}

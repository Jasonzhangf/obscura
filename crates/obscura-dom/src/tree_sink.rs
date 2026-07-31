use std::borrow::Cow;
use std::cell::Ref;
use std::fmt;

use html5ever::tendril::StrTendril;
use html5ever::tree_builder::{ElemName, ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::{Attribute as HtmlAttribute, LocalName, Namespace, QualName};

use crate::tree::{Attribute, DomTree, NodeData, NodeId};

pub struct ObscuraElemName<'a> {
    _ref: Ref<'a, ()>,
    name: *const QualName,
}

impl<'a> fmt::Debug for ObscuraElemName<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = unsafe { &*self.name };
        write!(f, "{:?}", name)
    }
}

impl<'a> ElemName for ObscuraElemName<'a> {
    fn ns(&self) -> &Namespace {
        unsafe { &(*self.name).ns }
    }

    fn local_name(&self) -> &LocalName {
        unsafe { &(*self.name).local }
    }
}

impl TreeSink for DomTree {
    type Handle = NodeId;
    type Output = Self;
    type ElemName<'a> = ObscuraElemName<'a>;

    fn finish(self) -> Self::Output {
        self
    }

    fn parse_error(&self, _msg: Cow<'static, str>) {}

    fn get_document(&self) -> NodeId {
        self.document()
    }

    fn elem_name<'a>(&'a self, target: &'a NodeId) -> ObscuraElemName<'a> {
        let borrow = self.borrow_inner();
        let node = borrow.nodes.get(target.index())
            .and_then(|n| n.as_ref())
            .expect("elem_name called on invalid node");
        let name_ptr: *const QualName = match &node.data {
            NodeData::Element { name, .. } => name as *const QualName,
            _ => panic!("elem_name called on non-element"),
        };
        let ref_guard = Ref::map(borrow, |_| &());
        ObscuraElemName {
            _ref: ref_guard,
            name: name_ptr,
        }
    }

    fn create_element(
        &self,
        name: QualName,
        attrs: Vec<HtmlAttribute>,
        flags: ElementFlags,
    ) -> NodeId {
        let converted_attrs: Vec<Attribute> = attrs
            .into_iter()
            .map(|a| Attribute {
                name: a.name,
                value: a.value.to_string(),
            })
            .collect();

        let id = self.new_node(NodeData::Element {
            name: name.clone(),
            attrs: converted_attrs,
            template_contents: None,
            mathml_annotation_xml_integration_point: flags.mathml_annotation_xml_integration_point,
        });

        if flags.template {
            let template_doc = self.new_node(NodeData::Document);
            self.with_node_mut(id, |node| {
                if let NodeData::Element { template_contents, .. } = &mut node.data {
                    *template_contents = Some(template_doc);
                }
            });
        }

        id
    }

    fn create_comment(&self, text: StrTendril) -> NodeId {
        self.new_node(NodeData::Comment {
            contents: text.to_string(),
        })
    }

    fn create_pi(&self, target: StrTendril, data: StrTendril) -> NodeId {
        self.new_node(NodeData::ProcessingInstruction {
            target: target.to_string(),
            data: data.to_string(),
        })
    }

    fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
        match child {
            NodeOrText::AppendNode(node_id) => {
                self.append_child(*parent, node_id);
            }
            NodeOrText::AppendText(text) => {
                self.append_text(*parent, &text);
            }
        }
    }

    fn append_based_on_parent_node(
        &self,
        element: &NodeId,
        prev_element: &NodeId,
        child: NodeOrText<NodeId>,
    ) {
        let has_parent = self.with_node(*element, |n| n.parent.is_some()).unwrap_or(false);
        if has_parent {
            self.append_before_sibling(element, child);
        } else {
            self.append(prev_element, child);
        }
    }

    fn append_doctype_to_document(
        &self,
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    ) {
        let doctype = self.new_node(NodeData::Doctype {
            name: name.to_string(),
            public_id: public_id.to_string(),
            system_id: system_id.to_string(),
        });
        let doc = self.document();
        self.append_child(doc, doctype);
    }

    fn add_attrs_if_missing(&self, target: &NodeId, attrs: Vec<HtmlAttribute>) {
        self.with_node_mut(*target, |node| {
            if let NodeData::Element { attrs: existing, .. } = &mut node.data {
                for attr in attrs {
                    let dominated = existing.iter().any(|a| a.name == attr.name);
                    if !dominated {
                        existing.push(Attribute {
                            name: attr.name,
                            value: attr.value.to_string(),
                        });
                    }
                }
            }
        });
    }

    fn remove_from_parent(&self, target: &NodeId) {
        self.detach(*target);
    }

    fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
        let children = self.children(*node);
        for child_id in children {
            self.append_child(*new_parent, child_id);
        }
    }

    fn append_before_sibling(&self, sibling: &NodeId, child: NodeOrText<NodeId>) {
        match child {
            NodeOrText::AppendNode(node_id) => {
                self.insert_before(*sibling, node_id);
            }
            NodeOrText::AppendText(text) => {
                let prev_text_id = {
                    let node = self.get_node(*sibling);
                    node.and_then(|n| n.prev_sibling).and_then(|prev_id| {
                        let prev = self.get_node(prev_id);
                        prev.and_then(|p| if p.is_text() { Some(prev_id) } else { None })
                    })
                };

                if let Some(prev_text_id) = prev_text_id {
                    self.with_node_mut(prev_text_id, |node| {
                        if let NodeData::Text { contents } = &mut node.data {
                            contents.push_str(&text);
                        }
                    });
                    return;
                }

                let text_id = self.new_node(NodeData::Text {
                    contents: text.to_string(),
                });
                self.insert_before(*sibling, text_id);
            }
        }
    }

    fn get_template_contents(&self, target: &NodeId) -> NodeId {
        self.with_node(*target, |n| match &n.data {
            NodeData::Element { template_contents, .. } => *template_contents,
            _ => None,
        })
        .flatten()
        .expect("get_template_contents called on non-template element")
    }

    fn same_node(&self, x: &NodeId, y: &NodeId) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, mode: QuirksMode) {
        // Only full quirks mode makes CSS class/id selectors case-insensitive;
        // limited-quirks behaves like no-quirks for selector matching.
        self.set_quirks(mode == QuirksMode::Quirks);
    }

    fn allow_declarative_shadow_roots(&self, _intended_parent: &NodeId) -> bool {
        // html5ever defaults this hook to `true`, but its default
        // `attach_declarative_shadow` implementation returns an error. In
        // that combination the tree builder consumes
        // `<template shadowrootmode=...>` without inserting a template and
        // then appends its contents directly to the host. That leaks shadow
        // styles and markup into the light DOM.
        //
        // Keep declarative shadow roots as ordinary inert templates until
        // DomTree has a real shadow-root node, tree-scoped selectors, and a
        // scoped stylesheet cascade. Returning false makes html5ever take its
        // fully implemented ordinary-template path.
        false
    }

    fn is_mathml_annotation_xml_integration_point(&self, target: &NodeId) -> bool {
        self.with_node(*target, |n| match &n.data {
            NodeData::Element { mathml_annotation_xml_integration_point, .. } => {
                *mathml_annotation_xml_integration_point
            }
            _ => false,
        })
        .unwrap_or(false)
    }
}

pub fn parse_html(html: &str) -> DomTree {
    use html5ever::tendril::TendrilSink;
    use html5ever::{parse_document, ParseOpts};

    let tree = DomTree::new();
    parse_document(tree, ParseOpts::default())
        .from_utf8()
        .one(html.as_bytes())
}

pub fn parse_fragment(html: &str) -> DomTree {
    let context_name = QualName::new(None, ns!(html), local_name!("body"));
    parse_fragment_with_context(html, context_name)
}

/// Parse an HTML fragment using the supplied context element.
///
/// The tree builder's insertion mode depends on this context. Treating every
/// `innerHTML` assignment as body content drops table-only elements such as a
/// top-level `<tr>` and mis-parses select/template fragments. Browsers instead
/// use the receiver element as the fragment parsing context.
pub fn parse_fragment_with_context(html: &str, context_name: QualName) -> DomTree {
    use html5ever::tendril::TendrilSink;
    use html5ever::{parse_fragment, ParseOpts};
    let tree = DomTree::new();
    // Obscura's fragment parser backs innerHTML in a scripting-enabled
    // document. html5ever 0.39 makes that context flag explicit; keeping it
    // true preserves browser parsing for context-sensitive content such as
    // <noscript>.
    parse_fragment(tree, ParseOpts::default(), context_name, vec![], true)
        .from_utf8()
        .one(html.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_html() {
        let tree = parse_html("<html><head></head><body><h1>Hello</h1></body></html>");
        assert!(tree.len() > 3);
        let text = tree.text_content(tree.document());
        assert!(text.contains("Hello"));
    }

    #[test]
    fn test_parse_with_attributes() {
        let tree = parse_html(r#"<div id="main" class="container">Text</div>"#);
        let main = tree.get_element_by_id("main");
        assert!(main.is_some());
        let node = tree.get_node(main.unwrap()).unwrap();
        assert_eq!(node.get_attribute("class"), Some("container"));
    }

    #[test]
    fn test_parse_nested_structure() {
        let tree = parse_html(
            r#"<html><body>
                <div id="outer">
                    <p id="para">Hello <strong>World</strong></p>
                    <ul>
                        <li>Item 1</li>
                        <li>Item 2</li>
                    </ul>
                </div>
            </body></html>"#,
        );

        let outer = tree.get_element_by_id("outer").unwrap();
        let text = tree.text_content(outer);
        assert!(text.contains("Hello"));
        assert!(text.contains("World"));
        assert!(text.contains("Item 1"));
        assert!(text.contains("Item 2"));
    }

    #[test]
    fn test_parse_malformed_html() {
        let tree = parse_html("<div><p>Unclosed paragraph<p>Another<div>Nested wrong</div>");
        assert!(tree.len() > 3);
        let text = tree.text_content(tree.document());
        assert!(text.contains("Unclosed paragraph"));
        assert!(text.contains("Another"));
    }

    #[test]
    fn test_parse_doctype() {
        let tree = parse_html("<!DOCTYPE html><html><body>Hello</body></html>");
        let first_child = tree.children(tree.document())[0];
        let node = tree.get_node(first_child).unwrap();
        assert!(matches!(node.data, NodeData::Doctype { .. }));
    }

    #[test]
    fn test_parse_fragment() {
        let tree = parse_fragment("<p>Hello</p><p>World</p>");
        let text = tree.text_content(tree.document());
        assert!(text.contains("Hello"));
        assert!(text.contains("World"));
    }

    #[test]
    fn test_parse_fragment_uses_table_context() {
        let context_name = QualName::new(None, ns!(html), local_name!("template"));
        let tree = parse_fragment_with_context("<tr><td>cell</td></tr>", context_name);
        let row = tree
            .query_selector("tr")
            .expect("valid selector")
            .expect("template context preserves the row");
        assert_eq!(tree.text_content(row), "cell");
    }

    #[test]
    fn declarative_shadow_markup_remains_an_inert_template() {
        let tree = parse_html(
            r#"<x-card id="host">
                 <template id="shadow" shadowrootmode="open">
                   <style id="shadow-style">.button { width:100% }</style>
                   <span id="shadow-content">shadow</span>
                 </template>
                 <span id="light-content">light</span>
               </x-card>"#,
        );

        let host = tree.get_element_by_id("host").unwrap();
        let template = tree.get_element_by_id("shadow").unwrap();
        let contents = tree
            .template_contents(template)
            .expect("parsed template has a contents document");
        let style = tree.get_element_by_id("shadow-style").unwrap();
        let shadow_content = tree.get_element_by_id("shadow-content").unwrap();
        let light_content = tree.get_element_by_id("light-content").unwrap();
        let element_children = |parent| {
            tree.children(parent)
                .into_iter()
                .filter(|child| tree.get_node(*child).is_some_and(|node| node.is_element()))
                .collect::<Vec<_>>()
        };

        assert_eq!(
            element_children(host),
            vec![template, light_content],
            "shadow markup must not be spliced into the host's light children"
        );
        assert!(
            tree.children(template).is_empty(),
            "template nodes keep their markup in the separate contents document"
        );
        assert_eq!(element_children(contents), vec![style, shadow_content]);
        assert_eq!(tree.get_node(style).unwrap().parent, Some(contents));
        assert_eq!(
            tree.get_node(shadow_content).unwrap().parent,
            Some(contents)
        );
        assert!(
            !tree.descendants(tree.document()).contains(&style),
            "inert shadow styles must not enter the document tree"
        );
    }

    #[test]
    fn ordinary_template_parsing_is_unchanged() {
        let tree = parse_html(
            r#"<div id="host">
                 <template id="ordinary"><span id="inside">content</span></template>
                 <span id="outside">light</span>
               </div>"#,
        );

        let host = tree.get_element_by_id("host").unwrap();
        let template = tree.get_element_by_id("ordinary").unwrap();
        let contents = tree.template_contents(template).unwrap();
        let inside = tree.get_element_by_id("inside").unwrap();
        let outside = tree.get_element_by_id("outside").unwrap();
        let element_children = |parent| {
            tree.children(parent)
                .into_iter()
                .filter(|child| tree.get_node(*child).is_some_and(|node| node.is_element()))
                .collect::<Vec<_>>()
        };

        assert_eq!(element_children(host), vec![template, outside]);
        assert!(tree.children(template).is_empty());
        assert_eq!(element_children(contents), vec![inside]);
        assert_eq!(tree.get_node(inside).unwrap().parent, Some(contents));
    }
}

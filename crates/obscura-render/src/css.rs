//! Stylesheet cascade: parse `<style>` text into rules once, index each rule by
//! its subject key (id / class / tag), and resolve the matching declarations for
//! an element by testing only the handful of candidate rules that share a key.
//!
//! This replaces the naive "run every selector against the whole tree" approach,
//! which is O(rules x nodes) and dominated render time on large pages (thousands
//! of rules). The indexed cascade is closer to how real browsers match: bucket
//! rules, gather candidates per element, then match and sort by specificity.

use obscura_dom::selector::{CompiledSelector, Matcher, SelectorKey};
use obscura_dom::tree::{DomTree, NodeId};
use std::collections::HashMap;

use crate::LayoutStyle;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
struct ContainerConditionId(u32);

impl ContainerConditionId {
    const NONE: Self = Self(0);
}

#[derive(Clone, Debug, PartialEq)]
struct ContainerConditionNode {
    parent: ContainerConditionId,
    /// Comma-separated queries in one prelude are alternatives; parent-linked
    /// nodes represent nested `@container` rules that must all match.
    alternatives: Vec<ContainerQuery>,
}

#[derive(Clone, Debug, PartialEq)]
struct ContainerQuery {
    name: Option<String>,
    condition: Option<ContainerQueryExpr>,
}

#[derive(Clone, Debug, PartialEq)]
enum ContainerQueryExpr {
    Feature(ContainerSizeFeature),
    /// Syntactically valid future/general-enclosed syntax has Kleene
    /// `unknown` truth. Retaining it prevents one unknown comma arm from
    /// discarding supported alternatives in the same `@container` rule.
    Unknown,
    Not(Box<ContainerQueryExpr>),
    And(Vec<ContainerQueryExpr>),
    Or(Vec<ContainerQueryExpr>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContainerQueryAxis {
    Width,
    Height,
    InlineSize,
    BlockSize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContainerQueryComparison {
    Min,
    Max,
    GreaterThan,
    LessThan,
    Equal,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ContainerSizeFeature {
    axis: ContainerQueryAxis,
    comparison: ContainerQueryComparison,
    length: ContainerQueryLength,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ContainerQueryLength { Px(f32), Em(f32), Rem(f32) }

struct ParsedRule {
    selector: String,
    declarations: String,
    container_condition_id: ContainerConditionId,
}

struct Rule {
    sel: CompiledSelector,
    normal_decls: String,
    important_decls: String,
    /// Source order, for breaking specificity ties (later wins).
    order: usize,
    container_condition_id: ContainerConditionId,
}

struct PseudoRule {
    sel: CompiledSelector,
    normal_decls: String,
    important_decls: String,
    order: usize,
    container_condition_id: ContainerConditionId,
}

const PROPERTY_REGISTRATION_SELECTOR_PREFIX: &str = "\0property:";

#[derive(Clone, Debug, PartialEq, Eq)]
struct RegisteredCustomProperty {
    syntax: String,
    inherits: bool,
    initial_value: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
struct KeyframeStop {
    /// CSS keyframes always provide an offset. Keeping this optional makes the
    /// normalization rule explicit and reusable by future script-created
    /// keyframes without changing the sampler.
    offset: Option<f32>,
    declarations: String,
    source_order: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Keyframes {
    stops: Vec<KeyframeStop>,
}

#[derive(Clone, Debug)]
pub(crate) struct ContainerBox {
    pub(crate) container_type: crate::ContainerType,
    /// Axes on which size containment actually applies to the generated box.
    /// This can be `Normal` even when computed `container-type` is non-normal
    /// (for example a non-atomic inline or an internal table box).
    pub(crate) available_type: crate::ContainerType,
    pub(crate) names: Vec<String>,
    pub(crate) content_width: f32,
    pub(crate) content_height: f32,
    pub(crate) font_size: f32,
}

impl PartialEq for ContainerBox {
    fn eq(&self, other: &Self) -> bool {
        if self.container_type != other.container_type
            || self.available_type != other.available_type
            || self.names != other.names
        {
            return false;
        }
        match self.available_type {
            crate::ContainerType::Normal => true,
            crate::ContainerType::InlineSize => {
                self.content_width == other.content_width
                    && self.font_size == other.font_size
            }
            crate::ContainerType::Size => {
                self.content_width == other.content_width
                    && self.content_height == other.content_height
                    && self.font_size == other.font_size
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ContainerSnapshot {
    pub(crate) boxes: HashMap<NodeId, ContainerBox>,
    pub(crate) root_font_size: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContainerQueryTruth {
    True,
    False,
    Unknown,
}

impl ContainerQueryTruth {
    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::False, _) | (_, Self::False) => Self::False,
            (Self::True, Self::True) => Self::True,
            _ => Self::Unknown,
        }
    }

    fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::True, _) | (_, Self::True) => Self::True,
            (Self::False, Self::False) => Self::False,
            _ => Self::Unknown,
        }
    }

    fn not(self) -> Self {
        match self {
            Self::True => Self::False,
            Self::False => Self::True,
            Self::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum ContainerQuerySubjectKind {
    Element,
    OriginatingPseudo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ContainerQueryCacheKey {
    subject: NodeId,
    condition: ContainerConditionId,
    kind: ContainerQuerySubjectKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ContainerQueryDecision {
    truth: ContainerQueryTruth,
    selected_containers: Vec<Option<NodeId>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ContainerDecisionSignature {
    decisions: HashMap<ContainerQueryCacheKey, ContainerQueryDecision>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ContainerQueryStats {
    pub(crate) evaluations: usize,
    pub(crate) cache_hits: usize,
    pub(crate) ancestor_steps: usize,
}

pub(crate) struct ContainerQueryEvaluator<'a> {
    tree: &'a DomTree,
    snapshot: &'a ContainerSnapshot,
    cache: HashMap<ContainerQueryCacheKey, ContainerQueryDecision>,
    stats: ContainerQueryStats,
}

impl<'a> ContainerQueryEvaluator<'a> {
    pub(crate) fn new(tree: &'a DomTree, snapshot: &'a ContainerSnapshot) -> Self {
        Self {
            tree,
            snapshot,
            cache: HashMap::new(),
            stats: ContainerQueryStats::default(),
        }
    }

    pub(crate) fn finish(self) -> (ContainerDecisionSignature, ContainerQueryStats) {
        (
            ContainerDecisionSignature {
                decisions: self.cache,
            },
            self.stats,
        )
    }

    fn condition_matches(
        &mut self,
        sheet: &Stylesheet,
        subject: NodeId,
        condition: ContainerConditionId,
        kind: ContainerQuerySubjectKind,
    ) -> bool {
        self.evaluate_condition_chain(sheet, subject, condition, kind).truth
            == ContainerQueryTruth::True
    }

    fn evaluate_condition_chain(
        &mut self,
        sheet: &Stylesheet,
        subject: NodeId,
        condition: ContainerConditionId,
        kind: ContainerQuerySubjectKind,
    ) -> ContainerQueryDecision {
        if condition == ContainerConditionId::NONE {
            return ContainerQueryDecision {
                truth: ContainerQueryTruth::True,
                selected_containers: Vec::new(),
            };
        }
        let key = ContainerQueryCacheKey {
            subject,
            condition,
            kind,
        };
        if let Some(decision) = self.cache.get(&key) {
            self.stats.cache_hits += 1;
            return decision.clone();
        }
        self.stats.evaluations += 1;
        let node = &sheet.container_conditions[condition.0 as usize];
        let mut own_truth = ContainerQueryTruth::False;
        let mut selected_containers = Vec::with_capacity(node.alternatives.len());
        for query in &node.alternatives {
            let (truth, selected) = self.evaluate_query(subject, kind, query);
            own_truth = own_truth.or(truth);
            selected_containers.push(selected);
            if own_truth == ContainerQueryTruth::True {
                break;
            }
        }
        if own_truth == ContainerQueryTruth::False {
            let decision = ContainerQueryDecision {
                truth: ContainerQueryTruth::False,
                selected_containers,
            };
            self.cache.insert(key, decision.clone());
            return decision;
        }
        let parent =
            self.evaluate_condition_chain(sheet, subject, node.parent, kind);
        selected_containers.extend(parent.selected_containers);
        let decision = ContainerQueryDecision {
            truth: own_truth.and(parent.truth),
            selected_containers,
        };
        self.cache.insert(key, decision.clone());
        decision
    }

    fn evaluate_query(
        &mut self,
        subject: NodeId,
        kind: ContainerQuerySubjectKind,
        query: &ContainerQuery,
    ) -> (ContainerQueryTruth, Option<NodeId>) {
        let required_axes = query
            .condition
            .as_ref()
            .map(container_query_required_axes)
            .unwrap_or_default();
        let mut candidate = match kind {
            ContainerQuerySubjectKind::Element => {
                self.tree.get_node(subject).and_then(|node| node.parent)
            }
            ContainerQuerySubjectKind::OriginatingPseudo => Some(subject),
        };
        while let Some(id) = candidate {
            self.stats.ancestor_steps += 1;
            let parent = self.tree.get_node(id).and_then(|node| node.parent);
            if let Some(container) = self.snapshot.boxes.get(&id) {
                let supports_axis = match container.container_type {
                    crate::ContainerType::Normal => {
                        !required_axes.inline && !required_axes.block
                    }
                    crate::ContainerType::InlineSize => !required_axes.block,
                    crate::ContainerType::Size => true,
                };
                let name_matches = query
                    .name
                    .as_ref()
                    .map_or(true, |name| container.names.iter().any(|item| item == name));
                if supports_axis && name_matches {
                    let axis_available = match container.available_type {
                        crate::ContainerType::Normal => {
                            !required_axes.inline && !required_axes.block
                        }
                        crate::ContainerType::InlineSize => !required_axes.block,
                        crate::ContainerType::Size => true,
                    };
                    if !axis_available {
                        return (ContainerQueryTruth::Unknown, Some(id));
                    }
                    let truth = query.condition.as_ref().map_or(
                        ContainerQueryTruth::True,
                        |condition| {
                            evaluate_container_query_expr(
                                condition,
                                container,
                                self.snapshot.root_font_size,
                            )
                        },
                    );
                    return (truth, Some(id));
                }
            }
            candidate = parent;
        }
        (ContainerQueryTruth::False, None)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ContainerQueryRequiredAxes {
    inline: bool,
    block: bool,
}

fn container_query_required_axes(expr: &ContainerQueryExpr) -> ContainerQueryRequiredAxes {
    match expr {
        ContainerQueryExpr::Feature(feature) => match feature.axis {
            ContainerQueryAxis::Width | ContainerQueryAxis::InlineSize => {
                ContainerQueryRequiredAxes {
                    inline: true,
                    block: false,
                }
            }
            ContainerQueryAxis::Height | ContainerQueryAxis::BlockSize => {
                ContainerQueryRequiredAxes {
                    inline: false,
                    block: true,
                }
            }
        },
        ContainerQueryExpr::Unknown => ContainerQueryRequiredAxes::default(),
        ContainerQueryExpr::Not(inner) => container_query_required_axes(inner),
        ContainerQueryExpr::And(items) | ContainerQueryExpr::Or(items) => {
            items.iter().fold(
                ContainerQueryRequiredAxes::default(),
                |mut axes, item| {
                    let item = container_query_required_axes(item);
                    axes.inline |= item.inline;
                    axes.block |= item.block;
                    axes
                },
            )
        }
    }
}

fn evaluate_container_query_expr(
    expr: &ContainerQueryExpr,
    container: &ContainerBox,
    root_font_size: f32,
) -> ContainerQueryTruth {
    match expr {
        ContainerQueryExpr::Feature(feature) => {
            let actual = match feature.axis {
                ContainerQueryAxis::Width | ContainerQueryAxis::InlineSize => {
                    container.content_width
                }
                ContainerQueryAxis::Height | ContainerQueryAxis::BlockSize => {
                    container.content_height
                }
            };
            let threshold = match feature.length {
                ContainerQueryLength::Px(value) => value,
                ContainerQueryLength::Em(value) => value * container.font_size,
                ContainerQueryLength::Rem(value) => value * root_font_size,
            };
            if !actual.is_finite() || !threshold.is_finite() {
                return ContainerQueryTruth::Unknown;
            }
            let matches = match feature.comparison {
                ContainerQueryComparison::Min => actual >= threshold,
                ContainerQueryComparison::Max => actual <= threshold,
                ContainerQueryComparison::GreaterThan => actual > threshold,
                ContainerQueryComparison::LessThan => actual < threshold,
                ContainerQueryComparison::Equal => actual == threshold,
            };
            if matches {
                ContainerQueryTruth::True
            } else {
                ContainerQueryTruth::False
            }
        }
        ContainerQueryExpr::Unknown => ContainerQueryTruth::Unknown,
        ContainerQueryExpr::Not(inner) => {
            evaluate_container_query_expr(inner, container, root_font_size).not()
        }
        ContainerQueryExpr::And(items) => {
            let mut truth = ContainerQueryTruth::True;
            for item in items {
                truth = truth.and(evaluate_container_query_expr(
                    item,
                    container,
                    root_font_size,
                ));
                if truth == ContainerQueryTruth::False {
                    break;
                }
            }
            truth
        }
        ContainerQueryExpr::Or(items) => {
            let mut truth = ContainerQueryTruth::False;
            for item in items {
                truth = truth.or(evaluate_container_query_expr(
                    item,
                    container,
                    root_font_size,
                ));
                if truth == ContainerQueryTruth::True {
                    break;
                }
            }
            truth
        }
    }
}

/// An indexed set of author rules ready for fast per-element matching.
pub struct Stylesheet {
    rules: Vec<Rule>,
    registered_custom_properties: HashMap<String, RegisteredCustomProperty>,
    /// Index zero is the unconditional sentinel.
    container_conditions: Vec<ContainerConditionNode>,
    /// Every offset from each `@keyframes` rule. The opacity sampler resolves
    /// property-specific segments at the stylesheet's explicit sample time.
    keyframes: HashMap<String, Keyframes>,
    animation_sample_time: crate::AnimationSampleTime,
    by_id: HashMap<String, Vec<usize>>,
    by_class: HashMap<String, Vec<usize>>,
    by_local: HashMap<String, Vec<usize>>,
    universal: Vec<usize>,
    /// `sel::before` / `sel::after` rules matched against their ordinary base
    /// selector. Keeping their full declaration cascade supports both literal
    /// generated text and positioned decorative boxes.
    before_rules: Vec<PseudoRule>,
    after_rules: Vec<PseudoRule>,
}

impl Stylesheet {
    /// Until Stage B supplies completed container geometry, preserved
    /// conditional rules remain inactive rather than using viewport geometry.
    fn container_condition_is_active(
        &self,
        id: ContainerConditionId,
        subject: NodeId,
        kind: ContainerQuerySubjectKind,
        evaluator: &mut Option<&mut ContainerQueryEvaluator<'_>>,
    ) -> bool {
        debug_assert!((id.0 as usize) < self.container_conditions.len());
        if id == ContainerConditionId::NONE {
            return true;
        }
        evaluator.as_deref_mut().is_some_and(|evaluator| {
            evaluator.condition_matches(self, subject, id, kind)
        })
    }

    pub(crate) fn has_container_queries(&self) -> bool {
        self.rules
            .iter()
            .any(|rule| rule.container_condition_id != ContainerConditionId::NONE)
            || self
                .before_rules
                .iter()
                .chain(&self.after_rules)
                .any(|rule| rule.container_condition_id != ContainerConditionId::NONE)
    }

    pub(crate) fn container_condition_depth(&self) -> usize {
        self.container_conditions
            .iter()
            .skip(1)
            .map(|node| {
                let mut depth = 1usize;
                let mut parent = node.parent;
                while parent != ContainerConditionId::NONE {
                    depth += 1;
                    parent = self.container_conditions[parent.0 as usize].parent;
                }
                depth
            })
            .max()
            .unwrap_or(0)
    }

    /// Parse and index a set of raw CSS sources (the text of each `<style>`
    /// block, in document order). Selectors that fail to parse are dropped.
    pub fn parse(tree: &DomTree, sources: &[String]) -> Self {
        Self::parse_for_viewport(tree, sources, (1280.0, 720.0))
    }

    /// Parse author CSS for the live CSS viewport. Media queries must use the
    /// same dimensions as layout and page JavaScript; filtering them against a
    /// fixed desktop width made responsive frameworks build one DOM while the
    /// renderer applied another breakpoint.
    pub fn parse_for_viewport(
        tree: &DomTree,
        sources: &[String],
        viewport: (f32, f32),
    ) -> Self {
        Self::parse_for_viewport_at_animation_time(
            tree,
            sources,
            viewport,
            crate::AnimationSampleTime::default(),
        )
    }

    pub fn parse_for_viewport_at_animation_time(
        tree: &DomTree,
        sources: &[String],
        viewport: (f32, f32),
        animation_sample_time: crate::AnimationSampleTime,
    ) -> Self {
        let mut sheet = Stylesheet {
            rules: Vec::new(),
            registered_custom_properties: HashMap::new(),
            container_conditions: vec![ContainerConditionNode {
                parent: ContainerConditionId::NONE,
                alternatives: Vec::new(),
            }],
            keyframes: HashMap::new(),
            animation_sample_time,
            by_id: HashMap::new(),
            by_class: HashMap::new(),
            by_local: HashMap::new(),
            universal: Vec::new(),
            before_rules: Vec::new(),
            after_rules: Vec::new(),
        };
        let mut order = 0usize;
        for src in sources {
            for (name, keyframes) in extract_keyframes(src) {
                sheet.keyframes.insert(name, keyframes);
            }
            let parsed = parse_stylesheet_for_viewport_preserving_containers(
                src,
                viewport,
                &mut sheet.container_conditions,
                ContainerConditionId::NONE,
            );
            for ParsedRule { selector, declarations: decls, container_condition_id } in parsed {
                if let Some(name) = selector.strip_prefix(PROPERTY_REGISTRATION_SELECTOR_PREFIX) {
                    if let Some(registration) = parse_property_registration(&decls) {
                        sheet
                            .registered_custom_properties
                            .insert(name.to_string(), registration);
                    }
                    continue;
                }
                let sel_trim = selector.trim();
                if let Some(base) = strip_pseudo_element(sel_trim, "before") {
                    if let Some(sel) = tree.compile_rule_selector(base) {
                        let (normal_decls, important_decls) =
                            crate::style::partition_declarations(&decls);
                        sheet.before_rules.push(PseudoRule {
                            sel,
                            normal_decls,
                            important_decls,
                            order,
                            container_condition_id,
                        });
                    }
                    order += 1;
                    continue;
                }
                if let Some(base) = strip_pseudo_element(sel_trim, "after") {
                    if let Some(sel) = tree.compile_rule_selector(base) {
                        let (normal_decls, important_decls) =
                            crate::style::partition_declarations(&decls);
                        sheet.after_rules.push(PseudoRule {
                            sel,
                            normal_decls,
                            important_decls,
                            order,
                            container_condition_id,
                        });
                    }
                    order += 1;
                    continue;
                }
                let Some(sel) = tree.compile_rule_selector(&selector) else { continue };
                let (normal_decls, important_decls) = crate::style::partition_declarations(&decls);
                let idx = sheet.rules.len();
                match sel.key() {
                    SelectorKey::Id(v) => sheet.by_id.entry(v.clone()).or_default().push(idx),
                    SelectorKey::Class(v) => sheet.by_class.entry(v.clone()).or_default().push(idx),
                    SelectorKey::Local(v) => sheet.by_local.entry(v.clone()).or_default().push(idx),
                    SelectorKey::Universal => sheet.universal.push(idx),
                }
                sheet.rules.push(Rule {
                    sel, normal_decls, important_decls, order, container_condition_id,
                });
                order += 1;
            }
        }
        sheet
    }

    pub fn pseudo_styles(
        &self,
        tree: &DomTree,
        matcher: &mut Matcher,
        nid: NodeId,
        props: &HashMap<String, String>,
        host_style: &LayoutStyle,
    ) -> (Option<LayoutStyle>, Option<LayoutStyle>) {
        self.pseudo_styles_internal(
            tree,
            matcher,
            nid,
            props,
            host_style,
            None,
        )
    }

    pub(crate) fn pseudo_styles_with_container_queries(
        &self,
        tree: &DomTree,
        matcher: &mut Matcher,
        nid: NodeId,
        props: &HashMap<String, String>,
        host_style: &LayoutStyle,
        evaluator: &mut ContainerQueryEvaluator<'_>,
    ) -> (Option<LayoutStyle>, Option<LayoutStyle>) {
        self.pseudo_styles_internal(
            tree,
            matcher,
            nid,
            props,
            host_style,
            Some(evaluator),
        )
    }

    fn pseudo_styles_internal(
        &self,
        tree: &DomTree,
        matcher: &mut Matcher,
        nid: NodeId,
        props: &HashMap<String, String>,
        host_style: &LayoutStyle,
        mut evaluator: Option<&mut ContainerQueryEvaluator<'_>>,
    ) -> (Option<LayoutStyle>, Option<LayoutStyle>) {
        let mut build = |rules: &[PseudoRule], matcher: &mut Matcher| {
            let mut matched: Vec<(u32, usize, &PseudoRule)> = Vec::new();
            for rule in rules {
                // Container lookup is an ancestor walk. Keep it behind the
                // selector match so unrelated pseudo rules remain cheap.
                if matcher.matches(tree, nid, &rule.sel)
                    && self.container_condition_is_active(
                        rule.container_condition_id,
                        nid,
                        ContainerQuerySubjectKind::OriginatingPseudo,
                        &mut evaluator,
                    )
                {
                    matched.push((rule.sel.specificity(), rule.order, rule));
                }
            }
            if matched.is_empty() {
                return None;
            }
            matched.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
            // Generated ::before/::after boxes have an inline outer display
            // by default. LayoutStyle's general default is block because it
            // primarily represents ordinary DOM boxes, so set the pseudo
            // initial value explicitly before applying author declarations.
            let mut style = LayoutStyle {
                display: crate::Display::Inline,
                ..Default::default()
            };
            style.color_scheme_dark = host_style.color_scheme_dark;
            let inherited_color_scheme_dark = host_style.color_scheme_dark;
            let mut content = None;
            for &(_, _, rule) in &matched {
                let expanded = substitute_declarations(&rule.normal_decls, props);
                crate::style::apply_color_scheme_declarations_from(
                    &mut style,
                    &expanded,
                    inherited_color_scheme_dark,
                );
            }
            for &(_, _, rule) in &matched {
                let expanded = substitute_declarations(&rule.important_decls, props);
                crate::style::apply_color_scheme_declarations_from(
                    &mut style,
                    &expanded,
                    inherited_color_scheme_dark,
                );
            }
            for &(_, _, rule) in &matched {
                let expanded = substitute_declarations(&rule.normal_decls, props);
                crate::style::apply_declarations_with_locked_color_scheme(
                    &mut style,
                    &expanded,
                );
                if let Some(value) = extract_content(&expanded, tree, nid) {
                    content = value;
                }
            }
            for &(_, _, rule) in &matched {
                let expanded = substitute_declarations(&rule.important_decls, props);
                crate::style::apply_declarations_with_locked_color_scheme(
                    &mut style,
                    &expanded,
                );
                if let Some(value) = extract_content(&expanded, tree, nid) {
                    content = value;
                }
            }
            style.before_content = content;
            if style.before_content.is_some() || style.content_image.is_some() {
                Some(style)
            } else {
                None
            }
        };
        (
            build(&self.before_rules, matcher),
            build(&self.after_rules, matcher),
        )
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    #[doc(hidden)]
    pub fn debug_stats(&self) -> (usize, usize, usize, usize, usize) {
        (self.rules.len(), self.by_id.len(), self.by_class.len(), self.by_local.len(), self.universal.len())
    }

    /// Apply every author rule that matches `nid` to `style`, in cascade order
    /// (ascending specificity, then source order, so the winner is applied last).
    /// `id`, `classes`, and `local` are the element's precomputed keys.
    /// `parent_props` is the element's inherited custom-property map. Returns
    /// `Some(map)` (parent + this element's own `--x` declarations) when this
    /// element declares any custom properties, so the caller can thread the
    /// richer map to descendants; `None` means "reuse the parent's map".
    pub fn apply(
        &self,
        tree: &DomTree,
        matcher: &mut Matcher,
        nid: NodeId,
        id: Option<&str>,
        classes: &[String],
        local: &str,
        style: &mut LayoutStyle,
        parent_props: &HashMap<String, String>,
        inline_css: Option<&str>,
    ) -> Option<HashMap<String, String>> {
        self.apply_internal(
            tree,
            matcher,
            nid,
            id,
            classes,
            local,
            style,
            parent_props,
            inline_css,
            None,
        )
    }

    pub(crate) fn apply_with_container_queries(
        &self,
        tree: &DomTree,
        matcher: &mut Matcher,
        nid: NodeId,
        id: Option<&str>,
        classes: &[String],
        local: &str,
        style: &mut LayoutStyle,
        parent_props: &HashMap<String, String>,
        inline_css: Option<&str>,
        evaluator: &mut ContainerQueryEvaluator<'_>,
    ) -> Option<HashMap<String, String>> {
        self.apply_internal(
            tree,
            matcher,
            nid,
            id,
            classes,
            local,
            style,
            parent_props,
            inline_css,
            Some(evaluator),
        )
    }

    fn apply_internal(
        &self,
        tree: &DomTree,
        matcher: &mut Matcher,
        nid: NodeId,
        id: Option<&str>,
        classes: &[String],
        local: &str,
        style: &mut LayoutStyle,
        parent_props: &HashMap<String, String>,
        inline_css: Option<&str>,
        mut evaluator: Option<&mut ContainerQueryEvaluator<'_>>,
    ) -> Option<HashMap<String, String>> {
        // (specificity, order, rule index) for each matching rule.
        let mut matched: Vec<(u32, usize, usize)> = Vec::new();
        let mut consider = |bucket: Option<&Vec<usize>>, matched: &mut Vec<(u32, usize, usize)>| {
            if let Some(idxs) = bucket {
                for &i in idxs {
                    let rule = &self.rules[i];
                    // Container lookup is an ancestor walk. Keep it behind
                    // selector matching and cache the result per condition.
                    if matcher.matches(tree, nid, &rule.sel)
                        && self.container_condition_is_active(
                            rule.container_condition_id,
                            nid,
                            ContainerQuerySubjectKind::Element,
                            &mut evaluator,
                        )
                    {
                        matched.push((rule.sel.specificity(), rule.order, i));
                    }
                }
            }
        };

        consider(self.by_local.get(local), &mut matched);
        if let Some(id) = id {
            consider(self.by_id.get(id), &mut matched);
        }
        for c in classes {
            consider(self.by_class.get(c), &mut matched);
        }
        if !self.universal.is_empty() {
            consider(Some(&self.universal), &mut matched);
        }

        matched.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

        let (inline_normal, inline_important) = inline_css
            .map(crate::style::partition_declarations)
            .unwrap_or_default();

        // Pass 1: collect this element's own custom properties (`--x: value`),
        // in cascade order (last wins), layered over the inherited map. Custom
        // properties cascade fully before any `var()` is substituted.
        let mut own: Vec<(String, String)> = Vec::new();
        let mut collect_custom = |css: &str| {
            for decl in crate::style::split_declarations(css) {
                if let Some((name, val)) = decl.split_once(':') {
                    let name = name.trim();
                    if name.starts_with("--") && name.len() > 2 {
                        own.push((name.to_string(), val.trim().to_string()));
                    }
                }
            }
        };
        for &(_, _, i) in &matched {
            collect_custom(&self.rules[i].normal_decls);
        }
        collect_custom(&inline_normal);
        for &(_, _, i) in &matched {
            collect_custom(&self.rules[i].important_decls);
        }
        collect_custom(&inline_important);
        let mut resolved_props = parent_props.clone();
        for (name, registration) in &self.registered_custom_properties {
            if registration.inherits && resolved_props.contains_key(name) {
                continue;
            }
            if let Some(initial) = &registration.initial_value {
                resolved_props.insert(name.clone(), initial.clone());
            } else {
                resolved_props.remove(name);
            }
        }
        let registration_changed_props = resolved_props != *parent_props;
        let has_own = !own.is_empty();
        for (k, v) in own {
            let registration = self.registered_custom_properties.get(&k);
            let set_initial = |props: &mut HashMap<String, String>| {
                if let Some(initial) = registration.and_then(|entry| entry.initial_value.as_ref()) {
                    props.insert(k.clone(), initial.clone());
                } else {
                    props.remove(&k);
                }
            };
            let inherit = |props: &mut HashMap<String, String>| {
                if let Some(inherited) = parent_props.get(&k) {
                    props.insert(k.clone(), inherited.clone());
                } else if let Some(initial) =
                    registration.and_then(|entry| entry.initial_value.as_ref())
                {
                    props.insert(k.clone(), initial.clone());
                } else {
                    props.remove(&k);
                }
            };
            match v.trim().to_ascii_lowercase().as_str() {
                "initial" => set_initial(&mut resolved_props),
                "inherit" => inherit(&mut resolved_props),
                "unset" | "revert" | "revert-layer"
                    if registration.is_some_and(|entry| !entry.inherits) =>
                {
                    set_initial(&mut resolved_props);
                }
                "unset" | "revert" | "revert-layer" => inherit(&mut resolved_props),
                _ => {
                    let valid = registration.is_none_or(|entry| {
                        substitute_var_value(&v, &resolved_props, 0)
                            .is_some_and(|value| registered_value_matches(entry, &value))
                    });
                    if valid {
                        resolved_props.insert(k, v);
                    } else {
                        set_initial(&mut resolved_props);
                    }
                }
            }
        }
        let effective = if has_own || registration_changed_props {
            Some(resolved_props)
        } else {
            None
        };
        let props = effective.as_ref().unwrap_or(parent_props);

        let inherited_color_scheme_dark = style.color_scheme_dark;
        // `light-dark()` resolves against the element's final used color
        // scheme, not the declaration order. Determine the scheme winner
        // across the complete author cascade before applying any color-valued
        // property. The style starts with its inherited scheme.
        for &(_, _, i) in &matched {
            let expanded =
                substitute_declarations(&self.rules[i].normal_decls, props);
            crate::style::apply_color_scheme_declarations_from(
                style,
                &expanded,
                inherited_color_scheme_dark,
            );
        }
        let expanded = substitute_declarations(&inline_normal, props);
        crate::style::apply_color_scheme_declarations_from(
            style,
            &expanded,
            inherited_color_scheme_dark,
        );
        for &(_, _, i) in &matched {
            let expanded =
                substitute_declarations(&self.rules[i].important_decls, props);
            crate::style::apply_color_scheme_declarations_from(
                style,
                &expanded,
                inherited_color_scheme_dark,
            );
        }
        let expanded = substitute_declarations(&inline_important, props);
        crate::style::apply_color_scheme_declarations_from(
            style,
            &expanded,
            inherited_color_scheme_dark,
        );

        // Pass 2: apply normal declarations with `var()` substituted against
        // the resolved custom-property map.
        for &(_, _, i) in &matched {
            let expanded = substitute_declarations(&self.rules[i].normal_decls, props);
            crate::style::apply_declarations_with_locked_color_scheme(
                style,
                &expanded,
            );
        }
        let expanded = substitute_declarations(&inline_normal, props);
        crate::style::apply_declarations_with_locked_color_scheme(style, &expanded);

        // Animation control properties from author !important participate in
        // computed timing, while the animated value itself remains below the
        // important author origin. Resolve those controls on a temporary style
        // before sampling, then let the ordinary important pass override the
        // sampled opacity where appropriate.
        let mut animation_style = style.clone();
        for &(_, _, i) in &matched {
            let expanded = substitute_declarations(&self.rules[i].important_decls, props);
            crate::style::apply_animation_declarations(&mut animation_style, &expanded);
        }
        let expanded = substitute_declarations(&inline_important, props);
        crate::style::apply_animation_declarations(&mut animation_style, &expanded);
        if let Some(name) = animation_style.animation_name.as_deref() {
            if let Some(keyframes) = self.keyframes.get(name) {
                if let Some(opacity) = sample_animation_opacity(
                    keyframes,
                    style.opacity.unwrap_or(1.0),
                    &animation_style.animation_timing,
                    self.animation_sample_time,
                    props,
                ) {
                    style.opacity = Some(opacity);
                }
            }
        }

        for &(_, _, i) in &matched {
            let expanded = substitute_declarations(&self.rules[i].important_decls, props);
            crate::style::apply_declarations_with_locked_color_scheme(
                style,
                &expanded,
            );
        }
        let expanded = substitute_declarations(&inline_important, props);
        crate::style::apply_declarations_with_locked_color_scheme(style, &expanded);
        effective
    }
}

/// Collect every offset and declaration block from standard and prefixed
/// keyframes rules. Keyframe selectors are percentages rather than DOM
/// selectors and must never enter the ordinary rule index.
fn extract_keyframes(css: &str) -> Vec<(String, Keyframes)> {
    let lower = css.to_ascii_lowercase();
    let mut found = Vec::new();
    let mut cursor = 0usize;

    while cursor < css.len() {
        let standard = lower[cursor..].find("@keyframes").map(|offset| {
            (cursor + offset, "@keyframes".len())
        });
        let webkit = lower[cursor..].find("@-webkit-keyframes").map(|offset| {
            (cursor + offset, "@-webkit-keyframes".len())
        });
        let Some((start, keyword_len)) = (match (standard, webkit) {
            (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }) else {
            break;
        };

        let after_keyword = start + keyword_len;
        let Some(open_rel) = css[after_keyword..].find('{') else { break };
        let open = after_keyword + open_rel;
        let name = css[after_keyword..open].trim();
        if name.is_empty() {
            cursor = open + 1;
            continue;
        }

        let mut depth = 1i32;
        let mut in_quote: Option<char> = None;
        let mut escaped = false;
        let mut close = None;
        for (offset, ch) in css[open + 1..].char_indices() {
            if let Some(quote) = in_quote {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == quote {
                    in_quote = None;
                }
                continue;
            }
            if ch == '"' || ch == '\'' {
                in_quote = Some(ch);
            } else if ch == '{' {
                depth += 1;
            } else if ch == '}' {
                depth -= 1;
                if depth == 0 {
                    close = Some(open + 1 + offset);
                    break;
                }
            }
        }
        let Some(close) = close else { break };
        let inner = &css[open + 1..close];
        let mut stops = Vec::new();
        for (source_order, (selector, declarations)) in
            parse_stylesheet_for_viewport(inner, (1280.0, 720.0))
                .into_iter()
                .enumerate()
        {
            for part in selector.split(',') {
                if let Some(offset) = parse_keyframe_offset(part) {
                    stops.push(KeyframeStop {
                        offset: Some(offset),
                        declarations: declarations.clone(),
                        source_order,
                    });
                }
            }
        }
        if !stops.is_empty() {
            found.push((name.to_string(), Keyframes { stops }));
        }
        cursor = close + 1;
    }
    found
}

fn parse_keyframe_offset(value: &str) -> Option<f32> {
    let value = value.trim().to_ascii_lowercase();
    match value.as_str() {
        "from" => Some(0.0),
        "to" => Some(1.0),
        _ => value
            .strip_suffix('%')?
            .trim()
            .parse::<f32>()
            .ok()
            .filter(|offset| offset.is_finite() && (0.0..=100.0).contains(offset))
            .map(|offset| offset / 100.0),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AnimationPhase {
    Before,
    Active,
    After,
}

fn sample_animation_opacity(
    keyframes: &Keyframes,
    underlying: f32,
    timing: &crate::AnimationTiming,
    sample_time: crate::AnimationSampleTime,
    props: &HashMap<String, String>,
) -> Option<f32> {
    let progress = animation_directed_progress(timing, sample_time)?;
    let normalized = normalized_keyframe_offsets(&keyframes.stops);
    let mut opacity_stops = Vec::<(f32, usize, f32)>::new();
    for (offset, stop) in normalized {
        let expanded = substitute_declarations(&stop.declarations, props);
        if let Some(opacity) = opacity_from_declarations(&expanded) {
            opacity_stops.push((offset, stop.source_order, opacity));
        }
    }
    if opacity_stops.is_empty() {
        return None;
    }
    opacity_stops.sort_by(|left, right| {
        left.0
            .total_cmp(&right.0)
            .then(left.1.cmp(&right.1))
    });
    let mut merged = Vec::<(f32, f32)>::new();
    for (offset, _, opacity) in opacity_stops {
        if merged.last().is_some_and(|last| last.0 == offset) {
            merged.last_mut().unwrap().1 = opacity;
        } else {
            merged.push((offset, opacity));
        }
    }
    if merged.first().is_none_or(|stop| stop.0 > 0.0) {
        merged.insert(0, (0.0, underlying));
    }
    if merged.last().is_none_or(|stop| stop.0 < 1.0) {
        merged.push((1.0, underlying));
    }
    if progress <= merged[0].0 {
        return Some(merged[0].1.clamp(0.0, 1.0));
    }
    for pair in merged.windows(2) {
        let (from_offset, from_value) = pair[0];
        let (to_offset, to_value) = pair[1];
        if progress <= to_offset {
            if progress == to_offset || to_offset == from_offset {
                return Some(to_value.clamp(0.0, 1.0));
            }
            let position = (progress - from_offset) / (to_offset - from_offset);
            return Some((from_value + (to_value - from_value) * position).clamp(0.0, 1.0));
        }
    }
    Some(merged.last().unwrap().1.clamp(0.0, 1.0))
}

fn opacity_from_declarations(declarations: &str) -> Option<f32> {
    let mut opacity = None;
    for declaration in crate::style::split_declarations(declarations) {
        let Some((name, value)) = declaration.split_once(':') else { continue };
        if name.trim().eq_ignore_ascii_case("opacity") {
            if let Ok(parsed) = value.trim().parse::<f32>() {
                if parsed.is_finite() {
                    opacity = Some(parsed);
                }
            }
        }
    }
    opacity
}

fn animation_directed_progress(
    timing: &crate::AnimationTiming,
    sample_time: crate::AnimationSampleTime,
) -> Option<f32> {
    let duration = timing.duration_ms.max(0.0);
    let iterations = timing.iteration_count.max(0.0);
    let active_duration = if duration == 0.0 || iterations == 0.0 {
        0.0
    } else {
        duration * iterations
    };
    let local_time = if timing.play_state == crate::AnimationPlayState::Paused {
        0.0
    } else {
        sample_time.milliseconds
    };
    if !local_time.is_finite() {
        return None;
    }
    let end_time = (timing.delay_ms + active_duration).max(0.0);
    let before_boundary = timing.delay_ms.clamp(0.0, end_time);
    let after_boundary = (timing.delay_ms + active_duration).min(end_time).max(0.0);
    let phase = if local_time < before_boundary {
        AnimationPhase::Before
    } else if local_time >= after_boundary {
        AnimationPhase::After
    } else {
        AnimationPhase::Active
    };
    let active_time = match phase {
        AnimationPhase::Before => {
            if !matches!(
                timing.fill_mode,
                crate::AnimationFillMode::Backwards | crate::AnimationFillMode::Both
            ) {
                return None;
            }
            (local_time - timing.delay_ms).max(0.0)
        }
        AnimationPhase::Active => local_time - timing.delay_ms,
        AnimationPhase::After => {
            if !matches!(
                timing.fill_mode,
                crate::AnimationFillMode::Forwards | crate::AnimationFillMode::Both
            ) {
                return None;
            }
            (local_time - timing.delay_ms).clamp(0.0, active_duration)
        }
    };

    let overall_progress = if duration == 0.0 {
        if phase == AnimationPhase::Before { 0.0 } else { iterations }
    } else {
        active_time / duration
    };
    if !overall_progress.is_finite() {
        return Some(match timing.direction {
            crate::AnimationDirection::Reverse | crate::AnimationDirection::AlternateReverse => 1.0,
            _ => 0.0,
        });
    }
    let mut current_iteration = overall_progress.floor().max(0.0);
    let mut simple_progress = overall_progress.rem_euclid(1.0);
    if phase == AnimationPhase::After
        && iterations > 0.0
        && simple_progress == 0.0
    {
        simple_progress = 1.0;
        current_iteration = (current_iteration - 1.0).max(0.0);
    }
    let reverse = match timing.direction {
        crate::AnimationDirection::Normal => false,
        crate::AnimationDirection::Reverse => true,
        crate::AnimationDirection::Alternate => current_iteration.rem_euclid(2.0) >= 1.0,
        crate::AnimationDirection::AlternateReverse => current_iteration.rem_euclid(2.0) < 1.0,
    };
    Some(if reverse {
        1.0 - simple_progress
    } else {
        simple_progress
    })
}

fn normalized_keyframe_offsets(stops: &[KeyframeStop]) -> Vec<(f32, &KeyframeStop)> {
    if stops.is_empty() {
        return Vec::new();
    }
    let mut offsets = stops.iter().map(|stop| stop.offset).collect::<Vec<_>>();
    if offsets.len() == 1 {
        offsets[0] = Some(offsets[0].unwrap_or(1.0));
    } else {
        if offsets[0].is_none() {
            offsets[0] = Some(0.0);
        }
        let last = offsets.len() - 1;
        if offsets[last].is_none() {
            offsets[last] = Some(1.0);
        }
    }
    let mut index = 0usize;
    while index < offsets.len() {
        if offsets[index].is_some() {
            index += 1;
            continue;
        }
        let start = index - 1;
        let mut end = index + 1;
        while offsets[end].is_none() {
            end += 1;
        }
        let from = offsets[start].unwrap();
        let to = offsets[end].unwrap();
        let span = (end - start) as f32;
        for missing in index..end {
            offsets[missing] = Some(from + (to - from) * (missing - start) as f32 / span);
        }
        index = end + 1;
    }
    stops
        .iter()
        .zip(offsets)
        .map(|(stop, offset)| (offset.unwrap(), stop))
        .collect()
}

/// Resolve variables one declaration at a time. An invalid variable poisons
/// its entire declaration at computed-value time, but must not erase unrelated
/// declarations in the same rule.
fn substitute_declarations(css: &str, props: &HashMap<String, String>) -> String {
    let mut expanded = String::new();
    for declaration in crate::style::split_declarations(css) {
        let Some((name, value)) = declaration.split_once(':') else {
            continue;
        };
        let Some(value) = substitute_var_value(value.trim(), props, 0) else {
            continue;
        };
        expanded.push_str(name.trim());
        expanded.push(':');
        expanded.push_str(&value);
        expanded.push(';');
    }
    expanded
}

/// Substitute every `var(--name, fallback?)` in one property value. `None`
/// represents CSS's guaranteed-invalid value. Crucially, invalidity propagates
/// through an intermediate custom property so an outer var() can use its own
/// fallback (`--toggle:var(--missing) dark; color:var(--toggle,light)`).
fn substitute_var_value(input: &str, props: &HashMap<String, String>, depth: u8) -> Option<String> {
    if depth > 16 {
        return None;
    }
    if !input.contains("var(") {
        return Some(input.to_string());
    }
    let mut out = String::new();
    let mut rest = input;
    while let Some(pos) = rest.find("var(") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 4..];
        // Matching close paren, respecting nesting.
        let mut d = 1i32;
        let mut end = None;
        for (i, ch) in after.char_indices() {
            match ch {
                '(' => d += 1,
                ')' => {
                    d -= 1;
                    if d == 0 {
                        end = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else {
            return None;
        };
        let inner = &after[..end];
        let (name, fallback) = match inner.split_once(',') {
            Some((n, f)) => (n.trim(), Some(f.trim())),
            None => (inner.trim(), None),
        };
        let resolved = props
            .get(name)
            .and_then(|value| substitute_var_value(value, props, depth + 1));
        let replacement = match resolved {
            Some(value) => value,
            None => {
                let fallback = fallback?;
                substitute_var_value(fallback, props, depth + 1)?
            }
        };
        out.push_str(&replacement);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// If `selector`'s rightmost compound is the pseudo-element `which`
/// (`"before"` or `"after"`, matching either the modern `::` or legacy `:`
/// form), return everything before it, trimmed. Only a trailing
/// pseudo-element is handled: `li::before` strips to `li`, but a selector
/// that uses `::before` as anything other than the final component (not
/// valid CSS in the first place) is left alone.
fn strip_pseudo_element<'a>(selector: &'a str, which: &str) -> Option<&'a str> {
    for prefix in ["::", ":"] {
        let suffix = format!("{prefix}{which}");
        if let Some(base) = selector.strip_suffix(&suffix) {
            if !base.is_empty() {
                return Some(base.trim());
            }
        }
    }
    None
}

/// Return the final valid `content` declaration in a declaration list.
///
/// The outer option says whether a declaration was found; the inner option is
/// the generated text (`none`/`normal` suppress the pseudo). Along with quoted
/// strings, support the common `attr(name)` form used by component-library
/// buttons and badges. The attribute is resolved against the originating
/// element, as CSS generated content requires.
fn extract_content(decls: &str, tree: &DomTree, nid: NodeId) -> Option<Option<String>> {
    let mut result = None;
    for raw in crate::style::split_declarations(decls) {
        let Some((name, value)) = raw.split_once(':') else { continue };
        if !name.trim().eq_ignore_ascii_case("content") {
            continue;
        }
        let value = value.trim();
        if value.eq_ignore_ascii_case("none") || value.eq_ignore_ascii_case("normal") {
            result = Some(None);
            continue;
        }
        let parsed = if let Some(quote) =
            value.chars().next().filter(|ch| matches!(ch, '"' | '\''))
        {
            let rest = &value[quote.len_utf8()..];
            rest.find(quote).map(|end| unescape_css_string(&rest[..end]))
        } else if let Some(rest) = value.strip_prefix("attr(") {
            rest.find(')').map(|end| {
                tree.get_node(nid)
                    .and_then(|node| {
                        node.get_attribute(rest[..end].trim()).map(str::to_owned)
                    })
                    .unwrap_or_default()
            })
        } else {
            None
        };
        if let Some(parsed) = parsed {
            result = Some(Some(parsed));
        } else if value
            .trim_start()
            .get(..4)
            .map_or(false, |prefix| prefix.eq_ignore_ascii_case("url("))
        {
            // An image-valued content declaration supersedes any earlier
            // string declaration. The image itself is retained on
            // LayoutStyle::content_image by apply_declarations; clearing the
            // text here keeps the two views of the winning declaration in
            // sync and, importantly, keeps the pseudo alive.
            result = Some(None);
        }
    }
    result
}

/// Decode CSS string escapes: `\` followed by 1-6 hex digits is a Unicode
/// code point (`\200B` -> U+200B ZERO WIDTH SPACE, ubiquitous in generated
/// `content` for accessible section-edit-link brackets and similar), with a
/// single trailing whitespace character consumed as the escape's own
/// terminator per the CSS spec rather than treated as literal content;
/// anything else after a backslash (`\"`, `\\`) is a literal escaped
/// character. Without this, a hex escape prints as its own literal digits.
fn unescape_css_string(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let mut hex = String::new();
        while hex.len() < 6 {
            match chars.peek() {
                Some(h) if h.is_ascii_hexdigit() => {
                    hex.push(*h);
                    chars.next();
                }
                _ => break,
            }
        }
        if !hex.is_empty() {
            if matches!(chars.peek(), Some(next) if next.is_whitespace()) {
                chars.next();
            }
            if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                out.push(ch);
                continue;
            }
        }
        if let Some(next) = chars.next() {
            out.push(next);
        }
    }
    out
}

/// Split a stylesheet into `(selector, declarations)` rules. Handles nested
/// braces, `/* comments */`, and the at-rules that carry ordinary rules inside
/// (`@media`, `@supports`, `@layer`); other at-rules (`@font-face`, `@keyframes`, ...) are
/// skipped since they do not contribute layout-relevant declarations here.
pub fn parse_stylesheet(css: &str) -> Vec<(String, String)> {
    parse_stylesheet_for_viewport(css, (1280.0, 720.0))
}

fn parse_stylesheet_for_viewport(
    css: &str,
    viewport: (f32, f32),
) -> Vec<(String, String)> {
    let mut conditions = vec![ContainerConditionNode {
        parent: ContainerConditionId::NONE,
        alternatives: Vec::new(),
    }];
    parse_stylesheet_for_viewport_preserving_containers(
        css, viewport, &mut conditions, ContainerConditionId::NONE,
    )
    .into_iter()
    // This legacy tuple API cannot express conditional context. Keep its
    // established behavior by omitting unresolved container rules.
    .filter(|rule| {
        rule.container_condition_id == ContainerConditionId::NONE
            && !rule
                .selector
                .starts_with(PROPERTY_REGISTRATION_SELECTOR_PREFIX)
    })
    .map(|rule| (rule.selector, rule.declarations))
    .collect()
}

fn parse_stylesheet_for_viewport_preserving_containers(
    css: &str,
    viewport: (f32, f32),
    container_conditions: &mut Vec<ContainerConditionNode>,
    container_condition_id: ContainerConditionId,
) -> Vec<ParsedRule> {
    let mut rules = Vec::new();
    let mut current_selector = String::new();
    let mut current_decls = String::new();
    let mut block_depth = 0;
    let mut in_comment = false;
    let mut chars = css.chars().peekable();

    while let Some(c) = chars.next() {
        if in_comment {
            if c == '*' && chars.peek() == Some(&'/') {
                chars.next();
                in_comment = false;
            }
            continue;
        }
        if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            in_comment = true;
            continue;
        }

        if c == '{' {
            if block_depth != 0 {
                current_decls.push(c);
            }
            block_depth += 1;
        } else if c == '}' && block_depth == 0 {
            // Stray top-level close brace (unbalanced author CSS; remoteok.com
            // ships one mid-sheet). Browsers error-recover and keep parsing;
            // without this block_depth goes negative and the state machine
            // inverts, scrambling and losing every rule in the rest of the sheet.
            current_selector.clear();
        } else if c == '}' {
            block_depth -= 1;
            if block_depth == 0 {
                let sel = current_selector.trim();
                let decls = current_decls.trim();
                if let Some(at) = sel.strip_prefix('@') {
                    flush_at_rule(
                        at, sel, decls, &mut rules, viewport,
                        container_conditions, container_condition_id,
                    );
                } else {
                    // The body may contain nested rules (CSS Nesting, ubiquitous
                    // in Tailwind v4 / modern frameworks: `.a{ &:hover{} .b{} }`).
                    // Flatten them against this selector; denest also handles the
                    // no-nesting case (just emits the rule's own declarations).
                    denest(
                        sel, decls, &mut rules, viewport,
                        container_conditions, container_condition_id,
                    );
                }
                current_selector.clear();
                current_decls.clear();
            } else {
                current_decls.push(c);
            }
        } else if c == ';' && block_depth == 0 {
            // A `;` at the top level terminates an at-statement that carries no
            // rules (`@layer a, b;`, `@import ...;`, `@charset ...;`). Discard
            // the buffered text so it cannot bleed into the next rule's selector
            // and take a real rule down with it.
            current_selector.clear();
        } else if block_depth > 0 {
            current_decls.push(c);
        } else {
            current_selector.push(c);
        }
    }
    rules
}

/// Handle the at-rules whose bodies contain ordinary rules. For `@media`, apply
/// the inner rules only when the query holds for a desktop 1280px viewport.
fn flush_at_rule(
    at: &str,
    _sel: &str,
    inner: &str,
    rules: &mut Vec<ParsedRule>,
    viewport: (f32, f32),
    container_conditions: &mut Vec<ContainerConditionNode>,
    container_condition_id: ContainerConditionId,
) {
    if let Some(prelude) = at_rule_prelude(at, "media") {
        if media_query_applies_for_viewport(prelude, viewport) {
            rules.extend(parse_stylesheet_for_viewport_preserving_containers(
                inner,
                viewport,
                container_conditions,
                container_condition_id,
            ));
        }
    } else if let Some(prelude) = at_rule_prelude(at, "supports") {
        if supports_condition_applies(prelude) {
            rules.extend(parse_stylesheet_for_viewport_preserving_containers(
                inner,
                viewport,
                container_conditions,
                container_condition_id,
            ));
        }
    } else if let Some(prelude) = at_rule_prelude(at, "container") {
        if let Some(alternatives) = parse_container_query_list(prelude) {
            let Ok(raw_id) = u32::try_from(container_conditions.len()) else {
                return;
            };
            let id = ContainerConditionId(raw_id);
            container_conditions.push(ContainerConditionNode {
                parent: container_condition_id,
                alternatives,
            });
            rules.extend(parse_stylesheet_for_viewport_preserving_containers(
                inner,
                viewport,
                container_conditions,
                id,
            ));
        }
    } else if let Some(name) = at_rule_prelude(at, "property") {
        if name.starts_with("--") {
            rules.push(ParsedRule {
                selector: format!("{PROPERTY_REGISTRATION_SELECTOR_PREFIX}{name}"),
                declarations: inner.to_string(),
                // Registrations are global name-defining rules. CSS
                // Conditional 5 deliberately does not gate them on an
                // enclosing container query.
                container_condition_id: ContainerConditionId::NONE,
            });
        }
    } else if at_rule_prelude(at, "layer").is_some() {
        // Cascade layers: `@layer name { ... }` wraps ordinary rules. We do not
        // model layer priority (real CSS ranks unlayered above layered and
        // later layers above earlier); just flatten the body in source order so
        // the (specificity, source-order) cascade applies it. Tailwind/UnoCSS,
        // Nuxt UI and similar wrap nearly all their CSS, including the `:root`
        // design tokens and background/color utilities, in `@layer`; dropping it
        // left whole pages unstyled (white backgrounds, collapsed layout). The
        // `@layer a, b;` ordering-statement form has no block and is discarded
        // by parse_stylesheet's top-level `;` handling.
        rules.extend(parse_stylesheet_for_viewport_preserving_containers(
            inner,
            viewport,
            container_conditions,
            container_condition_id,
        ));
    }
    // Other at-rules (@font-face, @keyframes, @import, ...) carry no
    // layout-relevant rules for us, so drop them.
}

fn parse_property_registration(descriptors: &str) -> Option<RegisteredCustomProperty> {
    let mut syntax = None;
    let mut inherits = None;
    let mut initial_value = None;
    for declaration in crate::style::split_declarations(descriptors) {
        let Some((name, value)) = declaration.split_once(':') else { continue };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "syntax" => {
                let unquoted = value
                    .strip_prefix('"')
                    .and_then(|value| value.strip_suffix('"'))
                    .or_else(|| {
                        value
                            .strip_prefix('\'')
                            .and_then(|value| value.strip_suffix('\''))
                    })?;
                if !unquoted.is_empty() {
                    syntax = Some(unquoted.to_string());
                }
            }
            "inherits" => {
                inherits = match value.to_ascii_lowercase().as_str() {
                    "true" => Some(true),
                    "false" => Some(false),
                    _ => None,
                };
            }
            "initial-value" if !value.is_empty() => initial_value = Some(value.to_string()),
            _ => {}
        }
    }
    let syntax = syntax?;
    if !matches!(
        syntax.as_str(),
        "*" | "<percentage>" | "<length>" | "<number>" | "<color>"
    ) {
        return None;
    }
    if initial_value.is_none() && syntax != "*" {
        return None;
    }
    let registration = RegisteredCustomProperty {
        syntax,
        inherits: inherits?,
        initial_value,
    };
    if registration
        .initial_value
        .as_deref()
        .is_some_and(|value| !registered_value_matches(&registration, value))
    {
        return None;
    }
    Some(registration)
}

fn registered_value_matches(registration: &RegisteredCustomProperty, value: &str) -> bool {
    let value = value.trim();
    match registration.syntax.as_str() {
        "*" => !value.is_empty(),
        "<percentage>" => value
            .strip_suffix('%')
            .and_then(|number| number.trim().parse::<f32>().ok())
            .is_some_and(f32::is_finite),
        "<number>" => value.parse::<f32>().ok().is_some_and(f32::is_finite),
        "<color>" => crate::style::parse_color(value).is_some(),
        "<length>" => {
            if value.parse::<f32>().ok().is_some_and(|number| number == 0.0) {
                return true;
            }
            let lower = value.to_ascii_lowercase();
            [
                "rem", "em", "ex", "vmin", "vmax", "dvw", "svw", "lvw", "dvh",
                "svh", "lvh", "vw", "vh", "px", "pt",
            ]
            .iter()
            .any(|unit| {
                lower
                    .strip_suffix(unit)
                    .and_then(|number| number.trim().parse::<f32>().ok())
                    .is_some_and(f32::is_finite)
            })
        }
        _ => false,
    }
}

/// Return the prelude after an exact ASCII-insensitive at-rule name.
///
/// The hand parser stores the text after `@`, so a prefix test would both
/// reject `@CONTAINER` and misclassify unknown rules such as
/// `@containerfoo`. The boundary accepts punctuation because whitespace is
/// optional before a parenthesized prelude.
fn at_rule_prelude<'a>(at: &'a str, expected: &str) -> Option<&'a str> {
    let prefix = at.get(..expected.len())?;
    if !prefix.eq_ignore_ascii_case(expected) {
        return None;
    }
    let rest = &at[expected.len()..];
    if rest.chars().next().is_some_and(|character| {
        character.is_ascii_alphanumeric()
            || matches!(character, '_' | '-' | '\\')
            || !character.is_ascii()
    }) {
        return None;
    }
    let mut rest = rest.trim_start();
    while let Some(comment) = rest.strip_prefix("/*") {
        let end = comment.find("*/")?;
        rest = comment[end + 2..].trim_start();
    }
    Some(rest.trim_end())
}

fn parse_container_query_list(prelude: &str) -> Option<Vec<ContainerQuery>> {
    let queries = split_media_query_list(prelude)
        .into_iter()
        .map(parse_container_query)
        .collect::<Option<Vec<_>>>()?;
    (!queries.is_empty()).then_some(queries)
}

fn parse_container_query(input: &str) -> Option<ContainerQuery> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    let starts_with_condition =
        input.starts_with('(') || strip_ascii_keyword(input, "not").is_some();
    let (name, condition) = if starts_with_condition {
        (None, Some(input))
    } else if let Some(split) = input.find(char::is_whitespace) {
        let name = parse_container_query_name(&input[..split])?;
        let condition = input[split..].trim();
        (Some(name), (!condition.is_empty()).then_some(condition))
    } else {
        (Some(parse_container_query_name(input)?), None)
    };
    let condition = match condition {
        Some(condition) => Some(parse_container_query_expr(condition)?),
        None => None,
    };
    Some(ContainerQuery { name, condition })
}

fn parse_container_query_name(input: &str) -> Option<String> {
    let mut input = cssparser::ParserInput::new(input.trim());
    let mut parser = cssparser::Parser::new(&mut input);
    let ident = parser.expect_ident_cloned().ok()?;
    if !parser.is_exhausted() {
        return None;
    }
    let lower = ident.to_ascii_lowercase();
    if is_reserved_container_custom_ident(&lower) {
        return None;
    }
    Some(ident.to_string())
}

fn is_reserved_container_custom_ident(lower: &str) -> bool {
    matches!(
        lower,
        "none"
            | "not"
            | "and"
            | "or"
            | "default"
            | "initial"
            | "inherit"
            | "unset"
            | "revert"
            | "revert-layer"
    )
}

const MAX_CONTAINER_QUERY_DEPTH: usize = 64;

fn parse_container_query_expr(input: &str) -> Option<ContainerQueryExpr> {
    parse_container_query_expr_at_depth(input, 0)
}

fn parse_container_query_expr_at_depth(input: &str, depth: usize) -> Option<ContainerQueryExpr> {
    if depth >= MAX_CONTAINER_QUERY_DEPTH {
        return None;
    }
    let input = input.trim();
    let or_parts = split_supports_operator(input, "or");
    let and_parts = split_supports_operator(input, "and");
    // One grammar level is either a homogeneous AND chain or a homogeneous OR
    // chain. Authors must parenthesize any mixture.
    if or_parts.is_some() && and_parts.is_some() {
        return None;
    }
    if let Some(parts) = or_parts {
        return Some(ContainerQueryExpr::Or(
            parts
                .into_iter()
                .map(|part| parse_container_query_in_parens(part, depth + 1))
                .collect::<Option<_>>()?,
        ));
    }
    if let Some(parts) = and_parts {
        return Some(ContainerQueryExpr::And(
            parts
                .into_iter()
                .map(|part| parse_container_query_in_parens(part, depth + 1))
                .collect::<Option<_>>()?,
        ));
    }
    if let Some(rest) = strip_ascii_keyword(input, "not") {
        return Some(ContainerQueryExpr::Not(Box::new(
            parse_container_query_in_parens(rest, depth + 1)?,
        )));
    }
    parse_container_query_in_parens(input, depth + 1)
}

fn parse_container_query_in_parens(input: &str, depth: usize) -> Option<ContainerQueryExpr> {
    if depth >= MAX_CONTAINER_QUERY_DEPTH {
        return None;
    }
    let inner = enclosing_parenthesized(input)?;
    if let Some(feature) = parse_container_size_feature(inner) {
        return Some(feature);
    }
    parse_container_query_expr_at_depth(inner, depth + 1).or_else(|| {
        is_general_enclosed_container_query(inner).then_some(ContainerQueryExpr::Unknown)
    })
}

fn is_general_enclosed_container_query(input: &str) -> bool {
    let mut input = cssparser::ParserInput::new(input.trim());
    let mut parser = cssparser::Parser::new(&mut input);
    matches!(
        parser.next(),
        Ok(cssparser::Token::Ident(_)) | Ok(cssparser::Token::Function(_))
    )
}

fn strip_ascii_keyword<'a>(input: &'a str, keyword: &str) -> Option<&'a str> {
    if !input.get(..keyword.len())?.eq_ignore_ascii_case(keyword) {
        return None;
    }
    let rest = &input[keyword.len()..];
    rest.chars().next()
        .filter(|character| character.is_whitespace())
        .map(|_| rest.trim_start())
}

fn parse_container_size_feature(input: &str) -> Option<ContainerQueryExpr> {
    if let Some(axis) = parse_container_query_axis(input) {
        return Some(ContainerQueryExpr::Feature(ContainerSizeFeature {
            axis,
            comparison: ContainerQueryComparison::GreaterThan,
            length: ContainerQueryLength::Px(0.0),
        }));
    }
    if let Some((name, value)) = input.split_once(':') {
        let (comparison, axis) = match name.trim().to_ascii_lowercase().as_str() {
            "min-width" => (ContainerQueryComparison::Min, ContainerQueryAxis::Width),
            "max-width" => (ContainerQueryComparison::Max, ContainerQueryAxis::Width),
            "width" => (ContainerQueryComparison::Equal, ContainerQueryAxis::Width),
            "min-height" => (ContainerQueryComparison::Min, ContainerQueryAxis::Height),
            "max-height" => (ContainerQueryComparison::Max, ContainerQueryAxis::Height),
            "height" => (ContainerQueryComparison::Equal, ContainerQueryAxis::Height),
            "min-inline-size" => (ContainerQueryComparison::Min, ContainerQueryAxis::InlineSize),
            "max-inline-size" => (ContainerQueryComparison::Max, ContainerQueryAxis::InlineSize),
            "inline-size" => (ContainerQueryComparison::Equal, ContainerQueryAxis::InlineSize),
            "min-block-size" => (ContainerQueryComparison::Min, ContainerQueryAxis::BlockSize),
            "max-block-size" => (ContainerQueryComparison::Max, ContainerQueryAxis::BlockSize),
            "block-size" => (ContainerQueryComparison::Equal, ContainerQueryAxis::BlockSize),
            _ => return None,
        };
        return Some(ContainerQueryExpr::Feature(ContainerSizeFeature {
            axis,
            comparison,
            length: parse_container_query_length(value)?,
        }));
    }

    let (operands, operators) = split_container_range(input)?;
    let make_feature =
        |axis: ContainerQueryAxis, operator: &str, value: &str, axis_on_left: bool| {
            let comparison = match (operator, axis_on_left) {
                (">=", true) | ("<=", false) => ContainerQueryComparison::Min,
                ("<=", true) | (">=", false) => ContainerQueryComparison::Max,
                (">", true) | ("<", false) => ContainerQueryComparison::GreaterThan,
                ("<", true) | (">", false) => ContainerQueryComparison::LessThan,
                ("=", _) => ContainerQueryComparison::Equal,
                _ => return None,
            };
            Some(ContainerQueryExpr::Feature(ContainerSizeFeature {
                axis,
                comparison,
                length: parse_container_query_length(value)?,
            }))
        };
    match (operands.as_slice(), operators.as_slice()) {
        ([left, right], [operator]) => {
            if let Some(axis) = parse_container_query_axis(left) {
                make_feature(axis, operator, right, true)
            } else {
                let axis = parse_container_query_axis(right)?;
                make_feature(axis, operator, left, false)
            }
        }
        ([lower, middle, upper], [lower_operator, upper_operator]) => {
            // Chained ranges must point consistently through the feature:
            // `10px < width <= 20px` or the fully reversed equivalent.
            // Equality is valid only in a single comparison, and a mixed
            // direction such as `10px < width > 20px` is not a range.
            let forward = matches!(*lower_operator, "<" | "<=")
                && matches!(*upper_operator, "<" | "<=");
            let reverse = matches!(*lower_operator, ">" | ">=")
                && matches!(*upper_operator, ">" | ">=");
            if !forward && !reverse {
                return None;
            }
            let axis = parse_container_query_axis(middle)?;
            Some(ContainerQueryExpr::And(vec![
                make_feature(axis, lower_operator, lower, false)?,
                make_feature(axis, upper_operator, upper, true)?,
            ]))
        }
        _ => None,
    }
}

fn parse_container_query_axis(input: &str) -> Option<ContainerQueryAxis> {
    match input.trim().to_ascii_lowercase().as_str() {
        "width" => Some(ContainerQueryAxis::Width),
        "height" => Some(ContainerQueryAxis::Height),
        "inline-size" => Some(ContainerQueryAxis::InlineSize),
        "block-size" => Some(ContainerQueryAxis::BlockSize),
        _ => None,
    }
}

fn split_container_range(input: &str) -> Option<(Vec<&str>, Vec<&str>)> {
    let mut operands = Vec::new();
    let mut operators = Vec::new();
    let mut start = 0usize;
    let bytes = input.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if matches!(bytes[index], b'<' | b'>' | b'=') {
            let end = if bytes.get(index + 1) == Some(&b'=') {
                index + 2
            } else {
                index + 1
            };
            let operand = input[start..index].trim();
            if operand.is_empty() {
                return None;
            }
            operands.push(operand);
            operators.push(&input[index..end]);
            start = end;
            index = end;
            continue;
        }
        index += 1;
    }
    if operators.is_empty() || operators.len() > 2 {
        return None;
    }
    let operand = input[start..].trim();
    if operand.is_empty() {
        return None;
    }
    operands.push(operand);
    Some((operands, operators))
}

fn parse_container_query_length(input: &str) -> Option<ContainerQueryLength> {
    let input = input.trim().to_ascii_lowercase();
    let number = |value: &str| {
        value.parse::<f32>().ok().filter(|value| value.is_finite())
    };
    if let Some(value) = input.strip_suffix("rem").and_then(number) {
        return Some(ContainerQueryLength::Rem(value));
    }
    if let Some(value) = input.strip_suffix("em").and_then(number) {
        return Some(ContainerQueryLength::Em(value));
    }
    if let Some(value) = input.strip_suffix("px").and_then(number) {
        return Some(ContainerQueryLength::Px(value));
    }
    number(&input).filter(|value| *value == 0.0).map(ContainerQueryLength::Px)
}

/// Evaluate the boolean subset of CSS Conditional Rules used by modern
/// framework stylesheets. This follows the same shape as Gecko's
/// SupportsCondition tree: declaration/selector leaves, arbitrary grouping,
/// unary `not`, and top-level `and`/`or` chains. Unknown or malformed future
/// syntax is false instead of optimistically enabling a fallback branch.
pub(crate) fn supports_condition_applies(condition: &str) -> bool {
    let condition = condition.trim();
    let condition = condition
        .strip_prefix("@supports")
        .or_else(|| condition.strip_prefix("supports"))
        .unwrap_or(condition)
        .trim();
    eval_supports_condition(condition)
}

fn eval_supports_condition(condition: &str) -> bool {
    let condition = condition.trim();
    if condition.is_empty() {
        return false;
    }
    if let Some(inner) = enclosing_parenthesized(condition) {
        return eval_supports_condition(inner);
    }
    if condition
        .get(..3)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("not"))
        && condition
            .as_bytes()
            .get(3)
            .is_some_and(u8::is_ascii_whitespace)
    {
        return !eval_supports_condition(condition[3..].trim());
    }
    if let Some(parts) = split_supports_operator(condition, "or") {
        return parts.into_iter().any(eval_supports_condition);
    }
    if let Some(parts) = split_supports_operator(condition, "and") {
        return parts.into_iter().all(eval_supports_condition);
    }
    let lower = condition.to_ascii_lowercase();
    if lower.starts_with("selector(") && condition.ends_with(')') {
        let inner = &condition["selector(".len()..condition.len() - 1];
        return obscura_dom::selector::parse_selector(inner.trim()).is_ok();
    }
    let Some((name, value)) = condition.split_once(':') else {
        return false;
    };
    crate::style::supports_declaration(name, value)
}

/// Return the contents when one outer parenthesis pair encloses the complete
/// expression. A declaration leaf such as `(display:grid)` intentionally
/// becomes `display:grid` and is handled after boolean operators.
fn enclosing_parenthesized(condition: &str) -> Option<&str> {
    if !condition.starts_with('(') {
        return None;
    }
    let mut depth = 0i32;
    let mut quote = None;
    for (index, character) in condition.char_indices() {
        if let Some(active) = quote {
            if character == active {
                quote = None;
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
                if depth == 0 {
                    return (index + character.len_utf8() == condition.len())
                        .then_some(&condition[1..index]);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_supports_operator<'a>(condition: &'a str, operator: &str) -> Option<Vec<&'a str>> {
    let bytes = condition.as_bytes();
    let operator = operator.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut index = 0usize;
    let mut depth = 0i32;
    let mut quote = None;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(active) = quote {
            if byte == active {
                quote = None;
            } else if byte == b'\\' {
                index += 1;
            }
            index += 1;
            continue;
        }
        match byte {
            b'\'' | b'"' => quote = Some(byte),
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
            }
            _ if depth == 0
                && index + operator.len() <= bytes.len()
                && bytes[index..index + operator.len()].eq_ignore_ascii_case(operator)
                && index > 0
                && bytes[index - 1].is_ascii_whitespace()
                && bytes
                    .get(index + operator.len())
                    .is_some_and(u8::is_ascii_whitespace) =>
            {
                let part = condition[start..index].trim();
                if part.is_empty() {
                    return None;
                }
                parts.push(part);
                index += operator.len();
                start = index;
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    if depth != 0 || quote.is_some() || parts.is_empty() {
        return None;
    }
    let tail = condition[start..].trim();
    if tail.is_empty() {
        return None;
    }
    parts.push(tail);
    Some(parts)
}

/// Coarse `@media` evaluation against the live layout viewport.
///
/// Real stylesheets format media features inconsistently
/// (`max-width:750px`, `max-width: 750px`, even `max-width : 750px`), so this
/// strips whitespace before scanning: CSS gives no semantic meaning to spaces
/// inside `(feature: value)`, so it's safe to discard them wholesale rather
/// than special-case every formatting variant a site might use.
pub(crate) fn media_query_applies_for_viewport(
    query: &str,
    viewport: (f32, f32),
) -> bool {
    // A media-query list is an OR, not an AND. Evaluate each top-level comma
    // arm independently (commas inside functions such as rgb() / calc() are
    // not list separators). This also keeps an inapplicable `print` arm from
    // suppressing a later screen/feature arm.
    split_media_query_list(query)
        .into_iter()
        .any(|query| single_media_query_applies_for_viewport(query, viewport))
}

fn single_media_query_applies_for_viewport(
    query: &str,
    viewport: (f32, f32),
) -> bool {
    let viewport_w = viewport.0;
    let viewport_h = viewport.1;
    let query = query.trim().strip_prefix("@media").unwrap_or(query).trim();
    let compact: String = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect();

    // Tailwind expresses every `max-*` breakpoint as the negation of a
    // min-width query (`not all and (min-width:40rem)`). Evaluate the inner
    // condition and invert it; treating all `not all` queries as permanently
    // false makes mutually exclusive responsive utilities apply together at
    // desktop widths. Unknown browser-targeting features still conservatively
    // evaluate true inside and therefore false after negation.
    if let Some(inner) = compact.strip_prefix("notalland") {
        return !single_media_query_applies_for_viewport(inner, viewport);
    }
    if compact == "notall" {
        return false;
    }
    if compact.contains("print") {
        return false;
    }

    // Color-scheme: we render the light (default) context. A site's
    // `@media (prefers-color-scheme: dark)` block must NOT apply on top of its
    // light defaults (that is what was leaking dark backgrounds, e.g. near
    // black inline <code>); a `:light` block should apply.
    if compact.contains("prefers-color-scheme:dark") {
        return false;
    }
    // Reduced-motion / high-contrast / inverted: default (no preference).
    if compact.contains("prefers-reduced-motion:reduce")
        || compact.contains("prefers-contrast:more")
        || compact.contains("prefers-contrast:less")
        || compact.contains("inverted-colors:inverted")
        || compact.contains("forced-colors:active")
    {
        return false;
    }

    // Width constraints, both `min-width:`/`max-width:` and the modern range
    // forms `width>=Npx` / `(Npx<=width)`.
    if let Some(px) = extract_length(&compact, "max-width:", viewport, LengthAxis::Width) {
        if viewport_w > px {
            return false;
        }
    }
    if let Some(px) = extract_length(&compact, "min-width:", viewport, LengthAxis::Width) {
        if viewport_w < px {
            return false;
        }
    }
    if let Some(px) = extract_length(&compact, "width<=", viewport, LengthAxis::Width) {
        if viewport_w > px {
            return false;
        }
    }
    if let Some(px) = extract_length(&compact, "width>=", viewport, LengthAxis::Width) {
        if viewport_w < px {
            return false;
        }
    }
    if let Some(px) = extract_length(&compact, "width>", viewport, LengthAxis::Width) {
        if viewport_w <= px {
            return false;
        }
    }
    if let Some(px) = extract_length(&compact, "width<", viewport, LengthAxis::Width) {
        if viewport_w >= px {
            return false;
        }
    }
    if let Some(px) = extract_length_before(&compact, "<=width", viewport, LengthAxis::Width) {
        if viewport_w < px {
            return false;
        }
    }
    if let Some(px) = extract_length_before(&compact, "<width", viewport, LengthAxis::Width) {
        if viewport_w <= px {
            return false;
        }
    }
    if let Some(px) = extract_length_before(&compact, ">=width", viewport, LengthAxis::Width) {
        if viewport_w > px {
            return false;
        }
    }
    if let Some(px) = extract_length_before(&compact, ">width", viewport, LengthAxis::Width) {
        if viewport_w >= px {
            return false;
        }
    }

    if let Some(px) = extract_length(&compact, "max-height:", viewport, LengthAxis::Height) {
        if viewport_h > px {
            return false;
        }
    }
    if let Some(px) = extract_length(&compact, "min-height:", viewport, LengthAxis::Height) {
        if viewport_h < px {
            return false;
        }
    }
    if let Some(px) = extract_length(&compact, "height<=", viewport, LengthAxis::Height) {
        if viewport_h > px {
            return false;
        }
    }
    if let Some(px) = extract_length(&compact, "height>=", viewport, LengthAxis::Height) {
        if viewport_h < px {
            return false;
        }
    }
    if let Some(px) = extract_length(&compact, "height>", viewport, LengthAxis::Height) {
        if viewport_h <= px {
            return false;
        }
    }
    if let Some(px) = extract_length(&compact, "height<", viewport, LengthAxis::Height) {
        if viewport_h >= px {
            return false;
        }
    }
    if let Some(px) = extract_length_before(&compact, "<=height", viewport, LengthAxis::Height) {
        if viewport_h < px {
            return false;
        }
    }
    if let Some(px) = extract_length_before(&compact, "<height", viewport, LengthAxis::Height) {
        if viewport_h <= px {
            return false;
        }
    }
    if let Some(px) = extract_length_before(&compact, ">=height", viewport, LengthAxis::Height) {
        if viewport_h > px {
            return false;
        }
    }
    if let Some(px) = extract_length_before(&compact, ">height", viewport, LengthAxis::Height) {
        if viewport_h >= px {
            return false;
        }
    }
    if compact.contains("orientation:portrait") && viewport_w > viewport_h {
        return false;
    }
    if compact.contains("orientation:landscape") && viewport_h > viewport_w {
        return false;
    }
    true
}

/// Combine a nested selector `child` with its enclosing `parent` (CSS Nesting).
/// `&` in the child is replaced by the parent; a child with no `&` becomes a
/// descendant (`parent child`). Both may be comma lists, so the result is the
/// cartesian product. `parent` is None only at the stylesheet top level, where
/// the child is returned unchanged.
fn combine_selectors(parent: &str, child: &str) -> String {
    let pparts = split_selector_list(parent);
    let cparts = split_selector_list(child);
    let mut out: Vec<String> = Vec::new();
    for c in &cparts {
        let c = c.trim();
        if c.is_empty() {
            continue;
        }
        for p in &pparts {
            let p = p.trim();
            if c.contains('&') {
                out.push(c.replace('&', p));
            } else {
                out.push(format!("{} {}", p, c));
            }
        }
    }
    out.join(", ")
}

/// Flatten a rule body that may contain nested rules (CSS Nesting) into flat
/// `(selector, declarations)` pairs. The parser hands us the whole body of a
/// rule; here we separate its own declarations from nested `sel { ... }` blocks
/// (and nested `@media`/`@supports`/`@layer` at-rules, which keep the parent's
/// selector), emit `(sel, own-declarations)`, and recurse into each nested rule
/// with the combined selector. Without this, Tailwind v4 / modern-framework CSS
/// (which nests almost everything) loses the nested utility rules entirely.
fn denest(
    sel: &str,
    body: &str,
    rules: &mut Vec<ParsedRule>,
    viewport: (f32, f32),
    container_conditions: &mut Vec<ContainerConditionNode>,
    container_condition_id: ContainerConditionId,
) {
    let chars: Vec<char> = body.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut seg = 0; // start of the current declaration / nested prelude
    let mut own = String::new();
    let mut quote: Option<char> = None;
    let mut comment = false;
    let mut paren = 0i32;
    while i < n {
        let c = chars[i];
        if comment {
            if c == '*' && chars.get(i + 1) == Some(&'/') {
                comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            comment = true;
            i += 2;
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '(' => paren += 1,
            ')' => paren = (paren - 1).max(0),
            '{' if paren == 0 => {
                let prelude: String = chars[seg..i].iter().collect();
                // Find the matching close brace (quote/comment aware).
                let mut depth = 1;
                let mut j = i + 1;
                let (mut q2, mut cm2) = (None::<char>, false);
                while j < n && depth > 0 {
                    let cj = chars[j];
                    if cm2 {
                        if cj == '*' && chars.get(j + 1) == Some(&'/') {
                            cm2 = false;
                            j += 2;
                            continue;
                        }
                    } else if let Some(qq) = q2 {
                        if cj == qq {
                            q2 = None;
                        }
                    } else if cj == '/' && chars.get(j + 1) == Some(&'*') {
                        cm2 = true;
                        j += 2;
                        continue;
                    } else if cj == '\'' || cj == '"' {
                        q2 = Some(cj);
                    } else if cj == '{' {
                        depth += 1;
                    } else if cj == '}' {
                        depth -= 1;
                    }
                    j += 1;
                }
                let inner: String = chars[i + 1..j.saturating_sub(1).max(i + 1)].iter().collect();
                let pre = prelude.trim();
                if let Some(at) = pre.strip_prefix('@') {
                    // A nested at-rule keeps the enclosing selector for its body.
                    if let Some(prelude) = at_rule_prelude(at, "media") {
                        if media_query_applies_for_viewport(prelude, viewport) {
                            denest(
                                sel,
                                &inner,
                                rules,
                                viewport,
                                container_conditions,
                                container_condition_id,
                            );
                        }
                    } else if let Some(prelude) = at_rule_prelude(at, "supports") {
                        if supports_condition_applies(prelude) {
                            denest(
                                sel,
                                &inner,
                                rules,
                                viewport,
                                container_conditions,
                                container_condition_id,
                            );
                        }
                    } else if let Some(prelude) = at_rule_prelude(at, "container") {
                        if let Some(alternatives) = parse_container_query_list(prelude) {
                            if let Ok(raw_id) = u32::try_from(container_conditions.len()) {
                                let id = ContainerConditionId(raw_id);
                                container_conditions.push(ContainerConditionNode {
                                    parent: container_condition_id,
                                    alternatives,
                                });
                                denest(sel, &inner, rules, viewport, container_conditions, id);
                            }
                        }
                    } else if at_rule_prelude(at, "layer").is_some() {
                        denest(
                            sel,
                            &inner,
                            rules,
                            viewport,
                            container_conditions,
                            container_condition_id,
                        );
                    }
                } else if !pre.is_empty() {
                    let full = combine_selectors(sel, pre);
                    denest(&full, &inner, rules, viewport, container_conditions, container_condition_id);
                }
                i = j;
                seg = i;
                continue;
            }
            ';' if paren == 0 => {
                let d: String = chars[seg..i].iter().collect();
                let d = d.trim();
                if !d.is_empty() {
                    own.push_str(d);
                    own.push(';');
                }
                i += 1;
                seg = i;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    let tail: String = chars[seg..].iter().collect();
    let tail = tail.trim();
    if !tail.is_empty() && !tail.contains('{') {
        own.push_str(tail);
        own.push(';');
    }
    if !own.trim().is_empty() {
        for s in split_selector_list(sel) {
            let s = s.trim();
            if !s.is_empty() {
                rules.push(ParsedRule {
                    selector: s.to_string(),
                    declarations: own.clone(),
                    container_condition_id,
                });
            }
        }
    }
}

/// Split a CSS selector list at top-level commas only, leaving commas inside
/// `()` (e.g. `:is(a, b)`, `:not(.x, .y)`) and `[]` (`[attr="a,b"]`) and quoted
/// strings intact. A naive `split(',')` shatters those grouped selectors into
/// fragments that fail to compile, dropping the whole rule.
fn split_selector_list(sel: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth_paren = 0i32;
    let mut depth_brack = 0i32;
    let mut quote: Option<char> = None;
    let mut cur = String::new();
    let mut chars = sel.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                cur.push(c);
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            _ if quote == Some(c) => {
                quote = None;
                cur.push(c);
            }
            '"' | '\'' if quote.is_none() => {
                quote = Some(c);
                cur.push(c);
            }
            '(' if quote.is_none() => {
                depth_paren += 1;
                cur.push(c);
            }
            ')' if quote.is_none() => {
                depth_paren -= 1;
                cur.push(c);
            }
            '[' if quote.is_none() => {
                depth_brack += 1;
                cur.push(c);
            }
            ']' if quote.is_none() => {
                depth_brack -= 1;
                cur.push(c);
            }
            ',' if quote.is_none() && depth_paren == 0 && depth_brack == 0 => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

#[derive(Clone, Copy)]
enum LengthAxis {
    Width,
    Height,
}

/// Read a CSS length immediately following `prop`. Media-query `em` and `rem`
/// units resolve against the initial font size (16 CSS px), not an element's
/// computed font. Modern utility frameworks deliberately use those units for
/// breakpoints, so treating only `px` as typed made every `min-width:64rem`
/// desktop rule unconditional.
fn extract_length(
    s: &str,
    prop: &str,
    viewport: (f32, f32),
    axis: LengthAxis,
) -> Option<f32> {
    let start = s.find(prop)? + prop.len();
    let rest = &s[start..];
    if let Some(inner) = rest.strip_prefix("calc(") {
        let end = inner.find(')')?;
        return eval_length_sum(&inner[..end], viewport, axis);
    }
    parse_length_prefix(rest, viewport, axis)
}

/// Read the length immediately before a range marker (`64rem<=width`).
fn extract_length_before(
    s: &str,
    marker: &str,
    viewport: (f32, f32),
    axis: LengthAxis,
) -> Option<f32> {
    let end = s.find(marker)?;
    let prefix = &s[..end];
    let start = prefix
        .rfind(|c: char| matches!(c, '(' | ')' | ':' | ','))
        .map_or(0, |idx| idx + 1);
    let value = &prefix[start..];
    if let Some(inner) = value.strip_prefix("calc(") {
        return eval_length_sum(inner, viewport, axis);
    }
    parse_length_prefix(value, viewport, axis)
}

fn parse_length_prefix(
    input: &str,
    viewport: (f32, f32),
    axis: LengthAxis,
) -> Option<f32> {
    let numeric_len = input
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_digit() || matches!(c, '.' | '+' | '-'))
        .last()
        .map_or(0, |(idx, c)| idx + c.len_utf8());
    if numeric_len == 0 {
        return None;
    }
    let value = input[..numeric_len].parse::<f32>().ok()?;
    let unit: String = input[numeric_len..]
        .chars()
        .take_while(|c| c.is_ascii_alphabetic() || *c == '%')
        .collect();
    let px = match unit.as_str() {
        "" | "px" => value,
        "em" | "rem" => value * 16.0,
        "vw" => value * viewport.0 / 100.0,
        "vh" => value * viewport.1 / 100.0,
        "vmin" => value * viewport.0.min(viewport.1) / 100.0,
        "vmax" => value * viewport.0.max(viewport.1) / 100.0,
        "in" => value * 96.0,
        "cm" => value * 96.0 / 2.54,
        "mm" => value * 96.0 / 25.4,
        "q" => value * 96.0 / 101.6,
        "pt" => value * 96.0 / 72.0,
        "pc" => value * 16.0,
        "%" => match axis {
            LengthAxis::Width => value * viewport.0 / 100.0,
            LengthAxis::Height => value * viewport.1 / 100.0,
        },
        _ => return None,
    };
    px.is_finite().then_some(px)
}

/// Sum the common media-query `calc()` form (`64rem - 1px`) left to right.
fn eval_length_sum(
    expr: &str,
    viewport: (f32, f32),
    axis: LengthAxis,
) -> Option<f32> {
    let mut total = 0.0;
    let mut sign = 1.0;
    let mut term = String::new();
    let flush = |term: &mut String, sign: f32, total: &mut f32| -> Option<()> {
        let value = parse_length_prefix(term, viewport, axis)?;
        *total += sign * value;
        term.clear();
        Some(())
    };
    for (idx, c) in expr.char_indices() {
        match c {
            '+' if idx > 0 => {
                flush(&mut term, sign, &mut total)?;
                sign = 1.0;
            }
            '-' if idx > 0 => {
                flush(&mut term, sign, &mut total)?;
                sign = -1.0;
            }
            _ => term.push(c),
        }
    }
    flush(&mut term, sign, &mut total)?;
    Some(total)
}

fn split_media_query_list(query: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut start = 0usize;
    for (idx, c) in query.char_indices() {
        match c {
            _ if quote == Some(c) => quote = None,
            '\'' | '"' if quote.is_none() => quote = Some(c),
            '(' if quote.is_none() => depth += 1,
            ')' if quote.is_none() => depth = (depth - 1).max(0),
            ',' if quote.is_none() && depth == 0 => {
                parts.push(query[start..idx].trim());
                start = idx + 1;
            }
            _ => {}
        }
    }
    parts.push(query[start..].trim());
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn condition_arena_root() -> Vec<ContainerConditionNode> {
        vec![ContainerConditionNode {
            parent: ContainerConditionId::NONE,
            alternatives: Vec::new(),
        }]
    }

    #[test]
    fn generated_content_resolves_host_attributes_and_resets() {
        let tree = obscura_dom::parse_html(
            r#"<button id="cta" data-label="Get Started"></button>"#,
        );
        let cta = tree.query_selector("#cta").unwrap().unwrap();
        assert_eq!(
            extract_content("content:attr(data-label)", &tree, cta),
            Some(Some("Get Started".to_string()))
        );
        assert_eq!(
            extract_content(r#"content:"fallback";content:none"#, &tree, cta),
            Some(None)
        );
    }

    #[test]
    fn keyframes_retain_every_animation_offset() {
        let css = r#"
            @keyframes dismiss {
                from { opacity: 1; visibility: visible; }
                50% { opacity: .5; }
                to { opacity: 0; visibility: hidden; }
            }
            @-webkit-keyframes slide {
                0% { transform: translateX(0); }
                100% { transform: translateX(20px); }
            }
        "#;
        let keyframes: HashMap<_, _> = extract_keyframes(css).into_iter().collect();
        let dismiss = normalized_keyframe_offsets(&keyframes["dismiss"].stops);
        assert_eq!(
            dismiss.iter().map(|(offset, _)| *offset).collect::<Vec<_>>(),
            [0.0, 0.5, 1.0]
        );
        assert!(dismiss[0].1.declarations.contains("opacity: 1"));
        assert!(dismiss[2].1.declarations.contains("visibility: hidden"));
        let slide = normalized_keyframe_offsets(&keyframes["slide"].stops);
        assert_eq!(
            slide.iter().map(|(offset, _)| *offset).collect::<Vec<_>>(),
            [0.0, 1.0]
        );
        assert!(slide[1].1.declarations.contains("translateX(20px)"));
    }

    fn sampled_animation_style(css: &str, sample_ms: f32, target_id: &str) -> LayoutStyle {
        let tree = obscura_dom::parse_html(&format!(r#"<div id="{target_id}"></div>"#));
        let target = tree.get_element_by_id(target_id).unwrap();
        let sheet = Stylesheet::parse_for_viewport_at_animation_time(
            &tree,
            &[css.to_string()],
            (1280.0, 720.0),
            crate::AnimationSampleTime {
                milliseconds: sample_ms,
            },
        );
        let node = tree.get_node(target).unwrap();
        let element = node.as_element().unwrap();
        let mut matcher = tree.matcher();
        let mut style = LayoutStyle::default();
        sheet.apply(
            &tree,
            &mut matcher,
            target,
            node.get_attribute("id"),
            &[],
            element.local.as_ref(),
            &mut style,
            &HashMap::new(),
            None,
        );
        style
    }

    fn sampled_fade(extra: &str, sample_ms: f32) -> f32 {
        let css = format!(
            r#"
                @keyframes fade {{ from {{ opacity:0 }} to {{ opacity:1 }} }}
                #target {{
                    opacity:.4;
                    animation:fade 1s linear infinite;
                    {extra}
                }}
            "#
        );
        sampled_animation_style(&css, sample_ms, "target")
            .opacity
            .unwrap_or(1.0)
    }

    fn assert_opacity(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 0.0001,
            "expected opacity {expected}, got {actual}"
        );
    }

    #[test]
    fn animation_timeline_handles_delay_fill_iterations_and_direction_at_t0() {
        assert_opacity(sampled_fade("", 0.0), 0.0);
        assert_opacity(sampled_fade("animation-delay:250ms", 0.0), 0.4);
        assert_opacity(
            sampled_fade("animation-delay:250ms;animation-fill-mode:backwards", 0.0),
            0.0,
        );
        assert_opacity(
            sampled_fade(
                "animation-delay:250ms;animation-fill-mode:backwards;animation-direction:reverse",
                0.0,
            ),
            1.0,
        );
        assert_opacity(sampled_fade("animation-delay:-250ms", 0.0), 0.25);
        assert_opacity(sampled_fade("animation-delay:-1s", 0.0), 0.0);
        assert_opacity(
            sampled_fade("animation-delay:-1s;animation-direction:alternate", 0.0),
            1.0,
        );
        assert_opacity(
            sampled_fade(
                "animation-delay:-1s;animation-iteration-count:1;animation-fill-mode:none",
                0.0,
            ),
            0.4,
        );
        assert_opacity(
            sampled_fade(
                "animation-delay:-1s;animation-iteration-count:1;animation-fill-mode:forwards",
                0.0,
            ),
            1.0,
        );
        assert_opacity(
            sampled_fade(
                "animation-delay:-1s;animation-iteration-count:1;animation-fill-mode:forwards;animation-direction:reverse",
                0.0,
            ),
            0.0,
        );
        assert_opacity(
            sampled_fade(
                "animation-delay:-2s;animation-iteration-count:2;animation-fill-mode:forwards;animation-direction:alternate",
                0.0,
            ),
            0.0,
        );
        assert_opacity(
            sampled_fade(
                "animation-iteration-count:0;animation-fill-mode:forwards",
                0.0,
            ),
            0.0,
        );
        assert_opacity(
            sampled_fade(
                "animation-duration:0s;animation-iteration-count:1;animation-fill-mode:none",
                0.0,
            ),
            0.4,
        );
        assert_opacity(
            sampled_fade(
                "animation-duration:0s;animation-iteration-count:1;animation-fill-mode:forwards",
                0.0,
            ),
            1.0,
        );
        assert_opacity(
            sampled_fade(
                "animation-delay:-2.5s;animation-iteration-count:2.5;animation-fill-mode:forwards",
                0.0,
            ),
            0.5,
        );
    }

    #[test]
    fn animation_calc_delay_pause_and_important_origin_are_deterministic() {
        assert_opacity(
            sampled_fade("--step:.1s;animation-delay:calc(var(--step) * -2.5)", 0.0),
            0.25,
        );
        assert_opacity(sampled_fade("animation-play-state:paused", 500.0), 0.0);
        assert_opacity(sampled_fade("", 500.0), 0.5);
        assert_opacity(
            sampled_fade(
                "animation-delay:-250ms !important;opacity:.8 !important",
                0.0,
            ),
            0.8,
        );
    }

    #[test]
    fn opacity_segments_use_underlying_endpoints_and_exact_later_boundaries() {
        let missing_endpoints = r#"
            @keyframes middle { 50% { opacity:1 } }
            #target { opacity:.4; animation:middle 1s linear 1 both }
        "#;
        assert_opacity(
            sampled_animation_style(missing_endpoints, 250.0, "target")
                .opacity
                .unwrap(),
            0.7,
        );
        assert_opacity(
            sampled_animation_style(missing_endpoints, 750.0, "target")
                .opacity
                .unwrap(),
            0.7,
        );
        let exact_boundary = r#"
            @keyframes peak {
                0% { opacity:0 }
                50% { opacity:1 }
                100% { opacity:0 }
            }
            #target { opacity:.4; animation:peak 1s linear 1 both }
        "#;
        assert_opacity(
            sampled_animation_style(exact_boundary, 500.0, "target")
                .opacity
                .unwrap(),
            1.0,
        );
    }

    #[test]
    fn missing_keyframe_offsets_follow_web_animation_distribution() {
        let stop = |offset| KeyframeStop {
            offset,
            declarations: "opacity:1".to_string(),
            source_order: 0,
        };
        let single = [stop(None)];
        assert_eq!(normalized_keyframe_offsets(&single)[0].0, 1.0);
        let pair = [stop(None), stop(None)];
        assert_eq!(
            normalized_keyframe_offsets(&pair)
                .iter()
                .map(|(offset, _)| *offset)
                .collect::<Vec<_>>(),
            [0.0, 1.0]
        );
        let distributed = [
            stop(Some(0.0)),
            stop(None),
            stop(None),
            stop(Some(0.75)),
            stop(None),
        ];
        assert_eq!(
            normalized_keyframe_offsets(&distributed)
                .iter()
                .map(|(offset, _)| *offset)
                .collect::<Vec<_>>(),
            [0.0, 0.25, 0.5, 0.75, 1.0]
        );
    }

    #[test]
    fn mozilla_stagger_samples_only_the_delay_zero_frame() {
        let mut html = String::new();
        let mut selectors = String::new();
        for frame in 0..12 {
            html.push_str(&format!(r#"<svg id="frame{frame}" class="frame"></svg>"#));
            selectors.push_str(&format!(
                "#frame{frame}{{animation-delay:calc(var(--base-delay) * {frame})}}"
            ));
        }
        let tree = obscura_dom::parse_html(&html);
        let css = format!(
            r#"
                @keyframes wave {{
                    0%, 8.333% {{ opacity:1 }}
                    8.4%, to {{ opacity:0 }}
                }}
                .frame {{
                    --base-delay:.1s;
                    opacity:0;
                    animation:wave 1.2s linear infinite;
                }}
                {selectors}
            "#
        );
        let sheet = Stylesheet::parse_for_viewport_at_animation_time(
            &tree,
            &[css],
            (1280.0, 720.0),
            crate::AnimationSampleTime::default(),
        );
        for frame in 0..12 {
            let id = format!("frame{frame}");
            let target = tree.get_element_by_id(&id).unwrap();
            let node = tree.get_node(target).unwrap();
            let element = node.as_element().unwrap();
            let classes = vec!["frame".to_string()];
            let mut matcher = tree.matcher();
            let mut style = LayoutStyle::default();
            sheet.apply(
                &tree,
                &mut matcher,
                target,
                node.get_attribute("id"),
                &classes,
                element.local.as_ref(),
                &mut style,
                &HashMap::new(),
                None,
            );
            assert_opacity(style.opacity.unwrap(), if frame == 0 { 1.0 } else { 0.0 });
        }
    }

    #[test]
    fn media_rules_use_the_live_width_and_height() {
        let css = r#"
            .base { color: black; }
            @media (max-width: 950px) { .narrow { color: green; } }
            @media (min-height: 900px) { .tall { color: blue; } }
            @media (orientation: portrait) { .portrait { color: red; } }
        "#;
        let selectors = |viewport| {
            parse_stylesheet_for_viewport(css, viewport)
                .into_iter()
                .map(|(selector, _)| selector)
                .collect::<Vec<_>>()
        };

        let narrow_tall = selectors((900.0, 1000.0));
        assert!(narrow_tall.iter().any(|s| s == ".base"));
        assert!(narrow_tall.iter().any(|s| s == ".narrow"));
        assert!(narrow_tall.iter().any(|s| s == ".tall"));
        assert!(narrow_tall.iter().any(|s| s == ".portrait"));

        let wide_short = selectors((1280.0, 720.0));
        assert!(wide_short.iter().any(|s| s == ".base"));
        assert!(!wide_short.iter().any(|s| s == ".narrow"));
        assert!(!wide_short.iter().any(|s| s == ".tall"));
        assert!(!wide_short.iter().any(|s| s == ".portrait"));
    }

    #[test]
    fn nested_media_rules_use_the_live_viewport() {
        let css = ".card { display:block; @media (max-width: 950px) { width:100%; } }";
        let narrow = parse_stylesheet_for_viewport(css, (900.0, 1000.0));
        let wide = parse_stylesheet_for_viewport(css, (1280.0, 720.0));
        assert!(
            narrow
                .iter()
                .any(|(selector, declarations)| selector == ".card"
                    && declarations.contains("width:100%"))
        );
        assert!(
            !wide
                .iter()
                .any(|(_, declarations)| declarations.contains("width:100%"))
        );
    }

    #[test]
    fn tailwind_container_query_is_retained_but_simple_parser_omits_it() {
        let css = r#"
            .base { width: 10px }
            @container (min-width: 28rem) {
                .\@md\:flex-row { flex-direction: row }
            }
            @container main not (max-inline-size: 60em) {
                .named { width: 20px }
            }
        "#;
        let mut conditions = condition_arena_root();
        let parsed = parse_stylesheet_for_viewport_preserving_containers(
            css, (1280.0, 720.0), &mut conditions, ContainerConditionId::NONE,
        );
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[1].container_condition_id, ContainerConditionId(1));
        assert_eq!(parsed[2].container_condition_id, ContainerConditionId(2));
        assert_eq!(conditions.len(), 3);
        assert_eq!(
            conditions[1].alternatives[0].condition,
            Some(ContainerQueryExpr::Feature(ContainerSizeFeature {
                axis: ContainerQueryAxis::Width,
                comparison: ContainerQueryComparison::Min,
                length: ContainerQueryLength::Rem(28.0),
            }))
        );
        assert_eq!(conditions[2].alternatives[0].name.as_deref(), Some("main"));
        assert!(matches!(
            conditions[2].alternatives[0].condition,
            Some(ContainerQueryExpr::Not(_))
        ));
        assert_eq!(parse_stylesheet(css).len(), 1);
    }

    #[test]
    fn container_at_rule_keyword_is_ascii_insensitive_and_exact() {
        assert_eq!(
            at_rule_prelude("CoNtAiNeR (min-width:1px)", "container"),
            Some("(min-width:1px)")
        );
        assert_eq!(
            at_rule_prelude("container/**/(min-width:1px)", "container"),
            Some("(min-width:1px)")
        );
        assert!(at_rule_prelude("containerfoo (min-width:1px)", "container").is_none());
        assert!(at_rule_prelude("container-type (min-width:1px)", "container").is_none());

        let css = r#"
            @CONTAINER (min-width:1px) {
                .top-level { width:1px }
            }
            .host {
                @CoNtAiNeR (min-width:2px) { width:2px }
                @containerfoo (min-width:3px) { height:3px }
            }
            .comment-host {
                @CONTAINER/**/(min-width:5px) { height:5px }
            }
            @containerfoo (min-width:4px) {
                .unknown-prefix { width:4px }
            }
        "#;
        let mut conditions = condition_arena_root();
        let parsed = parse_stylesheet_for_viewport_preserving_containers(
            css,
            (1280.0, 720.0),
            &mut conditions,
            ContainerConditionId::NONE,
        );
        assert_eq!(conditions.len(), 4);
        assert_eq!(parsed.len(), 3, "unknown prefix at-rules must be dropped");
        assert!(parsed.iter().any(|rule| rule.selector == ".top-level"));
        assert!(parsed
            .iter()
            .any(|rule| { rule.selector == ".host" && rule.declarations.contains("width:2px") }));
        assert!(parsed.iter().any(|rule| {
            rule.selector == ".comment-host" && rule.declarations.contains("height:5px")
        }));
        assert!(!parsed.iter().any(|rule| {
            rule.selector == ".unknown-prefix" || rule.declarations.contains("height:3px")
        }));
    }

    #[test]
    fn global_at_rules_inside_container_are_not_conditioned() {
        let css = r#"
            @container (min-width:10000px) {
                @property --cq-token {
                    syntax: "<length>";
                    inherits: false;
                    initial-value: 17px;
                }
                .conditional { width:999px }
            }
        "#;
        let mut conditions = condition_arena_root();
        let parsed = parse_stylesheet_for_viewport_preserving_containers(
            css,
            (1280.0, 720.0),
            &mut conditions,
            ContainerConditionId::NONE,
        );
        assert_eq!(conditions.len(), 2);
        let registration = parsed
            .iter()
            .find(|rule| {
                rule.selector
                    == format!("{PROPERTY_REGISTRATION_SELECTOR_PREFIX}--cq-token")
            })
            .expect("@property initial value should be registered");
        assert_eq!(
            registration.container_condition_id,
            ContainerConditionId::NONE
        );
        assert!(registration.declarations.contains("initial-value: 17px"));
        let conditional = parsed
            .iter()
            .find(|rule| rule.selector == ".conditional")
            .expect("ordinary conditional rule should remain indexed");
        assert_eq!(conditional.container_condition_id, ContainerConditionId(1));
        assert!(parse_stylesheet(css).is_empty());
    }

    fn apply_registered_property_test_style(
        sheet: &Stylesheet,
        tree: &DomTree,
        target: NodeId,
        parent_props: &HashMap<String, String>,
    ) -> (LayoutStyle, HashMap<String, String>) {
        let node = tree.get_node(target).unwrap();
        let element = node.as_element().unwrap();
        let classes = node
            .get_attribute("class")
            .map(|value| value.split_whitespace().map(str::to_string).collect::<Vec<_>>())
            .unwrap_or_default();
        let mut matcher = tree.matcher();
        let mut style = LayoutStyle::default();
        let effective = sheet.apply(
            tree,
            &mut matcher,
            target,
            node.get_attribute("id"),
            &classes,
            element.local.as_ref(),
            &mut style,
            parent_props,
            None,
        );
        (style, effective.unwrap_or_else(|| parent_props.clone()))
    }

    #[test]
    fn registered_custom_properties_obey_inherits_descriptors() {
        let tree = obscura_dom::parse_html(
            r#"<div id="parent"><div id="child"></div></div><div id="initial"></div>"#,
        );
        let css = r#"
            @property --private {
                syntax:"<percentage>";
                inherits:false;
                initial-value:75%;
            }
            @property --shared {
                syntax:"<percentage>";
                inherits:true;
                initial-value:25%;
            }
            #parent { --private:20%; --shared:30% }
            #child, #initial {
                width:var(--private);
                height:var(--shared);
            }
        "#;
        let sheet = Stylesheet::parse(&tree, &[css.to_string()]);
        let parent = tree.get_element_by_id("parent").unwrap();
        let child = tree.get_element_by_id("child").unwrap();
        let initial = tree.get_element_by_id("initial").unwrap();
        let (_, parent_props) =
            apply_registered_property_test_style(&sheet, &tree, parent, &HashMap::new());
        let (child_style, _) =
            apply_registered_property_test_style(&sheet, &tree, child, &parent_props);
        assert_eq!(child_style.width, crate::Dimension::Percent(0.75));
        assert_eq!(child_style.height, crate::Dimension::Percent(0.30));

        let (initial_style, _) =
            apply_registered_property_test_style(&sheet, &tree, initial, &HashMap::new());
        assert_eq!(initial_style.width, crate::Dimension::Percent(0.75));
        assert_eq!(initial_style.height, crate::Dimension::Percent(0.25));
    }

    #[test]
    fn registered_custom_property_overrides_and_var_fallbacks_stay_distinct() {
        let tree = obscura_dom::parse_html(
            r#"<div id="override"></div><div id="reset"></div><div id="invalid"></div>"#,
        );
        let css = r#"
            @property --registered {
                syntax:"<percentage>";
                inherits:false;
                initial-value:75%;
            }
            #override {
                --registered:60%;
                width:var(--registered, 10%);
                height:var(--missing, 12%);
            }
            #reset {
                --registered:initial;
                width:var(--registered, 10%);
                height:var(--missing, 12%);
            }
            #invalid {
                --registered:red;
                width:var(--registered, 10%);
            }
        "#;
        let sheet = Stylesheet::parse(&tree, &[css.to_string()]);
        let override_id = tree.get_element_by_id("override").unwrap();
        let reset_id = tree.get_element_by_id("reset").unwrap();
        let invalid_id = tree.get_element_by_id("invalid").unwrap();
        let (overridden, _) = apply_registered_property_test_style(
            &sheet,
            &tree,
            override_id,
            &HashMap::new(),
        );
        assert_eq!(overridden.width, crate::Dimension::Percent(0.60));
        assert_eq!(overridden.height, crate::Dimension::Percent(0.12));
        let (reset, _) =
            apply_registered_property_test_style(&sheet, &tree, reset_id, &HashMap::new());
        assert_eq!(reset.width, crate::Dimension::Percent(0.75));
        assert_eq!(reset.height, crate::Dimension::Percent(0.12));
        let (invalid, invalid_props) =
            apply_registered_property_test_style(&sheet, &tree, invalid_id, &HashMap::new());
        assert_eq!(invalid.width, crate::Dimension::Percent(0.75));
        assert_eq!(
            invalid_props.get("--registered").map(String::as_str),
            Some("75%"),
            "an invalid typed value computes to the registered initial value"
        );
    }

    #[test]
    fn wildcard_duplicate_and_invalid_property_registrations_are_bounded() {
        let tree = obscura_dom::parse_html(
            r#"<div id="wild"></div><div id="wild-fallback"></div><div id="typed"></div>"#,
        );
        let css = r#"
            @property --wild { syntax:"*"; inherits:false }
            @property --typed {
                syntax:"<number>";
                inherits:false;
                initial-value:2;
            }
            @property --typed {
                syntax:"<number>";
                initial-value:9;
            }
            @property --unsupported {
                syntax:"<angle>";
                inherits:false;
                initial-value:30deg;
            }
            @property --last {
                syntax:"<number>";
                inherits:false;
                initial-value:1;
            }
            @property --last {
                syntax:"<number>";
                inherits:false;
                initial-value:3;
            }
            #wild { --wild:20px; width:var(--wild, 7px) }
            #wild-fallback { width:var(--wild, 7px) }
            #typed {
                opacity:var(--typed, .1);
                height:var(--unsupported, 11px);
            }
        "#;
        let sheet = Stylesheet::parse(&tree, &[css.to_string()]);
        let wild = tree.get_element_by_id("wild").unwrap();
        let wild_fallback = tree.get_element_by_id("wild-fallback").unwrap();
        let typed = tree.get_element_by_id("typed").unwrap();
        let (wild_style, _) =
            apply_registered_property_test_style(&sheet, &tree, wild, &HashMap::new());
        assert_eq!(wild_style.width, crate::Dimension::Px(20.0));
        let (fallback_style, _) =
            apply_registered_property_test_style(&sheet, &tree, wild_fallback, &HashMap::new());
        assert_eq!(fallback_style.width, crate::Dimension::Px(7.0));
        let (typed_style, typed_props) =
            apply_registered_property_test_style(&sheet, &tree, typed, &HashMap::new());
        assert_eq!(typed_style.opacity, Some(2.0));
        assert_eq!(typed_style.height, crate::Dimension::Px(11.0));
        assert_eq!(
            typed_props.get("--typed").map(String::as_str),
            Some("2"),
            "a later invalid duplicate registration must not replace the valid one"
        );
        assert_eq!(
            typed_props.get("--last").map(String::as_str),
            Some("3"),
            "the last valid registration wins"
        );
        assert!(!typed_props.contains_key("--unsupported"));
    }

    #[test]
    fn registered_percentage_initial_value_keeps_radial_gradient_valid() {
        let tree = obscura_dom::parse_html(r#"<div id="pulse"></div>"#);
        let css = r#"
            @property --pulse-outer {
                syntax:"<percentage>";
                inherits:false;
                initial-value:75%;
            }
            #pulse {
                background-image:radial-gradient(
                    circle at 50% 50%,
                    transparent var(--pulse-outer),
                    rgb(255 255 255) 100%
                );
            }
        "#;
        let sheet = Stylesheet::parse(&tree, &[css.to_string()]);
        let pulse = tree.get_element_by_id("pulse").unwrap();
        let (style, props) =
            apply_registered_property_test_style(&sheet, &tree, pulse, &HashMap::new());
        assert_eq!(props.get("--pulse-outer").map(String::as_str), Some("75%"));
        let (_, stops) = style
            .background_radial_gradient
            .expect("registered initial value should preserve the gradient");
        assert_eq!(stops[0].1, Some(0.75));
        assert_eq!(stops[1].1, Some(1.0));
    }

    #[test]
    fn container_boolean_grammar_rejects_mixed_operators() {
        for invalid in [
            "(min-width:1px) and (max-width:2px) or (min-inline-size:3px)",
            "not (min-width:1px) and (max-width:2px)",
            "(min-width:1px) or not (max-width:2px)",
        ] {
            assert!(
                parse_container_query_expr(invalid).is_none(),
                "invalid boolean grammar was accepted: {invalid}"
            );
        }
        assert!(matches!(
            parse_container_query_expr(
                "(min-width:1px) and ((max-width:2px) or (min-inline-size:3px))"
            ),
            Some(ContainerQueryExpr::And(_))
        ));
        assert!(matches!(
            parse_container_query_expr("not ((min-width:1px) and (max-inline-size:2px))"),
            Some(ContainerQueryExpr::Not(_))
        ));
    }

    #[test]
    fn container_custom_ident_rejects_css_wide_and_default() {
        for reserved in [
            "none",
            "not",
            "and",
            "or",
            "default",
            "initial",
            "inherit",
            "unset",
            "revert",
            "revert-layer",
        ] {
            assert!(
                parse_container_query_name(reserved).is_none(),
                "reserved custom-ident was accepted: {reserved}"
            );
            if reserved != "not" {
                assert!(
                    parse_container_query(&format!("{reserved} (min-width:1px)")).is_none(),
                    "reserved query name was accepted: {reserved}"
                );
            }
        }
        assert!(
            matches!(
                parse_container_query("not (min-width:1px)"),
                Some(ContainerQuery {
                    name: None,
                    condition: Some(ContainerQueryExpr::Not(_)),
                })
            ),
            "`not` is reserved as a name but valid as the unary query operator"
        );
        for valid in ["auto", "normal", "container", "--card", "main"] {
            assert_eq!(
                parse_container_query_name(valid).as_deref(),
                Some(valid),
                "valid query name was rejected: {valid}"
            );
        }
    }

    #[test]
    fn unknown_comma_arm_does_not_drop_supported_arm() {
        let queries = parse_container_query_list("(future(foo)), main (min-width:1px)")
            .expect("general-enclosed arm is valid unknown syntax");
        assert_eq!(queries.len(), 2);
        assert_eq!(queries[0].condition, Some(ContainerQueryExpr::Unknown));
        assert_eq!(queries[1].name.as_deref(), Some("main"));
        assert!(matches!(
            queries[1].condition,
            Some(ContainerQueryExpr::Feature(_))
        ));

        let mut conditions = condition_arena_root();
        let parsed = parse_stylesheet_for_viewport_preserving_containers(
            "@container (future(foo)), main (min-width:1px) {.card{display:grid}}",
            (1280.0, 720.0),
            &mut conditions,
            ContainerConditionId::NONE,
        );
        assert_eq!(parsed.len(), 1);
        assert_eq!(conditions[1].alternatives.len(), 2);
    }

    #[test]
    fn container_query_recursion_depth_is_bounded() {
        let nested = format!(
            "{}min-width:1px{}",
            "(".repeat(MAX_CONTAINER_QUERY_DEPTH + 16),
            ")".repeat(MAX_CONTAINER_QUERY_DEPTH + 16)
        );
        assert!(parse_container_query_expr(&nested).is_none());
    }

    #[test]
    fn supports_accepts_container_css_wide_values() {
        for property in ["container", "container-name", "container-type"] {
            for keyword in ["initial", "inherit", "unset", "revert", "revert-layer"] {
                assert!(
                    supports_condition_applies(&format!("({property}:{keyword})")),
                    "{property}:{keyword} is a valid whole-value CSS-wide declaration"
                );
            }
        }
    }

    fn evaluate_container_styles(
        tree: &DomTree,
        sheet: &Stylesheet,
        target: NodeId,
        snapshot: &ContainerSnapshot,
    ) -> (LayoutStyle, ContainerDecisionSignature, ContainerQueryStats) {
        let node = tree.get_node(target).unwrap();
        let element = node.as_element().unwrap();
        let id = node.get_attribute("id");
        let classes: Vec<String> = node
            .get_attribute("class")
            .map(|value| value.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default();
        let mut matcher = tree.matcher();
        let mut evaluator = ContainerQueryEvaluator::new(tree, snapshot);
        let mut style = LayoutStyle::default();
        sheet.apply_with_container_queries(
            tree,
            &mut matcher,
            target,
            id,
            &classes,
            element.local.as_ref(),
            &mut style,
            &HashMap::new(),
            None,
            &mut evaluator,
        );
        let (signature, stats) = evaluator.finish();
        (style, signature, stats)
    }

    fn container_box(
        container_type: crate::ContainerType,
        names: &[&str],
        content_width: f32,
        font_size: f32,
    ) -> ContainerBox {
        ContainerBox {
            container_type,
            available_type: container_type,
            names: names.iter().map(|name| (*name).to_string()).collect(),
            content_width,
            content_height: 100.0,
            font_size,
        }
    }

    #[test]
    fn container_evaluator_honors_tailwind_threshold_and_cache() {
        let tree = obscura_dom::parse_html(
            r#"<div id="container"><div id="target"></div></div>"#,
        );
        let container = tree.get_element_by_id("container").unwrap();
        let target = tree.get_element_by_id("target").unwrap();
        let sheet = Stylesheet::parse(
            &tree,
            &[r#"
                #target { width:1px; height:1px }
                @container (min-width:28rem) {
                    #target { width:2px }
                    #target { height:2px }
                }
            "#
            .to_string()],
        );

        let snapshot = |width| {
            let mut snapshot = ContainerSnapshot {
                root_font_size: 16.0,
                ..Default::default()
            };
            snapshot.boxes.insert(
                container,
                container_box(crate::ContainerType::InlineSize, &[], width, 16.0),
            );
            snapshot
        };
        let (below, _, _) =
            evaluate_container_styles(&tree, &sheet, target, &snapshot(447.0));
        assert_eq!(below.width, crate::Dimension::Px(1.0));
        assert_eq!(below.height, crate::Dimension::Px(1.0));

        let (at, _, stats) =
            evaluate_container_styles(&tree, &sheet, target, &snapshot(448.0));
        assert_eq!(at.width, crate::Dimension::Px(2.0));
        assert_eq!(at.height, crate::Dimension::Px(2.0));
        assert_eq!(stats.evaluations, 1);
        assert!(stats.cache_hits >= 1);
    }

    #[test]
    fn container_evaluator_matches_selector_before_ancestor_lookup() {
        let tree = obscura_dom::parse_html(
            r#"<div id="container"><div id="target"></div></div>"#,
        );
        let container = tree.get_element_by_id("container").unwrap();
        let target = tree.get_element_by_id("target").unwrap();
        let sheet = Stylesheet::parse(
            &tree,
            &[r#"
                @container (min-width:1px) {
                    div[data-never-present] { width:999px }
                }
            "#
            .to_string()],
        );
        let mut snapshot = ContainerSnapshot {
            root_font_size: 16.0,
            ..Default::default()
        };
        snapshot.boxes.insert(
            container,
            container_box(crate::ContainerType::InlineSize, &[], 500.0, 16.0),
        );
        let (_, signature, stats) =
            evaluate_container_styles(&tree, &sheet, target, &snapshot);
        assert_eq!(stats.evaluations, 0);
        assert_eq!(stats.ancestor_steps, 0);
        assert!(signature.decisions.is_empty());
    }

    #[test]
    fn container_evaluator_selects_nearest_eligible_named_ancestor() {
        let tree = obscura_dom::parse_html(
            r#"<div id="outer"><div id="inner"><div id="target"></div></div></div>"#,
        );
        let outer = tree.get_element_by_id("outer").unwrap();
        let inner = tree.get_element_by_id("inner").unwrap();
        let target = tree.get_element_by_id("target").unwrap();
        let sheet = Stylesheet::parse(
            &tree,
            &[r#"
                #target { width:1px; height:1px }
                @container shell (min-width:500px) { #target { width:11px } }
                @container (min-width:500px) { #target { height:22px } }
            "#
            .to_string()],
        );
        let mut snapshot = ContainerSnapshot {
            root_font_size: 16.0,
            ..Default::default()
        };
        snapshot.boxes.insert(
            outer,
            container_box(
                crate::ContainerType::InlineSize,
                &["shell"],
                600.0,
                16.0,
            ),
        );
        snapshot.boxes.insert(
            inner,
            container_box(
                crate::ContainerType::InlineSize,
                &["other"],
                300.0,
                16.0,
            ),
        );
        let (style, _, stats) =
            evaluate_container_styles(&tree, &sheet, target, &snapshot);
        assert_eq!(style.width, crate::Dimension::Px(11.0));
        assert_eq!(style.height, crate::Dimension::Px(1.0));
        assert!(stats.ancestor_steps >= 3);
    }

    #[test]
    fn container_query_em_uses_container_font_and_rem_uses_root_font() {
        let tree = obscura_dom::parse_html(
            r#"<div id="container"><div id="target"></div></div>"#,
        );
        let container = tree.get_element_by_id("container").unwrap();
        let target = tree.get_element_by_id("target").unwrap();
        let sheet = Stylesheet::parse(
            &tree,
            &[r#"
                #target { width:1px; height:1px }
                @container (min-width:30em) { #target { width:3px } }
                @container (min-width:30rem) { #target { height:3px } }
            "#
            .to_string()],
        );
        let mut snapshot = ContainerSnapshot {
            root_font_size: 20.0,
            ..Default::default()
        };
        snapshot.boxes.insert(
            container,
            container_box(crate::ContainerType::InlineSize, &[], 400.0, 10.0),
        );
        let (style, _, _) =
            evaluate_container_styles(&tree, &sheet, target, &snapshot);
        assert_eq!(style.width, crate::Dimension::Px(3.0));
        assert_eq!(style.height, crate::Dimension::Px(1.0));
    }

    #[test]
    fn container_snapshot_compares_only_axes_exposed_by_container_type() {
        let mut inline_a =
            container_box(crate::ContainerType::InlineSize, &[], 400.0, 16.0);
        let mut inline_b = inline_a.clone();
        inline_a.content_height = 100.0;
        inline_b.content_height = 900.0;
        assert_eq!(inline_a, inline_b);

        let mut size_a = container_box(crate::ContainerType::Size, &[], 400.0, 16.0);
        let mut size_b = size_a.clone();
        size_a.content_height = 100.0;
        size_b.content_height = 900.0;
        assert_ne!(size_a, size_b);
    }

    #[test]
    fn nested_container_conditions_select_independent_containers() {
        let tree = obscura_dom::parse_html(
            r#"<div id="outer"><div id="inner"><div id="target"></div></div></div>"#,
        );
        let outer = tree.get_element_by_id("outer").unwrap();
        let inner = tree.get_element_by_id("inner").unwrap();
        let target = tree.get_element_by_id("target").unwrap();
        let sheet = Stylesheet::parse(
            &tree,
            &[r#"
                #target { width:1px }
                @container outer (min-width:500px) {
                    @container inner (min-width:200px) {
                        #target { width:9px }
                    }
                }
            "#
            .to_string()],
        );
        let snapshot = |inner_width| {
            let mut snapshot = ContainerSnapshot {
                root_font_size: 16.0,
                ..Default::default()
            };
            snapshot.boxes.insert(
                outer,
                container_box(
                    crate::ContainerType::InlineSize,
                    &["outer"],
                    600.0,
                    16.0,
                ),
            );
            snapshot.boxes.insert(
                inner,
                container_box(
                    crate::ContainerType::InlineSize,
                    &["inner"],
                    inner_width,
                    16.0,
                ),
            );
            snapshot
        };
        let (matching, _, _) =
            evaluate_container_styles(&tree, &sheet, target, &snapshot(200.0));
        assert_eq!(matching.width, crate::Dimension::Px(9.0));
        let (failing, _, _) =
            evaluate_container_styles(&tree, &sheet, target, &snapshot(199.0));
        assert_eq!(failing.width, crate::Dimension::Px(1.0));
    }

    #[test]
    fn unknown_container_alternative_does_not_mask_true_alternative() {
        let tree = obscura_dom::parse_html(
            r#"<div id="container"><div id="target"></div></div>"#,
        );
        let container = tree.get_element_by_id("container").unwrap();
        let target = tree.get_element_by_id("target").unwrap();
        let sheet = Stylesheet::parse(
            &tree,
            &[r#"
                #target { width:1px }
                @container (future(foo)), (min-width:100px) {
                    #target { width:7px }
                }
            "#
            .to_string()],
        );
        let mut snapshot = ContainerSnapshot {
            root_font_size: 16.0,
            ..Default::default()
        };
        snapshot.boxes.insert(
            container,
            container_box(crate::ContainerType::InlineSize, &[], 100.0, 16.0),
        );
        let (style, _, _) =
            evaluate_container_styles(&tree, &sheet, target, &snapshot);
        assert_eq!(style.width, crate::Dimension::Px(7.0));
    }

    #[test]
    fn container_query_boolean_evaluation_uses_kleene_truth_tables() {
        let container =
            container_box(crate::ContainerType::InlineSize, &[], 200.0, 16.0);
        let min_width = |threshold| {
            ContainerQueryExpr::Feature(ContainerSizeFeature {
                axis: ContainerQueryAxis::Width,
                comparison: ContainerQueryComparison::Min,
                length: ContainerQueryLength::Px(threshold),
            })
        };
        assert_eq!(
            evaluate_container_query_expr(
                &ContainerQueryExpr::Or(vec![
                    min_width(100.0),
                    ContainerQueryExpr::Unknown,
                ]),
                &container,
                16.0,
            ),
            ContainerQueryTruth::True
        );
        assert_eq!(
            evaluate_container_query_expr(
                &ContainerQueryExpr::And(vec![
                    min_width(300.0),
                    ContainerQueryExpr::Unknown,
                ]),
                &container,
                16.0,
            ),
            ContainerQueryTruth::False
        );
        assert_eq!(
            evaluate_container_query_expr(
                &ContainerQueryExpr::Or(vec![
                    min_width(300.0),
                    ContainerQueryExpr::Unknown,
                ]),
                &container,
                16.0,
            ),
            ContainerQueryTruth::Unknown
        );
        assert_eq!(
            evaluate_container_query_expr(
                &ContainerQueryExpr::Not(Box::new(ContainerQueryExpr::Unknown)),
                &container,
                16.0,
            ),
            ContainerQueryTruth::Unknown
        );
    }

    #[test]
    fn container_range_syntax_supports_strict_inclusive_and_chained_queries() {
        let container =
            container_box(crate::ContainerType::Size, &[], 200.0, 16.0);
        for query in [
            "(width)",
            "(width > 199px)",
            "(width>=200px)",
            "(199px < width)",
            "(199px < width <= 200px)",
            "(height = 100px)",
            "(block-size >= 100px)",
        ] {
            let expression =
                parse_container_query_expr(query).expect("valid range query");
            assert_eq!(
                evaluate_container_query_expr(&expression, &container, 16.0),
                ContainerQueryTruth::True,
                "{query}"
            );
        }
        for query in [
            "(width > 200px)",
            "(width < 200px)",
            "(200px < width < 300px)",
            "(height > 100px)",
        ] {
            let expression =
                parse_container_query_expr(query).expect("valid range query");
            assert_eq!(
                evaluate_container_query_expr(&expression, &container, 16.0),
                ContainerQueryTruth::False,
                "{query}"
            );
        }
        for invalid in [
            "(100px < width > 200px)",
            "(200px > width < 100px)",
            "(100px = width = 100px)",
            "(100px < width = 200px)",
        ] {
            assert!(
                parse_container_query_expr(invalid).is_none(),
                "invalid mixed/equality chain parsed: {invalid}"
            );
        }
    }

    #[test]
    fn block_axis_query_requires_size_container() {
        let tree = obscura_dom::parse_html(
            r#"<div id="container"><div id="target"></div></div>"#,
        );
        let container = tree.get_element_by_id("container").unwrap();
        let target = tree.get_element_by_id("target").unwrap();
        let sheet = Stylesheet::parse(
            &tree,
            &[r#"
                #target { width:1px }
                @container (height >= 100px) { #target { width:8px } }
            "#
            .to_string()],
        );
        let snapshot = |container_type| {
            let mut snapshot = ContainerSnapshot {
                root_font_size: 16.0,
                ..Default::default()
            };
            snapshot.boxes.insert(
                container,
                container_box(container_type, &[], 300.0, 16.0),
            );
            snapshot
        };
        let (inline_only, _, _) = evaluate_container_styles(
            &tree,
            &sheet,
            target,
            &snapshot(crate::ContainerType::InlineSize),
        );
        assert_eq!(inline_only.width, crate::Dimension::Px(1.0));
        let (size, _, _) = evaluate_container_styles(
            &tree,
            &sheet,
            target,
            &snapshot(crate::ContainerType::Size),
        );
        assert_eq!(size.width, crate::Dimension::Px(8.0));
    }

    #[test]
    fn nested_container_rules_form_a_parent_condition_chain() {
        let css = "@container shell (min-width:40rem){\
            @container (max-inline-size:50rem){.card{display:grid}}}";
        let mut conditions = condition_arena_root();
        let parsed = parse_stylesheet_for_viewport_preserving_containers(
            css, (1280.0, 720.0), &mut conditions, ContainerConditionId::NONE,
        );
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].container_condition_id, ContainerConditionId(2));
        assert_eq!(conditions[2].parent, ContainerConditionId(1));
        assert_eq!(conditions[1].parent, ContainerConditionId::NONE);
    }

    #[test]
    fn unresolved_container_rules_do_not_enter_cascade_or_pseudos() {
        let tree = obscura_dom::parse_html(r#"<div id="target"></div>"#);
        let target = tree.query_selector("#target").unwrap().unwrap();
        let sheet = Stylesheet::parse(&tree, &[r#"
            #target{width:10px}
            @container (min-width:28rem){
                #target{width:999px}
                #target::before{content:"inactive"}
            }
            #target{height:20px}
        "#.to_string()]);
        assert_eq!(sheet.rules.len(), 3, "conditional rule remains indexed");
        assert_eq!(sheet.container_conditions.len(), 2);
        let mut matcher = tree.matcher();
        let mut style = LayoutStyle::default();
        sheet.apply(
            &tree, &mut matcher, target, Some("target"), &[], "div", &mut style,
            &HashMap::new(), None,
        );
        assert_eq!(style.width, crate::Dimension::Px(10.0));
        assert_eq!(style.height, crate::Dimension::Px(20.0));
        let (before, after) =
            sheet.pseudo_styles(&tree, &mut matcher, target, &HashMap::new(), &style);
        assert!(before.is_none() && after.is_none());
    }

    #[test]
    fn no_container_parser_output_and_order_are_unchanged() {
        let css = ".card{width:10px}\
            @supports (display:grid){.card{display:grid}}\
            @media (min-width:64rem){.card{width:20px}}\
            .card{height:30px}";
        assert_eq!(
            parse_stylesheet_for_viewport(css, (1280.0, 720.0)),
            vec![
                (".card".into(), "width:10px;".into()),
                (".card".into(), "display:grid;".into()),
                (".card".into(), "width:20px;".into()),
                (".card".into(), "height:30px;".into()),
            ]
        );
        let mut conditions = condition_arena_root();
        let rich = parse_stylesheet_for_viewport_preserving_containers(
            css, (1280.0, 720.0), &mut conditions, ContainerConditionId::NONE,
        );
        assert_eq!(conditions.len(), 1);
        assert!(rich.iter().all(|rule| rule.container_condition_id == ContainerConditionId::NONE));
    }

    #[test]
    fn supports_conditions_gate_legacy_framework_fallbacks() {
        let legacy_probe = "(((-webkit-hyphens:none)) and \
            (not (margin-trim:inline))) or \
            ((-moz-orient:inline) and \
            (not (color:rgb(from red r g b))))";
        assert!(!supports_condition_applies(legacy_probe));
        assert!(supports_condition_applies(
            "(display:grid) and (selector(.card > *))"
        ));
        assert!(supports_condition_applies(
            "not (unknown-engine-prop:value)"
        ));

        let css = format!(
            "@supports {legacy_probe} {{ .legacy {{ line-height:1.5 }} }}\
             @supports (display:grid) {{ .modern {{ display:grid }} }}\
             .host {{ @supports {legacy_probe} {{ width:999px }} }}"
        );
        let rules = parse_stylesheet_for_viewport(&css, (1280.0, 720.0));
        assert!(
            !rules.iter().any(|(selector, declarations)| {
                selector == ".legacy" || declarations.contains("999px")
            }),
            "false supports branches must be skipped: {rules:?}"
        );
        assert!(
            rules.iter().any(|(selector, declarations)| {
                selector == ".modern" && declarations.contains("display:grid")
            }),
            "true supports branch should remain: {rules:?}"
        );
    }

    #[test]
    fn media_breakpoints_support_font_relative_lengths_and_ranges() {
        assert!(!media_query_applies_for_viewport(
            "@media (min-width: 64rem)",
            (900.0, 1000.0)
        ));
        assert!(media_query_applies_for_viewport(
            "@media (min-width: 64rem)",
            (1024.0, 768.0)
        ));
        assert!(media_query_applies_for_viewport(
            "@media (56.25rem <= width)",
            (900.0, 1000.0)
        ));
        assert!(!media_query_applies_for_viewport(
            "@media (width > calc(60em - 1px))",
            (900.0, 1000.0)
        ));
    }

    #[test]
    fn negated_min_width_queries_form_max_breakpoints() {
        assert!(media_query_applies_for_viewport(
            "@media not all and (min-width: 40rem)",
            (639.0, 900.0)
        ));
        assert!(!media_query_applies_for_viewport(
            "@media not all and (min-width: 40rem)",
            (1280.0, 900.0)
        ));

        let css = r#"
            .hidden { display: none }
            @media not all and (min-width: 40rem) {
                .max-sm\:inline { display: inline }
            }
            @media (min-width: 80rem) {
                .xl\:inline { display: inline }
            }
        "#;
        let desktop = parse_stylesheet_for_viewport(css, (1280.0, 900.0));
        assert!(!desktop
            .iter()
            .any(|(selector, _)| selector == r".max-sm\:inline"));
        assert!(desktop
            .iter()
            .any(|(selector, _)| selector == r".xl\:inline"));
    }

    #[test]
    fn media_query_lists_are_or_conditions() {
        assert!(media_query_applies_for_viewport(
            "@media print, (min-width: 64rem)",
            (1280.0, 720.0)
        ));
        assert!(!media_query_applies_for_viewport(
            "@media print, (min-width: 64rem)",
            (900.0, 1000.0)
        ));
    }

    #[test]
    fn rem_breakpoint_does_not_reveal_desktop_menu_on_narrow_viewport() {
        let css = r#"
            header .menu-toolkit { display: none }
            @media (min-width: 64rem) {
                header .menu-toolkit { display: flex }
            }
        "#;
        let narrow = parse_stylesheet_for_viewport(css, (900.0, 1000.0));
        assert!(narrow.iter().any(|(selector, declarations)| {
            selector == "header .menu-toolkit" && declarations.contains("display: none")
        }));
        assert!(!narrow.iter().any(|(_, declarations)| declarations.contains("display: flex")));

        let wide = parse_stylesheet_for_viewport(css, (1024.0, 768.0));
        assert!(wide.iter().any(|(_, declarations)| declarations.contains("display: flex")));
    }
}

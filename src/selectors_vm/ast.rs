use super::parser::{Selector, SelectorImplDescriptor};
use crate::selectors_vm::{DenseHashSet, MatchId};
use selectors::attr::{AttrSelectorOperator, ParsedAttrSelectorOperation, ParsedCaseSensitivity};
use selectors::parser::{Combinator, Component, NthType};
use std::fmt::{self, Debug, Formatter};
use std::mem;

#[derive(PartialEq, Eq, Debug, Copy, Clone)]
pub(crate) struct NthChild {
    step: i32,
    offset: i32,
}

impl NthChild {
    #[inline]
    #[must_use]
    pub const fn new(step: i32, offset: i32) -> Self {
        Self { step, offset }
    }

    #[must_use]
    pub const fn has_index(self, index: i32) -> bool {
        let Self { offset, step } = self;
        // wrap to prevent panic/abort. we won't wrap around anyway, even with a
        // max offset value (i32::MAX) since index is always more than 0
        let offsetted = index.wrapping_sub(offset);
        if step == 0 {
            offsetted == 0
        } else if (offsetted < 0 && step > 0) || (offsetted > 0 && step < 0) {
            false
        } else {
            // again, wrap the remainder op. overflow only occurs with
            // i32::MIN / -1. while the step can be -1, the offsetted
            // value will never be i32::MIN since this index is always
            // more than 0
            offsetted.wrapping_rem(step) == 0
        }
    }
}

#[derive(PartialEq, Eq, Debug, Clone)]
pub(crate) enum OnTagNameExpr {
    ExplicitAny,
    Unmatchable,
    LocalName(Box<str>),
    NthChild(NthChild),
    NthOfType(NthChild),
}

#[derive(Eq, PartialEq, Clone)]
pub(crate) struct AttributeComparisonExpr {
    pub name: Box<str>,
    pub value: Box<str>,
    pub case_sensitivity: ParsedCaseSensitivity,
    pub operator: AttrSelectorOperator,
}

impl AttributeComparisonExpr {
    #[inline]
    #[must_use]
    pub const fn new(
        name: Box<str>,
        value: Box<str>,
        case_sensitivity: ParsedCaseSensitivity,
        operator: AttrSelectorOperator,
    ) -> Self {
        Self {
            name,
            value,
            case_sensitivity,
            operator,
        }
    }
}

impl Debug for AttributeComparisonExpr {
    #[cold]
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("AttributeExpr")
            .field("name", &self.name)
            .field("value", &self.value)
            .field("case_sensitivity", &self.case_sensitivity)
            .field(
                "operator",
                match self.operator {
                    AttrSelectorOperator::Equal => &"AttrSelectorOperator::Equal",
                    AttrSelectorOperator::Includes => &"AttrSelectorOperator::Includes",
                    AttrSelectorOperator::DashMatch => &"AttrSelectorOperator::DashMatch",
                    AttrSelectorOperator::Prefix => &"AttrSelectorOperator::Prefix",
                    AttrSelectorOperator::Substring => &"AttrSelectorOperator::Substring",
                    AttrSelectorOperator::Suffix => &"AttrSelectorOperator::Suffix",
                },
            )
            .finish()
    }
}

/// An attribute check when attributes are received and parsed.
#[derive(PartialEq, Eq, Debug, Clone)]
pub(crate) enum OnAttributesExpr {
    Id(Box<str>),
    Class(Box<str>),
    AttributeExists(Box<str>),
    AttributeComparisonExpr(AttributeComparisonExpr),
}

#[derive(PartialEq, Eq, Debug)]
/// Conditions executed as part of a predicate, or an "expect" in pseudo instructions.
/// These are executed in order of definition.
enum Condition {
    OnTagName(OnTagNameExpr),
    OnAttributes(OnAttributesExpr),
}

impl From<&Component<SelectorImplDescriptor>> for Condition {
    fn from(component: &Component<SelectorImplDescriptor>) -> Self {
        match component {
            Component::LocalName(n) => {
                Self::OnTagName(OnTagNameExpr::LocalName(n.name.to_boxed_slice()))
            }
            Component::ExplicitUniversalType | Component::ExplicitAnyNamespace => {
                Self::OnTagName(OnTagNameExpr::ExplicitAny)
            }
            Component::ExplicitNoNamespace => Self::OnTagName(OnTagNameExpr::Unmatchable),
            Component::ID(id) => Self::OnAttributes(OnAttributesExpr::Id(id.to_boxed_slice())),
            Component::Class(c) => Self::OnAttributes(OnAttributesExpr::Class(c.to_boxed_slice())),
            Component::AttributeInNoNamespaceExists {
                local_name_lower, ..
            } => Self::OnAttributes(OnAttributesExpr::AttributeExists(
                local_name_lower.to_boxed_slice(),
            )),
            &Component::AttributeInNoNamespace {
                ref local_name,
                ref value,
                operator,
                case_sensitivity,
            } => Self::OnAttributes(OnAttributesExpr::AttributeComparisonExpr(
                AttributeComparisonExpr::new(
                    local_name.to_boxed_slice(),
                    value.to_boxed_slice(),
                    case_sensitivity,
                    operator,
                ),
            )),
            Component::AttributeOther(attr) if attr.namespace.is_none() => {
                Self::OnAttributes(match &attr.operation {
                    ParsedAttrSelectorOperation::Exists => {
                        OnAttributesExpr::AttributeExists(attr.local_name_lower.to_boxed_slice())
                    }
                    ParsedAttrSelectorOperation::WithValue {
                        operator,
                        case_sensitivity,
                        value,
                    } => OnAttributesExpr::AttributeComparisonExpr(AttributeComparisonExpr::new(
                        attr.local_name_lower.to_boxed_slice(),
                        value.to_boxed_slice(),
                        *case_sensitivity,
                        *operator,
                    )),
                })
            }
            Component::Nth(data) if data.ty == NthType::Child => Self::OnTagName(
                OnTagNameExpr::NthChild(NthChild::new(data.an_plus_b.0, data.an_plus_b.1)),
            ),
            Component::Nth(data) if data.ty == NthType::OfType => Self::OnTagName(
                OnTagNameExpr::NthOfType(NthChild::new(data.an_plus_b.0, data.an_plus_b.1)),
            ),
            // NOTE: the rest of the components are explicit namespace or
            // pseudo class-related. Ideally none of them should appear in
            // the parsed selector as we should bail earlier in the parser.
            // Otherwise, we'll have AST in invalid state in case of error.
            bad_selector => {
                debug_assert!(
                    false,
                    "Unsupported selector components should be filtered out by the parser: {bad_selector:?}"
                );
                Self::OnTagName(OnTagNameExpr::Unmatchable)
            }
        }
    }
}

#[derive(PartialEq, Eq, Debug, Clone)]
pub(crate) struct Expr<E>
where
    E: PartialEq + Eq + Debug,
{
    pub simple_expr: E,
    pub negation: bool,
}

impl<E> Expr<E>
where
    E: PartialEq + Eq + Debug,
{
    #[inline]
    const fn new(simple_expr: E, negation: bool) -> Self {
        Self {
            simple_expr,
            negation,
        }
    }
}

#[derive(PartialEq, Eq, Debug, Default, Clone)]
pub(crate) struct Predicate {
    pub on_tag_name_exprs: Vec<Expr<OnTagNameExpr>>,
    pub on_attr_exprs: Vec<Expr<OnAttributesExpr>>,
}

#[inline]
fn add_expr_to_list<E>(list: &mut Vec<Expr<E>>, expr: E, negation: bool)
where
    E: PartialEq + Eq + Debug,
{
    list.push(Expr::new(expr, negation));
}

impl Predicate {
    #[inline]
    fn add_component(&mut self, component: &Component<SelectorImplDescriptor>, negation: bool) {
        match Condition::from(component) {
            Condition::OnTagName(e) => add_expr_to_list(&mut self.on_tag_name_exprs, e, negation),
            Condition::OnAttributes(e) => add_expr_to_list(&mut self.on_attr_exprs, e, negation),
        }
    }

    /// The conjunction of `self` and `other`.
    fn and(mut self, other: &Self) -> Self {
        self.on_tag_name_exprs
            .extend_from_slice(&other.on_tag_name_exprs);
        self.on_attr_exprs.extend_from_slice(&other.on_attr_exprs);
        self
    }
}

/// A disjunction of predicates: an element matches when it matches any one of them.
///
/// A [`Predicate`] is a conjunction, so a single one can't express `:not(a.b)`, which is
/// `:not(a)` OR `:not(b)`, or `:not(:not(a, b))`, which is `a` OR `b`. Every alternative gets
/// an AST node of its own with the same match id, like every selector of a selector list does.
type Alternatives = Vec<Predicate>;

/// How many [`Alternatives`] a selector can expand to before the parser refuses it.
///
/// The expansion multiplies: `:not(a.b):not(c.d)` has 4 alternatives, and one more `:not()`
/// of that shape doubles them again.
pub(crate) const MAX_ALTERNATIVES: usize = 256;

type SelectorComponent = Component<SelectorImplDescriptor>;
type ComplexSelector = selectors::parser::Selector<SelectorImplDescriptor>;

/// What a selector expands to: the [`Alternatives`] for the AST, or only their number for the
/// parser. Both come from the same traversal, so they can't disagree.
trait Expansion: Sized {
    /// A simple selector, or its negation.
    fn of(component: &SelectorComponent, negation: bool) -> Self;
    /// Matches when all the `operands` match.
    fn all_of(operands: impl Iterator<Item = Self>) -> Self;
    /// Matches when any of the `operands` matches.
    fn any_of(operands: impl Iterator<Item = Self>) -> Self;
}

impl Expansion for Alternatives {
    fn of(component: &SelectorComponent, negation: bool) -> Self {
        let mut predicate = Predicate::default();
        predicate.add_component(component, negation);
        vec![predicate]
    }

    fn all_of(operands: impl Iterator<Item = Self>) -> Self {
        operands.fold(vec![Predicate::default()], |matched, operand| {
            if let [only] = operand.as_slice() {
                matched.into_iter().map(|p| p.and(only)).collect()
            } else {
                matched
                    .iter()
                    .flat_map(|p| operand.iter().map(|other| p.clone().and(other)))
                    .collect()
            }
        })
    }

    fn any_of(operands: impl Iterator<Item = Self>) -> Self {
        operands.flatten().collect()
    }
}

impl Expansion for usize {
    fn of(_: &SelectorComponent, _: bool) -> Self {
        1
    }

    fn all_of(operands: impl Iterator<Item = Self>) -> Self {
        operands.fold(1, Self::saturating_mul)
    }

    fn any_of(operands: impl Iterator<Item = Self>) -> Self {
        operands.fold(0, Self::saturating_add)
    }
}

/// Expands a compound selector, or its negation, with De Morgan's laws.
fn expand_compound<'c, X: Expansion>(
    components: impl Iterator<Item = &'c SelectorComponent>,
    negation: bool,
) -> X {
    let operands = components
        // `:not(|p)` has always matched what `:not(p)` matches: the negated no-namespace
        // prefix was one more term of a conjunction, and it is true for every element. As an
        // alternative of its own it would make `:not(|p)` match everything.
        .filter(|component| !(negation && matches!(component, Component::ExplicitNoNamespace)))
        .map(|component| match component {
            Component::Negation(selectors) => expand_negation(selectors.slice(), !negation),
            _ => X::of(component, negation),
        });

    // `!(a && b)` is `!a || !b`
    if negation {
        X::any_of(operands)
    } else {
        X::all_of(operands)
    }
}

/// Expands the selector list of a `:not()`. `negation` is set when the `:not()` applies, and
/// unset when an enclosing `:not()` cancels it.
///
/// The parser refuses combinators inside `:not()`, so every selector of the list is a single
/// compound selector.
fn expand_negation<X: Expansion>(selectors: &[ComplexSelector], negation: bool) -> X {
    let operands = selectors
        .iter()
        .map(|selector| expand_compound(selector.iter(), negation));

    // `!(a || b)` is `!a && !b`
    if negation {
        X::all_of(operands)
    } else {
        X::any_of(operands)
    }
}

/// How many AST paths [`Ast::add_selector`] creates for `selector`: one for every combination
/// of one alternative per compound selector.
pub(crate) fn count_alternatives(selector: &ComplexSelector) -> usize {
    // A combinator counts as 1, so the product over all the components is the product over
    // the compound selectors.
    expand_compound(selector.iter_raw_match_order(), false)
}

#[derive(PartialEq, Eq, Debug)]
pub(crate) struct AstNode {
    pub predicate: Predicate,
    pub children: Vec<Self>,
    pub descendants: Vec<Self>,
    pub match_ids: DenseHashSet,
}

impl AstNode {
    #[inline]
    #[must_use]
    fn new(predicate: Predicate) -> Self {
        Self {
            predicate,
            children: Vec::default(),
            descendants: Vec::default(),
            match_ids: DenseHashSet::new(),
        }
    }
}

// exposed for selectors_ast tool
#[derive(Default, PartialEq, Eq, Debug)]
pub struct Ast {
    pub(crate) root: Vec<AstNode>,
    // NOTE: used to preallocate instruction vector during compilation.
    pub(crate) cumulative_node_count: usize,
}

impl Ast {
    #[inline]
    fn host_expressions(
        predicate: Predicate,
        branches: &mut Vec<AstNode>,
        cumulative_node_count: &mut usize,
    ) -> usize {
        branches
            .iter()
            .enumerate()
            .find(|(_, n)| n.predicate == predicate)
            .map(|(i, _)| i)
            .unwrap_or_else(move || {
                branches.push(AstNode::new(predicate));
                *cumulative_node_count += 1;

                branches.len() - 1
            })
    }

    /// `match_id` is a small integer chosen by the caller. It will be returned back in `MatchInfo`
    pub fn add_selector(&mut self, selector: &Selector, match_id: MatchId) {
        for selector_item in (selector.0).slice() {
            let components: Vec<_> = selector_item.iter_raw_parse_order_from(0).collect();

            // The compound selectors in parse order, each with the combinator that follows it.
            let mut compounds: Vec<_> = components
                .split_inclusive(|component| {
                    matches!(
                        component,
                        Component::Combinator(Combinator::Child | Combinator::Descendant)
                    )
                })
                .map(|compound| match compound.split_last() {
                    Some((&&Component::Combinator(combinator), compound)) => (
                        expand_compound::<Alternatives>(compound.iter().copied(), false),
                        Some(combinator),
                    ),
                    _ => (
                        expand_compound::<Alternatives>(compound.iter().copied(), false),
                        None,
                    ),
                })
                .collect();

            if compounds
                .iter()
                .any(|(alternatives, _)| alternatives.is_empty())
            {
                continue;
            }

            // Every combination of one alternative per compound selector is a path from the
            // root. `path` counts through the combinations, the last compound selector fastest.
            let mut path = vec![0; compounds.len()];

            loop {
                let is_last_path = path
                    .iter()
                    .zip(&compounds)
                    .all(|(&i, (alternatives, _))| i + 1 == alternatives.len());
                let mut branches = &mut self.root;

                for ((alternatives, combinator), &i) in compounds.iter_mut().zip(&path) {
                    let predicate = if is_last_path {
                        mem::take(&mut alternatives[i])
                    } else {
                        alternatives[i].clone()
                    };
                    let node_idx = Self::host_expressions(
                        predicate,
                        branches,
                        &mut self.cumulative_node_count,
                    );
                    let node = &mut branches[node_idx];

                    branches = match combinator {
                        Some(Combinator::Child) => &mut node.children,
                        // `Combinator::Descendant`, the only other one `compounds` splits at
                        Some(_) => &mut node.descendants,
                        None => {
                            node.match_ids.insert(match_id);
                            break;
                        }
                    };
                }

                if is_last_path {
                    break;
                }

                for (i, (alternatives, _)) in path.iter_mut().zip(&compounds).rev() {
                    *i += 1;
                    if *i < alternatives.len() {
                        break;
                    }
                    *i = 0;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selectors_vm::{DenseHashSet, SelectorError};

    #[track_caller]
    fn assert_ast(selectors: &[&str], expected: Ast) {
        let mut ast = Ast::default();

        for (selector, match_id) in selectors.iter().zip(0..) {
            ast.add_selector(&selector.parse().unwrap(), match_id);
        }

        assert_eq!(ast, expected);
    }

    #[track_caller]
    fn assert_err(selector: &str, expected_err: SelectorError) {
        assert_eq!(selector.parse::<Selector>().unwrap_err(), expected_err);
    }

    #[test]
    fn simple_non_attr_expression() {
        for (selector, expected) in [
            (
                "*",
                Expr {
                    simple_expr: OnTagNameExpr::ExplicitAny,
                    negation: false,
                },
            ),
            (
                "div",
                Expr {
                    simple_expr: OnTagNameExpr::LocalName("div".into()),
                    negation: false,
                },
            ),
            (
                ":not(div)",
                Expr {
                    simple_expr: OnTagNameExpr::LocalName("div".into()),
                    negation: true,
                },
            ),
        ] {
            assert_ast(
                &[selector],
                Ast {
                    root: vec![AstNode {
                        predicate: Predicate {
                            on_tag_name_exprs: vec![expected],
                            ..Default::default()
                        },
                        children: vec![],
                        descendants: vec![],
                        match_ids: DenseHashSet::from([0]),
                    }],
                    cumulative_node_count: 1,
                },
            );
        }
    }

    #[test]
    fn simple_attr_expression() {
        for (selector, expected) in [
            (
                "#foo",
                Expr {
                    simple_expr: OnAttributesExpr::Id("foo".into()),
                    negation: false,
                },
            ),
            (
                ".bar",
                Expr {
                    simple_expr: OnAttributesExpr::Class("bar".into()),
                    negation: false,
                },
            ),
            (
                "[foo]",
                Expr {
                    simple_expr: OnAttributesExpr::AttributeExists("foo".into()),
                    negation: false,
                },
            ),
            (
                "[FOO]",
                Expr {
                    simple_expr: OnAttributesExpr::AttributeExists("foo".into()),
                    negation: false,
                },
            ),
            (
                r#"[foo="bar"]"#,
                Expr {
                    simple_expr: OnAttributesExpr::AttributeComparisonExpr(
                        AttributeComparisonExpr {
                            name: "foo".into(),
                            value: "bar".into(),
                            case_sensitivity: ParsedCaseSensitivity::CaseSensitive,
                            operator: AttrSelectorOperator::Equal,
                        },
                    ),
                    negation: false,
                },
            ),
            (
                r#"[FOO="bar"]"#,
                Expr {
                    simple_expr: OnAttributesExpr::AttributeComparisonExpr(
                        AttributeComparisonExpr {
                            name: "foo".into(),
                            value: "bar".into(),
                            case_sensitivity: ParsedCaseSensitivity::CaseSensitive,
                            operator: AttrSelectorOperator::Equal,
                        },
                    ),
                    negation: false,
                },
            ),
            (
                r#"[foo*=""]"#,
                Expr {
                    simple_expr: OnAttributesExpr::AttributeComparisonExpr(
                        AttributeComparisonExpr {
                            name: "foo".into(),
                            value: "".into(),
                            case_sensitivity: ParsedCaseSensitivity::CaseSensitive,
                            operator: AttrSelectorOperator::Substring,
                        },
                    ),
                    negation: false,
                },
            ),
            (
                r#"[foo~="bar" i]"#,
                Expr {
                    simple_expr: OnAttributesExpr::AttributeComparisonExpr(
                        AttributeComparisonExpr {
                            name: "foo".into(),
                            value: "bar".into(),
                            case_sensitivity: ParsedCaseSensitivity::AsciiCaseInsensitive,
                            operator: AttrSelectorOperator::Includes,
                        },
                    ),
                    negation: false,
                },
            ),
            (
                r#"[foo|="bar" s]"#,
                Expr {
                    simple_expr: OnAttributesExpr::AttributeComparisonExpr(
                        AttributeComparisonExpr {
                            name: "foo".into(),
                            value: "bar".into(),
                            case_sensitivity: ParsedCaseSensitivity::ExplicitCaseSensitive,
                            operator: AttrSelectorOperator::DashMatch,
                        },
                    ),
                    negation: false,
                },
            ),
            (
                r#"[foo^="bar"]"#,
                Expr {
                    simple_expr: OnAttributesExpr::AttributeComparisonExpr(
                        AttributeComparisonExpr {
                            name: "foo".into(),
                            value: "bar".into(),
                            case_sensitivity: ParsedCaseSensitivity::CaseSensitive,
                            operator: AttrSelectorOperator::Prefix,
                        },
                    ),
                    negation: false,
                },
            ),
            (
                r#"[foo*="bar"]"#,
                Expr {
                    simple_expr: OnAttributesExpr::AttributeComparisonExpr(
                        AttributeComparisonExpr {
                            name: "foo".into(),
                            value: "bar".into(),
                            case_sensitivity: ParsedCaseSensitivity::CaseSensitive,
                            operator: AttrSelectorOperator::Substring,
                        },
                    ),
                    negation: false,
                },
            ),
            (
                r#"[foo$="bar"]"#,
                Expr {
                    simple_expr: OnAttributesExpr::AttributeComparisonExpr(
                        AttributeComparisonExpr {
                            name: "foo".into(),
                            value: "bar".into(),
                            case_sensitivity: ParsedCaseSensitivity::CaseSensitive,
                            operator: AttrSelectorOperator::Suffix,
                        },
                    ),
                    negation: false,
                },
            ),
            (
                r#":not([foo$="bar"])"#,
                Expr {
                    simple_expr: OnAttributesExpr::AttributeComparisonExpr(
                        AttributeComparisonExpr {
                            name: "foo".into(),
                            value: "bar".into(),
                            case_sensitivity: ParsedCaseSensitivity::CaseSensitive,
                            operator: AttrSelectorOperator::Suffix,
                        },
                    ),
                    negation: true,
                },
            ),
        ] {
            assert_ast(
                &[selector],
                Ast {
                    root: vec![AstNode {
                        predicate: Predicate {
                            on_attr_exprs: vec![expected],
                            ..Default::default()
                        },
                        children: vec![],
                        descendants: vec![],
                        match_ids: DenseHashSet::from([0]),
                    }],
                    cumulative_node_count: 1,
                },
            );
        }
    }

    #[test]
    fn compound_selectors() {
        assert_ast(
            &["div.foo#bar:not([baz])"],
            Ast {
                root: vec![AstNode {
                    predicate: Predicate {
                        on_tag_name_exprs: vec![Expr {
                            simple_expr: OnTagNameExpr::LocalName("div".into()),
                            negation: false,
                        }],
                        on_attr_exprs: vec![
                            Expr {
                                simple_expr: OnAttributesExpr::AttributeExists("baz".into()),
                                negation: true,
                            },
                            Expr {
                                simple_expr: OnAttributesExpr::Id("bar".into()),
                                negation: false,
                            },
                            Expr {
                                simple_expr: OnAttributesExpr::Class("foo".into()),
                                negation: false,
                            },
                        ],
                    },
                    children: vec![],
                    descendants: vec![],
                    match_ids: DenseHashSet::from([0]),
                }],
                cumulative_node_count: 1,
            },
        );
    }

    #[test]
    fn multiple_payloads() {
        assert_ast(
            &["#foo", "#foo"],
            Ast {
                root: vec![AstNode {
                    predicate: Predicate {
                        on_attr_exprs: vec![Expr {
                            simple_expr: OnAttributesExpr::Id("foo".into()),
                            negation: false,
                        }],
                        ..Default::default()
                    },
                    children: vec![],
                    descendants: vec![],
                    match_ids: DenseHashSet::from([0, 1]),
                }],
                cumulative_node_count: 1,
            },
        );
    }

    #[test]
    fn selector_list() {
        assert_ast(
            &["#foo > div, #foo > span", "#foo > .c1, #foo > .c2"],
            Ast {
                root: vec![AstNode {
                    predicate: Predicate {
                        on_attr_exprs: vec![Expr {
                            simple_expr: OnAttributesExpr::Id("foo".into()),
                            negation: false,
                        }],
                        ..Default::default()
                    },
                    children: vec![
                        AstNode {
                            predicate: Predicate {
                                on_tag_name_exprs: vec![Expr {
                                    simple_expr: OnTagNameExpr::LocalName("div".into()),
                                    negation: false,
                                }],
                                ..Default::default()
                            },
                            children: vec![],
                            descendants: vec![],
                            match_ids: DenseHashSet::from([0]),
                        },
                        AstNode {
                            predicate: Predicate {
                                on_tag_name_exprs: vec![Expr {
                                    simple_expr: OnTagNameExpr::LocalName("span".into()),
                                    negation: false,
                                }],
                                ..Default::default()
                            },
                            children: vec![],
                            descendants: vec![],
                            match_ids: DenseHashSet::from([0]),
                        },
                        AstNode {
                            predicate: Predicate {
                                on_attr_exprs: vec![Expr {
                                    simple_expr: OnAttributesExpr::Class("c1".into()),
                                    negation: false,
                                }],
                                ..Default::default()
                            },
                            children: vec![],
                            descendants: vec![],
                            match_ids: DenseHashSet::from([1]),
                        },
                        AstNode {
                            predicate: Predicate {
                                on_attr_exprs: vec![Expr {
                                    simple_expr: OnAttributesExpr::Class("c2".into()),
                                    negation: false,
                                }],
                                ..Default::default()
                            },
                            children: vec![],
                            descendants: vec![],
                            match_ids: DenseHashSet::from([1]),
                        },
                    ],
                    descendants: vec![],
                    match_ids: DenseHashSet::from([]),
                }],
                cumulative_node_count: 5,
            },
        );
    }

    #[test]
    fn combinators() {
        assert_ast(
            &[
                ".c1 > .c2 .c3 #foo",
                ".c1 > .c2 #bar",
                ".c1 > #qux",
                ".c1 #baz",
                ".c1 [foo] [bar]",
                "#quz",
            ],
            Ast {
                root: vec![
                    AstNode {
                        predicate: Predicate {
                            on_attr_exprs: vec![Expr {
                                simple_expr: OnAttributesExpr::Class("c1".into()),
                                negation: false,
                            }],
                            ..Default::default()
                        },
                        children: vec![
                            AstNode {
                                predicate: Predicate {
                                    on_attr_exprs: vec![Expr {
                                        simple_expr: OnAttributesExpr::Class("c2".into()),
                                        negation: false,
                                    }],
                                    ..Default::default()
                                },
                                children: vec![],
                                descendants: vec![
                                    AstNode {
                                        predicate: Predicate {
                                            on_attr_exprs: vec![Expr {
                                                simple_expr: OnAttributesExpr::Class("c3".into()),
                                                negation: false,
                                            }],
                                            ..Default::default()
                                        },
                                        children: vec![],
                                        descendants: vec![AstNode {
                                            predicate: Predicate {
                                                on_attr_exprs: vec![Expr {
                                                    simple_expr: OnAttributesExpr::Id("foo".into()),
                                                    negation: false,
                                                }],
                                                ..Default::default()
                                            },
                                            children: vec![],
                                            descendants: vec![],
                                            match_ids: DenseHashSet::from([0]),
                                        }],
                                        match_ids: DenseHashSet::from([]),
                                    },
                                    AstNode {
                                        predicate: Predicate {
                                            on_attr_exprs: vec![Expr {
                                                simple_expr: OnAttributesExpr::Id("bar".into()),
                                                negation: false,
                                            }],
                                            ..Default::default()
                                        },
                                        children: vec![],
                                        descendants: vec![],
                                        match_ids: DenseHashSet::from([1]),
                                    },
                                ],
                                match_ids: DenseHashSet::from([]),
                            },
                            AstNode {
                                predicate: Predicate {
                                    on_attr_exprs: vec![Expr {
                                        simple_expr: OnAttributesExpr::Id("qux".into()),
                                        negation: false,
                                    }],
                                    ..Default::default()
                                },
                                children: vec![],
                                descendants: vec![],
                                match_ids: DenseHashSet::from([2]),
                            },
                        ],
                        descendants: vec![
                            AstNode {
                                predicate: Predicate {
                                    on_attr_exprs: vec![Expr {
                                        simple_expr: OnAttributesExpr::Id("baz".into()),
                                        negation: false,
                                    }],
                                    ..Default::default()
                                },
                                children: vec![],
                                descendants: vec![],
                                match_ids: DenseHashSet::from([3]),
                            },
                            AstNode {
                                predicate: Predicate {
                                    on_attr_exprs: vec![Expr {
                                        simple_expr: OnAttributesExpr::AttributeExists(
                                            "foo".into(),
                                        ),
                                        negation: false,
                                    }],
                                    ..Default::default()
                                },
                                children: vec![],
                                descendants: vec![AstNode {
                                    predicate: Predicate {
                                        on_attr_exprs: vec![Expr {
                                            simple_expr: OnAttributesExpr::AttributeExists(
                                                "bar".into(),
                                            ),
                                            negation: false,
                                        }],
                                        ..Default::default()
                                    },
                                    children: vec![],
                                    descendants: vec![],
                                    match_ids: DenseHashSet::from([4]),
                                }],
                                match_ids: DenseHashSet::from([]),
                            },
                        ],
                        match_ids: DenseHashSet::from([]),
                    },
                    AstNode {
                        predicate: Predicate {
                            on_attr_exprs: vec![Expr {
                                simple_expr: OnAttributesExpr::Id("quz".into()),
                                negation: false,
                            }],
                            ..Default::default()
                        },
                        children: vec![],
                        descendants: vec![],
                        match_ids: DenseHashSet::from([5]),
                    },
                ],
                cumulative_node_count: 10,
            },
        );
    }

    #[test]
    fn parse_errors() {
        assert_err("div@", SelectorError::UnexpectedToken);
        assert_err("div.", SelectorError::UnexpectedEnd);
        assert_err(r#"div[="foo"]"#, SelectorError::MissingAttributeName);
        assert_err("", SelectorError::EmptySelector);
        assert_err("div >", SelectorError::DanglingCombinator);
        assert_err(
            r#"div[foo~"bar"]"#,
            SelectorError::UnexpectedTokenInAttribute,
        );
        assert_err("svg|img", SelectorError::NamespacedSelector);
        assert_err("[*|Foo]", SelectorError::NamespacedSelector);
        assert_err("[*|Foo=bar]", SelectorError::NamespacedSelector);
        assert_err(".foo()", SelectorError::InvalidClassName);
        assert_err(":not()", SelectorError::EmptySelector);
        assert_err("div + span", SelectorError::UnsupportedCombinator('+'));
        assert_err("div ~ span", SelectorError::UnsupportedCombinator('~'));
        assert_err(":nth-child(n of a)", SelectorError::UnexpectedToken);
    }

    #[test]
    fn pseudo_class_parse_errors() {
        for s in &[
            ":active",
            ":any-link",
            ":blank",
            ":checked",
            ":current",
            ":default",
            ":defined",
            ":dir(rtl)",
            ":disabled",
            ":drop",
            ":empty",
            ":enabled",
            ":first",
            ":fullscreen",
            ":future",
            ":focus",
            ":focus-visible",
            ":focus-within",
            ":has(div)",
            ":host",
            ":host(h1)",
            ":host-context(h1)",
            ":hover",
            ":indeterminate",
            ":in-range",
            ":invalid",
            ":is(header)",
            ":lang(en)",
            ":last-child",
            ":last-of-type",
            ":left",
            ":link",
            ":local-link",
            ":nth-col(1)",
            ":nth-last-child(1)",
            ":nth-last-col(1)",
            ":nth-last-of-type(1)",
            ":only-child",
            ":only-of-type",
            ":optional",
            ":out-of-range",
            ":past",
            ":placeholder-shown",
            ":read-only",
            ":read-write",
            ":required",
            ":right",
            ":root",
            ":scope",
            ":target",
            ":target-within",
            ":user-invalid",
            ":valid",
            ":visited",
            ":where(p)",
            ":not(foo bar)",
            ":not(foo > bar)",
            ":not(* > .x)",
        ] {
            assert_err(s, SelectorError::UnsupportedPseudoClassOrElement);
        }
    }

    #[test]
    fn pseudo_elements_parse_errors() {
        for s in &[
            "::after",
            "::backdrop",
            "::before",
            "::cue",
            "::first-letter",
            "::first-line",
            "::grammar-error",
            "::marker",
            "::placeholder",
            "::selection",
            "::slotted()",
            "::spelling-error",
        ] {
            assert_err(s, SelectorError::UnsupportedPseudoClassOrElement);
        }
    }

    #[test]
    fn negated_pseudo_class_parse_error() {
        assert_err(
            ":not(:nth-last-child(even))",
            SelectorError::UnsupportedPseudoClassOrElement,
        );
    }

    #[test]
    fn nested_not_selector() {
        assert_ast(
            &[":not(:not(div))"],
            Ast {
                root: vec![AstNode {
                    predicate: Predicate {
                        on_tag_name_exprs: vec![Expr {
                            simple_expr: OnTagNameExpr::LocalName("div".into()),
                            negation: false, // simplified double negation
                        }],
                        ..Default::default()
                    },
                    children: vec![],
                    descendants: vec![],
                    match_ids: DenseHashSet::from([0]),
                }],
                cumulative_node_count: 1,
            },
        );

        assert_ast(
            &[":not(:not(:not(div)))"],
            Ast {
                root: vec![AstNode {
                    predicate: Predicate {
                        on_tag_name_exprs: vec![Expr {
                            simple_expr: OnTagNameExpr::LocalName("div".into()),
                            negation: true,
                        }],
                        ..Default::default()
                    },
                    children: vec![],
                    descendants: vec![],
                    match_ids: DenseHashSet::from([0]),
                }],
                cumulative_node_count: 1,
            },
        );

        assert_ast(
            &["div:not(:not(.foo))"],
            Ast {
                root: vec![AstNode {
                    predicate: Predicate {
                        on_tag_name_exprs: vec![Expr {
                            simple_expr: OnTagNameExpr::LocalName("div".into()),
                            negation: false,
                        }],
                        on_attr_exprs: vec![Expr {
                            simple_expr: OnAttributesExpr::Class("foo".into()),
                            negation: false, // simplified double negation
                        }],
                    },
                    children: vec![],
                    descendants: vec![],
                    match_ids: DenseHashSet::from([0]),
                }],
                cumulative_node_count: 1,
            },
        );
    }

    fn tag_name(name: &str, negation: bool) -> Predicate {
        Predicate {
            on_tag_name_exprs: vec![Expr {
                simple_expr: OnTagNameExpr::LocalName(name.into()),
                negation,
            }],
            ..Default::default()
        }
    }

    fn class(name: &str, negation: bool) -> Predicate {
        Predicate {
            on_attr_exprs: vec![Expr {
                simple_expr: OnAttributesExpr::Class(name.into()),
                negation,
            }],
            ..Default::default()
        }
    }

    fn leaf(predicate: Predicate) -> AstNode {
        AstNode {
            predicate,
            children: vec![],
            descendants: vec![],
            match_ids: DenseHashSet::from([0]),
        }
    }

    #[test]
    fn negated_compound_selector_has_an_alternative_per_simple_selector() {
        // `!(div && .foo)` is `!div || !.foo`
        assert_ast(
            &[":not(div.foo)"],
            Ast {
                root: vec![leaf(tag_name("div", true)), leaf(class("foo", true))],
                cumulative_node_count: 2,
            },
        );

        // `!(div || span)` is `!div && !span`
        assert_ast(
            &[":not(div, span)"],
            Ast {
                root: vec![leaf(tag_name("div", true).and(&tag_name("span", true)))],
                cumulative_node_count: 1,
            },
        );
    }

    #[test]
    fn double_negation_of_a_selector_list_has_an_alternative_per_selector() {
        for selector in [":not(:not(div, span))", ":not(:not(div):not(span))"] {
            assert_ast(
                &[selector],
                Ast {
                    root: vec![leaf(tag_name("div", false)), leaf(tag_name("span", false))],
                    cumulative_node_count: 2,
                },
            );
        }

        assert_ast(
            &[":not(:not(div.foo))"],
            Ast {
                root: vec![leaf(tag_name("div", false).and(&class("foo", false)))],
                cumulative_node_count: 1,
            },
        );
    }

    #[test]
    fn alternatives_of_every_compound_selector_are_combined() {
        let child = |predicate| AstNode {
            children: vec![leaf(tag_name("a", true)), leaf(class("b", true))],
            match_ids: DenseHashSet::new(),
            ..leaf(predicate)
        };

        assert_ast(
            &[":not(div.foo) > :not(a.b)"],
            Ast {
                root: vec![child(tag_name("div", true)), child(class("foo", true))],
                cumulative_node_count: 6,
            },
        );
    }

    #[test]
    fn too_many_alternatives() {
        let selector =
            |count| -> String { (0..count).map(|i| format!(":not(a{i}.b{i})")).collect() };

        // 2^8 alternatives
        let mut ast = Ast::default();
        ast.add_selector(&selector(8).parse().unwrap(), 0);
        assert_eq!(ast.root.len(), MAX_ALTERNATIVES);

        assert_err(&selector(9), SelectorError::UnsupportedSyntax);
        assert_err(
            &format!("{} > {}", selector(5), selector(4)),
            SelectorError::UnsupportedSyntax,
        );
        assert_err(&selector(200), SelectorError::UnsupportedSyntax);

        // Every `:not()` has one alternative here, however many there are.
        ":not(.a)".repeat(1000).parse::<Selector>().unwrap();
    }

    #[test]
    fn nth_child_is_index() {
        let even = NthChild::new(2, 0);
        assert!(even.has_index(2));
        assert!(!even.has_index(1));

        let odd = NthChild::new(2, 1);
        assert!(odd.has_index(1));
        assert!(!odd.has_index(2));
        assert!(odd.has_index(3));

        let first = NthChild::new(0, 1);
        assert!(first.has_index(1));
        assert!(!first.has_index(2));
        assert!(!first.has_index(3));
    }
}

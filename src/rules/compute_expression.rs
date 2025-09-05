use crate::nodes::{BinaryOperator, Block, Expression};
use crate::utils::origin::{anchor_from_expression, token_from_content_with_anchor};
use crate::process::{DefaultVisitor, Evaluator, NodeProcessor, NodeVisitor};
use crate::rules::{
    Context, FlawlessRule, RuleConfiguration, RuleConfigurationError, RuleProperties,
};

use super::verify_no_rule_properties;

#[derive(Debug, Clone, Default)]
struct Computer {
    evaluator: Evaluator,
}

impl Computer {
    fn replace_with(&mut self, expression: &Expression) -> Option<Expression> {
        match expression {
            Expression::Unary(_) => {
                if !self.evaluator.has_side_effects(expression) {
                    self.evaluator.evaluate(expression).to_expression()
                } else {
                    None
                }
            }
            Expression::Binary(binary) => {
                if !self.evaluator.has_side_effects(expression) {
                    self.evaluator
                        .evaluate(expression)
                        .to_expression()
                        .or_else(|| {
                            match binary.operator() {
                                BinaryOperator::And => {
                                    self.evaluator.evaluate(binary.left()).is_truthy().map(
                                        |is_truthy| {
                                            if is_truthy {
                                                binary.right().clone()
                                            } else {
                                                binary.left().clone()
                                            }
                                        },
                                    )
                                }
                                BinaryOperator::Or => {
                                    self.evaluator.evaluate(binary.left()).is_truthy().map(
                                        |is_truthy| {
                                            if is_truthy {
                                                binary.left().clone()
                                            } else {
                                                binary.right().clone()
                                            }
                                        },
                                    )
                                }
                                _ => None,
                            }
                            .map(|mut expression| {
                                self.process_expression(&mut expression);
                                expression
                            })
                        })
                } else {
                    match binary.operator() {
                        BinaryOperator::And => {
                            if !self.evaluator.has_side_effects(binary.left()) {
                                self.evaluator.evaluate(binary.left()).is_truthy().map(
                                    |is_truthy| {
                                        if is_truthy {
                                            binary.right().clone()
                                        } else {
                                            binary.left().clone()
                                        }
                                    },
                                )
                            } else {
                                None
                            }
                        }
                        BinaryOperator::Or => {
                            if !self.evaluator.has_side_effects(binary.left()) {
                                self.evaluator.evaluate(binary.left()).is_truthy().map(
                                    |is_truthy| {
                                        if is_truthy {
                                            binary.left().clone()
                                        } else {
                                            binary.right().clone()
                                        }
                                    },
                                )
                            } else {
                                None
                            }
                        }
                        _ => None,
                    }
                }
            }
            Expression::If(_) => {
                if !self.evaluator.has_side_effects(expression) {
                    self.evaluator.evaluate(expression).to_expression()
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

impl NodeProcessor for Computer {
    fn process_expression(&mut self, expression: &mut Expression) {
        if let Some(evaluated) = self.replace_with(expression) {
            let mut replace_with = evaluated;
            use crate::generator::utils as gen_utils;

            if let Some(anchor) = anchor_from_expression(expression) {
                // Best-effort: set token on simple literal results
                use crate::nodes::Expression as E;
                match &mut replace_with {
                    E::True(token_opt) => {
                        let token = token_from_content_with_anchor("true", anchor);
                        *token_opt = Some(token);
                    }
                    E::False(token_opt) => {
                        let token = token_from_content_with_anchor("false", anchor);
                        *token_opt = Some(token);
                    }
                    E::Nil(token_opt) => {
                        let token = token_from_content_with_anchor("nil", anchor);
                        *token_opt = Some(token);
                    }
                    E::VariableArguments(token_opt) => {
                        let token = token_from_content_with_anchor("...", anchor);
                        *token_opt = Some(token);
                    }
                    E::Number(num) => {
                        if num.get_token().is_none() {
                            let content = gen_utils::write_number(num);
                            let token = token_from_content_with_anchor(content, anchor);
                            num.set_token(token);
                        }
                    }
                    E::String(str_expr) => {
                        if str_expr.get_token().is_none() {
                            let content = gen_utils::write_string(str_expr.get_value());
                            let token = token_from_content_with_anchor(content, anchor);
                            str_expr.set_token(token);
                        }
                    }
                    _ => {}
                }
            }

            *expression = replace_with;
        }
    }
}

pub const COMPUTE_EXPRESSIONS_RULE_NAME: &str = "compute_expression";

/// A rule that compute expressions that do not have any side-effects.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ComputeExpression {}

impl FlawlessRule for ComputeExpression {
    fn flawless_process(&self, block: &mut Block, _: &Context) {
        let mut processor = Computer::default();
        DefaultVisitor::visit_block(block, &mut processor);
    }
}

impl RuleConfiguration for ComputeExpression {
    fn configure(&mut self, properties: RuleProperties) -> Result<(), RuleConfigurationError> {
        verify_no_rule_properties(&properties)?;

        Ok(())
    }

    fn get_name(&self) -> &'static str {
        COMPUTE_EXPRESSIONS_RULE_NAME
    }

    fn serialize_to_properties(&self) -> RuleProperties {
        RuleProperties::new()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::rules::Rule;
    use crate::frontend::Resources;
    use crate::rules::ContextBuilder;
    use crate::nodes::*;

    use insta::assert_json_snapshot;

    fn new_rule() -> ComputeExpression {
        ComputeExpression::default()
    }

    #[test]
    fn serialize_default_rule() {
        let rule: Box<dyn Rule> = Box::new(new_rule());

        assert_json_snapshot!("default_compute_expression", rule);
    }

    #[test]
    fn preserves_origin_when_replacing_expression_with_literal() {
        // return 1 + 2  (operator token anchored at line 7, source 1)
        let mut binary = BinaryExpression::new(
            BinaryOperator::Plus,
            DecimalNumber::new(1.0),
            DecimalNumber::new(2.0),
        );
        binary.set_token(Token::from_content_with_origin("+", 7, 1));

        let expr: Expression = binary.into();
        let mut block = Block::default()
            .with_last_statement(ReturnStatement::one(expr));

        let resources = Resources::from_memory();
        let context = ContextBuilder::new("test.lua", &resources, "").build();

        new_rule().flawless_process(&mut block, &context);

        let ret = block.get_last_statement().expect("has last statement");
        match ret {
            LastStatement::Return(ret) => {
                let expr = ret.iter_expressions().next().expect("one expr");
                match expr {
                    Expression::Number(num) => {
                        let tok = num.get_token().expect("token present");
                        assert_eq!(tok.get_line_number(), Some(7));
                        assert_eq!(tok.get_source_id(), Some(1));
                    }
                    _ => panic!("expected number expression"),
                }
            }
            _ => panic!("expected return statement"),
        }
    }
}

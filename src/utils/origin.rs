use crate::nodes::{Expression, Token};
use std::borrow::Cow;

/// Anchors represent an origin (line, source_id) to apply to synthesized tokens.
#[derive(Debug, Clone, Copy)]
pub struct OriginAnchor {
    pub line_number: usize,
    pub source_id: u32,
}

impl OriginAnchor {
    pub fn from_token(token: &Token) -> Option<Self> {
        Some(Self {
            line_number: token.get_line_number()?,
            source_id: token.get_source_id()?,
        })
    }
}

/// Try to extract an origin anchor from an expression by inspecting a few common tokens.
pub fn anchor_from_expression(expr: &Expression) -> Option<OriginAnchor> {
    use crate::nodes::Expression as E;
    match expr {
        E::Identifier(id) => id.get_token().and_then(OriginAnchor::from_token),
        E::Number(num) => num.get_token().and_then(OriginAnchor::from_token),
        E::String(s) => s.get_token().and_then(OriginAnchor::from_token),
        E::Unary(u) => u.get_token().and_then(OriginAnchor::from_token),
        E::Binary(b) => b.get_token().and_then(OriginAnchor::from_token),
        E::If(i) => i.get_tokens().map(|t| &t.r#if).and_then(OriginAnchor::from_token),
        _ => None,
    }
}

/// Apply origin to a token content, preserving source_id and line.
pub fn token_from_content_with_anchor(
    content: impl Into<Cow<'static, str>>,
    anchor: OriginAnchor,
) -> Token {
    Token::from_content_with_origin(content, anchor.line_number, anchor.source_id)
}

/// Create a token from content and origin, preserving trivia from an existing token.
pub fn token_from_content_with_anchor_preserve_trivia(
    content: impl Into<Cow<'static, str>>,
    anchor: OriginAnchor,
    from: &Token,
) -> Token {
    let mut token = token_from_content_with_anchor(content, anchor);
    for trivia in from.iter_leading_trivia() {
        token.push_leading_trivia(trivia.clone());
    }
    for trivia in from.iter_trailing_trivia() {
        token.push_trailing_trivia(trivia.clone());
    }
    token
}


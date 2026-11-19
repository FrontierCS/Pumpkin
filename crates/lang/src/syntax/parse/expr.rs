//! The parser's expressions, by the grammar in [`crate::syntax`], as methods of the parent's
//! `Parser`, which keeps the statements and the token helpers.

use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;

use super::{At, Parser, left_out, target};
use crate::problem::{Fault, refusal};
use crate::syntax::lex::{Keyword, Punct, Token, TokenKind};
use crate::syntax::{
    Args, Arithmetic, Clause, Comprehension, Expr, ExprKind, Logic, Name, Operator, Target, Unary,
};
use crate::value::Span;

// The binding of each level of binary operator, from the loosest. `not` binds between `and` and
// the comparisons, as in the specification's grammar.
const OR: u8 = 0;
const AND: u8 = 1;
const NOT: u8 = 2;
const COMPARE: u8 = 3;
const UNION: u8 = 4;
const SUM: u8 = 5;
const PRODUCT: u8 = 6;

#[derive(Clone, Copy)]
enum Infix {
    Logic(Logic),
    Value(Operator),
}

const TRAILING: &str = "a tuple without parentheses can't end with `,`: drop the `,`, or put the \
                        tuple in parentheses";

enum Argument {
    Positional(Expr),
    Keyword(Name, Expr),
    /// The span of `**`, and the dict it passes.
    Kwargs(Span, Expr),
}

impl Parser<'_> {
    pub(super) fn expressions(&mut self) -> Result<Expr, Fault> {
        let first = self.test()?;
        if !self.at_punct(Punct::Comma) {
            return Ok(first);
        }
        let mut items = Vec::from([first]);
        while self.at_punct(Punct::Comma) {
            let comma = self.advance();
            // What ends a statement's expressions, or an assignment's target.
            if matches!(
                self.peek().kind,
                TokenKind::Newline | TokenKind::Punct(Punct::Eq | Punct::Semicolon)
            ) {
                return Err(Fault::new(comma, TRAILING));
            }
            items.push(self.test()?);
        }
        let span = items[0].span.to(items[items.len() - 1].span);
        Ok(Expr {
            kind: ExprKind::Tuple(items),
            span,
        })
    }

    pub(super) fn test(&mut self) -> Result<Expr, Fault> {
        let then = self.binary(OR)?;
        if self.at_keyword(Keyword::If) {
            self.conditional(then)
        } else {
            Ok(then)
        }
    }

    fn conditional(&mut self, then: Expr) -> Result<Expr, Fault> {
        let at = self.advance();
        let (condition, otherwise) = self.nested(at, |parser| {
            let condition = parser.binary(OR)?;
            if !parser.at_keyword(Keyword::Else) {
                let message = "this conditional expression has no `else`: give the value when \
                               the condition is false, after `else`";
                return Err(Fault::new(parser.peek().span, message));
            }
            parser.advance();
            Ok((condition, parser.test()?))
        })?;
        let span = then.span.to(otherwise.span);
        Ok(Expr {
            kind: ExprKind::Conditional {
                then: Box::new(then),
                condition: Box::new(condition),
                otherwise: Box::new(otherwise),
            },
            span,
        })
    }

    // Operators that bind at least as tight as `min`, each taking its right operand one level
    // tighter than itself, so that equal ones group from the left.
    fn binary(&mut self, min: u8) -> Result<Expr, Fault> {
        let mut left = if min <= NOT && self.at_keyword(Keyword::Not) {
            self.not()?
        } else {
            self.unary()?
        };
        while let Some((infix, precedence)) = self.infix()?
            && precedence >= min
        {
            left = match infix {
                Infix::Logic(logic) => self.logic(left, logic, precedence)?,
                Infix::Value(operator) => self.chain(left, operator, precedence)?,
            };
        }
        Ok(left)
    }

    fn not(&mut self) -> Result<Expr, Fault> {
        let at = self.advance();
        let operand = self.nested(at, |parser| parser.binary(NOT))?;
        let span = at.to(operand.span);
        Ok(Expr {
            kind: ExprKind::Unary {
                operator: Unary::Not,
                at,
                operand: Box::new(operand),
            },
            span,
        })
    }

    fn logic(&mut self, first: Expr, logic: Logic, precedence: u8) -> Result<Expr, Fault> {
        // Every `and` or `or` of a run is one node, whatever its length, so that the run does not
        // nest in the tree.
        let mut rest = Vec::new();
        while self.at_keyword(logic.keyword()) {
            self.advance();
            rest.push(self.binary(precedence + 1)?);
        }
        let span = first.span.to(rest[rest.len() - 1].span);
        Ok(Expr {
            kind: ExprKind::Logic {
                operator: logic,
                first: Box::new(first),
                rest,
            },
            span,
        })
    }

    fn chain(&mut self, first: Expr, operator: Operator, precedence: u8) -> Result<Expr, Fault> {
        let mut rest = Vec::new();
        let mut operator = operator;
        loop {
            let at = self.advance_operator(operator);
            rest.push((operator, at, self.binary(precedence + 1)?));
            match self.infix()? {
                Some((_, next)) if next == precedence && precedence == COMPARE => {
                    let message = "comparisons don't chain in Starlark: join them with `and`";
                    return Err(Fault::new(self.peek().span, message));
                }
                Some((Infix::Value(next), next_precedence)) if next_precedence == precedence => {
                    operator = next;
                }
                Some(_) | None => break,
            }
        }
        let span = first.span.to(rest[rest.len() - 1].2.span);
        Ok(Expr {
            kind: ExprKind::Binary {
                first: Box::new(first),
                rest,
            },
            span,
        })
    }

    // The binary operator at the next token, and its precedence. An operator that the subset leaves
    // out is refused wherever it is, not passed over.
    fn infix(&self) -> Result<Option<(Infix, u8)>, Fault> {
        let infix = match self.peek().kind {
            TokenKind::Keyword(Keyword::Or) => (Infix::Logic(Logic::Or), OR),
            TokenKind::Keyword(Keyword::And) => (Infix::Logic(Logic::And), AND),
            _ => match self.operator()? {
                Some((operator, precedence)) => (Infix::Value(operator), precedence),
                None => return Ok(None),
            },
        };
        Ok(Some(infix))
    }

    fn operator(&self) -> Result<Option<(Operator, u8)>, Fault> {
        let Token { kind, span } = self.peek();
        let arithmetic = |arithmetic, precedence| (Operator::Arithmetic(arithmetic), precedence);
        let operator = match kind {
            TokenKind::Keyword(Keyword::In) => (Operator::In, COMPARE),
            TokenKind::Keyword(Keyword::Not)
                if matches!(self.peek_next(), Some(TokenKind::Keyword(Keyword::In))) =>
            {
                (Operator::NotIn, COMPARE)
            }
            TokenKind::Punct(Punct::EqEq) => (Operator::Equal, COMPARE),
            TokenKind::Punct(Punct::BangEq) => (Operator::NotEqual, COMPARE),
            TokenKind::Punct(Punct::Less) => (Operator::Less, COMPARE),
            TokenKind::Punct(Punct::LessEq) => (Operator::LessEqual, COMPARE),
            TokenKind::Punct(Punct::Greater) => (Operator::Greater, COMPARE),
            TokenKind::Punct(Punct::GreaterEq) => (Operator::GreaterEqual, COMPARE),
            TokenKind::Punct(Punct::Pipe) => (Operator::Union, UNION),
            TokenKind::Punct(Punct::Plus) => arithmetic(Arithmetic::Add, SUM),
            TokenKind::Punct(Punct::Minus) => arithmetic(Arithmetic::Subtract, SUM),
            TokenKind::Punct(Punct::Star) => arithmetic(Arithmetic::Multiply, PRODUCT),
            TokenKind::Punct(Punct::Slash) => arithmetic(Arithmetic::Divide, PRODUCT),
            TokenKind::Punct(Punct::SlashSlash) => arithmetic(Arithmetic::FloorDivide, PRODUCT),
            TokenKind::Punct(Punct::Percent) => arithmetic(Arithmetic::Modulo, PRODUCT),
            _ => {
                return match left_out(kind, At::Operator) {
                    Some(message) => Err(Fault::new(*span, message)),
                    None => Ok(None),
                };
            }
        };
        Ok(Some(operator))
    }

    fn advance_operator(&mut self, operator: Operator) -> Span {
        let first = self.advance();
        match operator {
            Operator::NotIn => first.to(self.advance()),
            Operator::Equal
            | Operator::NotEqual
            | Operator::Less
            | Operator::LessEqual
            | Operator::Greater
            | Operator::GreaterEqual
            | Operator::In
            | Operator::Union
            | Operator::Arithmetic(_) => first,
        }
    }

    fn unary(&mut self) -> Result<Expr, Fault> {
        if let Some(message) = left_out(&self.peek().kind, At::Operand) {
            return Err(Fault::new(self.peek().span, message));
        }
        if !self.at_punct(Punct::Minus) {
            return self.primary();
        }
        let at = self.advance();
        let operand = Box::new(self.nested(at, Self::unary)?);
        let span = at.to(operand.span);
        Ok(Expr {
            kind: ExprKind::Unary {
                operator: Unary::Negate,
                at,
                operand,
            },
            span,
        })
    }

    fn primary(&mut self) -> Result<Expr, Fault> {
        let mut expr = self.operand()?;
        // Each call, index or method nests the expression before it in the tree, so a chain counts
        // as deep as it is long, and the depth is its start's again once it ends.
        let depth = self.depth;
        // Each suffix is read by a function of its own, so that the frames on the deepest paths
        // hold only the one being read.
        loop {
            expr = if self.at_punct(Punct::LeftParen) {
                self.call(expr)?
            } else if self.at_punct(Punct::LeftBracket) {
                self.indexed(expr)?
            } else if self.at_punct(Punct::Dot) {
                self.method(expr)?
            } else {
                break;
            };
        }
        self.depth = depth;
        Ok(expr)
    }

    fn call(&mut self, callee: Expr) -> Result<Expr, Fault> {
        let open = self.advance();
        self.nest(open)?;
        let depth = self.depth;
        let (args, close) = self.arguments()?;
        let span = callee.span.to(close);
        let kind = ExprKind::Call {
            callee: Box::new(callee),
            args: Box::new(args),
            depth,
        };
        Ok(Expr { kind, span })
    }

    fn indexed(&mut self, object: Expr) -> Result<Expr, Fault> {
        let open = self.advance();
        self.nest(open)?;
        let (index, close) = self.index()?;
        let span = object.span.to(close);
        let kind = ExprKind::Index {
            object: Box::new(object),
            index: Box::new(index),
        };
        Ok(Expr { kind, span })
    }

    fn method(&mut self, receiver: Expr) -> Result<Expr, Fault> {
        self.advance();
        let name = self.name("a method's name")?;
        if !self.at_punct(Punct::LeftParen) {
            let message = refusal(
                "a method named but not called",
                format_args!("call it here, as `.{}(…)`", name.text),
            );
            return Err(Fault::new(name.span, message));
        }
        let open = self.advance();
        self.nest(open)?;
        let (args, close) = self.arguments()?;
        let span = receiver.span.to(close);
        let kind = ExprKind::Method {
            receiver: Box::new(receiver),
            name: Box::new(name),
            args: Box::new(args),
        };
        Ok(Expr { kind, span })
    }

    fn index(&mut self) -> Result<(Expr, Span), Fault> {
        let slice = |at: Span| {
            let message = refusal("a slice", "index the elements one by one");
            Fault::new(at, message)
        };
        if self.at_punct(Punct::Colon) {
            return Err(slice(self.peek().span));
        }
        let index = self.test()?;
        if self.at_punct(Punct::Colon) {
            return Err(slice(self.peek().span));
        }
        if self.at_punct(Punct::Comma) {
            let message = refusal("an index of several values", "index with one value");
            return Err(Fault::new(self.peek().span, message));
        }
        let close = self.expect_punct(Punct::RightBracket, "`]`")?;
        Ok((index, close))
    }

    fn arguments(&mut self) -> Result<(Args, Span), Fault> {
        let (arguments, close) = self.sequence(Punct::RightParen, Self::argument)?;
        let mut positional = Vec::new();
        let mut keyword: Vec<(Name, Expr)> = Vec::new();
        let mut kwargs = None;
        // At most one `**`, after every other argument, as in starlark-go's resolver:
        // https://github.com/google/starlark-go/blob/4f4267fb140d1e8b91c9bce55cee2f809e8840dd/resolve/resolve.go#L736-L789
        for argument in arguments {
            match argument {
                Argument::Positional(arg) if kwargs.is_some() => {
                    let message = "a positional argument follows `**`: put positional arguments \
                                   first";
                    return Err(Fault::new(arg.span, message));
                }
                Argument::Positional(arg) if keyword.is_empty() => positional.push(arg),
                Argument::Positional(arg) => {
                    let message = "a positional argument follows a keyword argument: put \
                                   positional arguments first";
                    return Err(Fault::new(arg.span, message));
                }
                Argument::Keyword(name, _) if kwargs.is_some() => {
                    let message = "a keyword argument follows `**`: put `**` last";
                    return Err(Fault::new(name.span, message));
                }
                Argument::Keyword(name, _) if keyword.iter().any(|(n, _)| n.text == name.text) => {
                    let message = format!("`{}` is given twice: give it once", name.text);
                    return Err(Fault::new(name.span, message));
                }
                Argument::Keyword(name, arg) => keyword.push((name, arg)),
                Argument::Kwargs(at, _) if kwargs.is_some() => {
                    let message = "this is a second `**`: pass one dict, joining them with `|`";
                    return Err(Fault::new(at, message));
                }
                Argument::Kwargs(_, arg) => kwargs = Some(arg),
            }
        }
        let args = Args {
            positional,
            keyword,
            kwargs,
        };
        Ok((args, close))
    }

    fn argument(&mut self) -> Result<Argument, Fault> {
        match self.peek().kind {
            TokenKind::Punct(Punct::Star) => {
                let message = refusal(
                    "`*` before an argument, which passes a sequence's elements,",
                    "pass each argument by itself",
                );
                return Err(Fault::new(self.peek().span, message));
            }
            TokenKind::Punct(Punct::StarStar) => {
                let at = self.advance();
                return Ok(Argument::Kwargs(at, self.test()?));
            }
            _ => {}
        }
        if let Some(name) = self.keyword_name() {
            return Ok(Argument::Keyword(name, self.test()?));
        }
        self.test().map(Argument::Positional)
    }

    fn operand(&mut self) -> Result<Expr, Fault> {
        let span = self.peek().span;
        let kind = match &self.peek().kind {
            TokenKind::Name => ExprKind::Name(self.spelling().into()),
            TokenKind::Int(int) => ExprKind::Int(*int),
            TokenKind::Float(float) => ExprKind::Float(*float),
            TokenKind::Str(text) => ExprKind::Str(text.clone()),
            TokenKind::Punct(Punct::LeftBracket) => return self.bracketed(Self::list),
            TokenKind::Punct(Punct::LeftBrace) => return self.bracketed(Self::dict),
            TokenKind::Punct(Punct::LeftParen) => return self.bracketed(Self::parenthesized),
            _ => return Err(self.unexpected("an expression")),
        };
        self.advance();
        Ok(Expr { kind, span })
    }

    fn bracketed(
        &mut self,
        inner: fn(&mut Self, Span) -> Result<Expr, Fault>,
    ) -> Result<Expr, Fault> {
        let open = self.advance();
        self.nested(open, |parser| inner(parser, open))
    }

    fn list(&mut self, open: Span) -> Result<Expr, Fault> {
        if self.at_punct(Punct::RightBracket) {
            let close = self.advance();
            return Ok(Expr {
                kind: ExprKind::List(Vec::new()),
                span: open.to(close),
            });
        }
        let first = self.test()?;
        if self.at_keyword(Keyword::For) {
            return self.list_comprehension(open, first);
        }
        let (items, close) = self.after(first, Punct::RightBracket, Self::test)?;
        Ok(Expr {
            kind: ExprKind::List(items),
            span: open.to(close),
        })
    }

    fn list_comprehension(&mut self, open: Span, body: Expr) -> Result<Expr, Fault> {
        let comprehension = Box::new(self.comprehension()?);
        let close = self.expect_punct(Punct::RightBracket, "`]`")?;
        Ok(Expr {
            kind: ExprKind::ListComp {
                body: Box::new(body),
                comprehension,
            },
            span: open.to(close),
        })
    }

    fn dict(&mut self, open: Span) -> Result<Expr, Fault> {
        if self.at_punct(Punct::RightBrace) {
            let close = self.advance();
            return Ok(Expr {
                kind: ExprKind::Dict(Vec::new()),
                span: open.to(close),
            });
        }
        let key = self.test()?;
        if self.at_punct(Punct::Comma)
            || self.at_punct(Punct::RightBrace)
            || self.at_keyword(Keyword::For)
        {
            let message = "a set literal is not in Starlark: write a list, or give each key a \
                           value after `:`";
            return Err(Fault::new(open, message));
        }
        self.expect_punct(Punct::Colon, "`:`")?;
        let value = self.test()?;
        if self.at_keyword(Keyword::For) {
            let comprehension = Box::new(self.comprehension()?);
            let close = self.expect_punct(Punct::RightBrace, "`}`")?;
            return Ok(Expr {
                kind: ExprKind::DictComp {
                    key: Box::new(key),
                    value: Box::new(value),
                    comprehension,
                },
                span: open.to(close),
            });
        }
        let (entries, close) = self.after((key, value), Punct::RightBrace, Self::entry)?;
        Ok(Expr {
            kind: ExprKind::Dict(entries),
            span: open.to(close),
        })
    }

    fn parenthesized(&mut self, open: Span) -> Result<Expr, Fault> {
        let mut items = Vec::new();
        if !self.at_punct(Punct::RightParen) {
            let first = self.test()?;
            if self.at_keyword(Keyword::For) {
                let message = "a generator is not in Starlark: build a list, as `[x for x in xs]`";
                return Err(Fault::new(self.peek().span, message));
            }
            if !self.at_punct(Punct::Comma) {
                self.expect_punct(Punct::RightParen, "`,` or `)`")?;
                return Ok(first);
            }
            self.advance();
            items.push(first);
        }
        let (rest, close) = self.sequence(Punct::RightParen, Self::test)?;
        items.extend(rest);
        let span = open.to(close);
        Ok(Expr {
            kind: ExprKind::Tuple(items),
            span,
        })
    }

    fn entry(&mut self) -> Result<(Expr, Expr), Fault> {
        let key = self.test()?;
        self.expect_punct(Punct::Colon, "`:`")?;
        Ok((key, self.test()?))
    }

    fn comprehension(&mut self) -> Result<Comprehension, Fault> {
        // Each clause nests the ones after it, and the depth is the comprehension's start again
        // once they end.
        let depth = self.depth;
        let (target, iterable) = self.for_clause()?;
        let mut clauses = Vec::new();
        loop {
            match self.peek().kind {
                TokenKind::Keyword(Keyword::For) => {
                    let (target, iterable) = self.for_clause()?;
                    clauses.push(Clause::For { target, iterable });
                }
                TokenKind::Keyword(Keyword::If) => {
                    let at = self.advance();
                    self.nest(at)?;
                    clauses.push(Clause::If(self.binary(OR)?));
                }
                _ => break,
            }
        }
        self.depth = depth;
        Ok(Comprehension {
            target,
            iterable: Box::new(iterable),
            clauses,
        })
    }

    fn for_clause(&mut self) -> Result<(Target, Expr), Fault> {
        let keyword = self.advance();
        self.nest(keyword)?;
        // The targets are read as primary expressions, as a full one would take `in` for an
        // operator, and the iterable is read as no conditional expression, which would take the
        // next clause's `if` for its own.
        let first = self.primary()?;
        let variables = if self.at_punct(Punct::Comma) {
            let mut items = Vec::from([first]);
            while self.at_punct(Punct::Comma) {
                let comma = self.advance();
                if self.at_keyword(Keyword::In) {
                    return Err(Fault::new(comma, TRAILING));
                }
                items.push(self.primary()?);
            }
            let span = items[0].span.to(items[items.len() - 1].span);
            Expr {
                kind: ExprKind::Tuple(items),
                span,
            }
        } else {
            first
        };
        if !self.at_keyword(Keyword::In) {
            return Err(self.unexpected("`in`, then what to iterate"));
        }
        self.advance();
        Ok((target(variables)?, self.binary(OR)?))
    }
}

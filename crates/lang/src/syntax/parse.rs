//! The parser: a file's tokens into a [`Module`], by the grammar in [`crate::syntax`].

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

use super::lex::{Keyword, Punct, Token, TokenKind, lex};
use super::{
    Args, Arithmetic, Branch, Clause, Comprehension, Def, Expr, ExprKind, Load, LoadName, Logic,
    Module, Name, Operator, Param, Simple, Statement, Step, Target, Unary,
};
use crate::problem::{Fault, refusal};
use crate::value::{FileId, Span};
use crate::{Label, MAX_NESTING};

/// The module that `text` is, or the fault at its first refused token.
pub(crate) fn parse(file: FileId, text: &str) -> Result<Module, Fault> {
    let tokens = lex(file, text);
    let mut parser = Parser {
        text,
        tokens,
        pos: 0,
        depth: 0,
        deepest: 0,
    };
    parser.module()
}

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

const COMPREHEND: &str = "build the list with a comprehension, as `[f(x) for x in xs]`";

const TRAILING: &str = "a tuple without parentheses can't end with `,`: drop the `,`, or put the \
                        tuple in parentheses";

// Where a statement stands: a file's top level, or a function's body.
#[derive(Clone, Copy)]
enum Level {
    Top,
    Body,
}

// What `left_out` checks a token against: where in a line or an expression it stands.
#[derive(Clone, Copy)]
enum At {
    /// The start of a line's first statement.
    Line(Level),
    /// The start of a small statement after a `;`, or on a block's header line.
    Small(Level),
    /// After an assignment's target.
    Target,
    /// Where an operand starts.
    Operand,
    /// After an operand.
    Operator,
}

struct Parser<'t> {
    text: &'t str,
    /// Never empty: the last token is the end of the file, or an error.
    tokens: Vec<Token>,
    pos: usize,
    /// How deep the parts being read nest in the file, as `MAX_NESTING` counts.
    depth: usize,
    /// The deepest `depth` has been in the body of the `def` being read.
    deepest: usize,
}

enum Argument {
    Positional(Expr),
    Keyword(Name, Expr),
}

impl Parser<'_> {
    fn module(&mut self) -> Result<Module, Fault> {
        let mut module = Module {
            loads: Vec::new(),
            statements: Vec::new(),
        };
        while !matches!(self.peek().kind, TokenKind::Eof) {
            if self.at_keyword(Keyword::Def) {
                let def = self.def()?;
                module.statements.push(Statement::Def(Rc::new(def)));
                continue;
            }
            self.line(At::Line(Level::Top), |parser| {
                if parser.at_keyword(Keyword::Load) {
                    module.loads.push(parser.load()?);
                } else {
                    module.statements.push(Statement::Simple(parser.simple()?));
                }
                Ok(())
            })?;
        }
        Ok(module)
    }

    // A line of statements that `small` reads, separated by `;`, and its line break.
    fn line(
        &mut self,
        mut at: At,
        mut small: impl FnMut(&mut Self) -> Result<(), Fault>,
    ) -> Result<(), Fault> {
        loop {
            if let Some(message) = left_out(&self.peek().kind, at) {
                return Err(Fault::new(self.peek().span, message));
            }
            small(self)?;
            if let At::Line(level) = at {
                at = At::Small(level);
            }
            if !self.eat_punct(Punct::Semicolon) || matches!(self.peek().kind, TokenKind::Newline) {
                break;
            }
        }
        if matches!(self.peek().kind, TokenKind::Newline) {
            self.advance();
            Ok(())
        } else {
            Err(self.unexpected("the end of the line"))
        }
    }

    fn steps_line(&mut self, at: At, steps: &mut Vec<Step>) -> Result<(), Fault> {
        self.line(at, |parser| {
            steps.push(parser.step()?);
            Ok(())
        })
    }

    fn simple(&mut self) -> Result<Simple, Fault> {
        if self.at_keyword(Keyword::Pass) {
            return Ok(Simple::Pass(self.advance()));
        }
        let expr = self.expressions()?;
        if self.at_punct(Punct::Colon) {
            return Err(annotation(self.peek().span, ":"));
        }
        if let Some(message) = left_out(&self.peek().kind, At::Target) {
            return Err(Fault::new(self.peek().span, message));
        }
        if !self.eat_punct(Punct::Eq) {
            return Ok(Simple::Expr(expr));
        }
        let target = target(expr)?;
        let value = self.expressions()?;
        Ok(Simple::Assign { target, value })
    }

    fn step(&mut self) -> Result<Step, Fault> {
        if !self.at_keyword(Keyword::Return) {
            return Ok(Step::Simple(self.simple()?));
        }
        let keyword = self.advance();
        let value = if matches!(
            self.peek().kind,
            TokenKind::Newline | TokenKind::Punct(Punct::Semicolon)
        ) {
            None
        } else {
            Some(self.expressions()?)
        };
        Ok(Step::Return { keyword, value })
    }

    fn def(&mut self) -> Result<Def, Fault> {
        let keyword = self.advance();
        let name = self.name("the function's name")?;
        let open = self.expect_punct(Punct::LeftParen, "`(`")?;
        let (params, positional) = self.nested(open, Self::params)?;
        if self.at_punct(Punct::Minus)
            && matches!(self.peek_next(), Some(TokenKind::Punct(Punct::Greater)))
        {
            return Err(annotation(self.peek().span, "->"));
        }
        // The parameters' defaults run where the `def` does, so only the body's depth is what a
        // call of the function adds.
        self.deepest = 0;
        let body = self.suite(keyword)?;
        let locals = locals(&params, &body);
        Ok(Def {
            keyword,
            name,
            params,
            positional,
            body,
            locals,
            depth: self.deepest,
        })
    }

    fn params(&mut self) -> Result<(Vec<Param>, usize), Fault> {
        let mut params: Vec<Param> = Vec::new();
        let mut named: BTreeMap<String, Span> = BTreeMap::new();
        // The bare `*`, and how many parameters came before it.
        let mut star: Option<(Span, usize)> = None;
        // The first parameter before any `*` that has a default.
        let mut defaulted: Option<String> = None;
        while !self.at_punct(Punct::RightParen) {
            if self.at_punct(Punct::Star) || self.at_punct(Punct::StarStar) {
                star = Some((self.star(star.is_some())?, params.len()));
            } else {
                let param = self.param()?;
                let Name { text, span } = &param.name;
                if let Some(first) = named.insert(text.clone(), *span) {
                    let message = format!("`{text}` is a parameter already: name each once");
                    let fault = Fault::new(*span, message);
                    return Err(fault.note("it is first named at", Some(first)));
                }
                // Past the `*` each parameter is keyword-only, so defaults may come in any order.
                if star.is_none() {
                    match (&param.default, &defaulted) {
                        (Some(_), None) => defaulted = Some(text.clone()),
                        (None, Some(first)) => {
                            let message = format!(
                                "`{text}` has no default, and follows `{first}`, which has one: \
                                 give it a default, or put it before `{first}`"
                            );
                            return Err(Fault::new(*span, message));
                        }
                        (Some(_), Some(_)) | (None, None) => {}
                    }
                }
                params.push(param);
            }
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::RightParen, "`,` or `)`")?;
        let positional = match star {
            None => params.len(),
            Some((at, before)) if before == params.len() => {
                let message = "no parameter follows this `*`: name a keyword-only one after it, \
                               or drop it";
                return Err(Fault::new(at, message));
            }
            Some((_, before)) => before,
        };
        Ok((params, positional))
    }

    fn star(&mut self, again: bool) -> Result<Span, Fault> {
        if self.at_punct(Punct::StarStar) {
            let message = refusal(
                "`**kwargs`, which would let a misspelt keyword pass unseen,",
                "name each parameter",
            );
            return Err(Fault::new(self.peek().span, message));
        }
        let span = self.advance();
        let message = if matches!(self.peek().kind, TokenKind::Name(_)) {
            refusal("`*args`", "take the values in a list")
        } else if again {
            String::from("this is a second `*`: mark the keyword-only parameters with one")
        } else {
            return Ok(span);
        };
        Err(Fault::new(span, message))
    }

    fn param(&mut self) -> Result<Param, Fault> {
        let name = self.name("a parameter's name, or `*`")?;
        if self.at_punct(Punct::Colon) {
            return Err(annotation(self.peek().span, ":"));
        }
        let default = if self.eat_punct(Punct::Eq) {
            Some(self.test()?)
        } else {
            None
        };
        Ok(Param { name, default })
    }

    // A block's `:`, then its statements one level deeper than the header at `header`: an indented
    // block, or statements on the header's own line.
    fn suite(&mut self, header: Span) -> Result<Vec<Step>, Fault> {
        self.expect_punct(Punct::Colon, "`:`")?;
        self.nested(header, |parser| {
            let mut steps = Vec::new();
            if !matches!(parser.peek().kind, TokenKind::Newline) {
                parser.steps_line(At::Small(Level::Body), &mut steps)?;
                return Ok(steps);
            }
            parser.advance();
            if !matches!(parser.peek().kind, TokenKind::Indent) {
                return Err(parser.unexpected("the block's lines, indented"));
            }
            parser.advance();
            while !matches!(parser.peek().kind, TokenKind::Outdent) {
                if parser.at_keyword(Keyword::If) {
                    steps.push(parser.if_statement()?);
                } else {
                    parser.steps_line(At::Line(Level::Body), &mut steps)?;
                }
            }
            parser.advance();
            Ok(steps)
        })
    }

    fn if_statement(&mut self) -> Result<Step, Fault> {
        let mut branches = Vec::new();
        loop {
            let keyword = self.advance();
            let condition = self.test()?;
            let steps = self.suite(keyword)?;
            branches.push(Branch { condition, steps });
            if !self.at_keyword(Keyword::Elif) {
                break;
            }
        }
        let otherwise = if self.at_keyword(Keyword::Else) {
            let keyword = self.advance();
            self.suite(keyword)?
        } else {
            Vec::new()
        };
        Ok(Step::If {
            branches,
            otherwise,
        })
    }

    fn load(&mut self) -> Result<Load, Fault> {
        let keyword = self.advance();
        self.expect_punct(Punct::LeftParen, "`(`")?;
        let (text, at) = self.string("the label of the file to load, as `\"//lib/x.star\"`")?;
        let label = Label::parse(&text).map_err(|message| Fault::new(at, message))?;
        #[expect(
            clippy::case_sensitive_file_extension_comparisons,
            reason = "a label is case-sensitive, as the path it names is"
        )]
        if !label.path().ends_with(".star") {
            let message = format!("`{label}` names no `.star` file: load a `.star` file");
            return Err(Fault::new(at, message));
        }
        let mut names = Vec::new();
        while self.eat_punct(Punct::Comma) && !self.at_punct(Punct::RightParen) {
            let alias = match (&self.peek().kind, self.peek_next()) {
                (TokenKind::Name(alias), Some(TokenKind::Punct(Punct::Eq))) => Some(alias.clone()),
                _ => None,
            };
            let local = alias.map(|alias| {
                let span = self.advance();
                self.advance();
                Name { text: alias, span }
            });
            let (text, span) = self.string("a name to load, as `\"x\"` or `y = \"x\"`")?;
            let remote = Name { text, span };
            let local = local.unwrap_or_else(|| Name {
                text: remote.text.clone(),
                span,
            });
            names.push(LoadName { local, remote });
        }
        self.expect_punct(Punct::RightParen, "`,` or `)`")?;
        if names.is_empty() {
            let message = "this `load` binds no name: name one, as `load(\"//lib/x.star\", \"x\")`";
            return Err(Fault::new(keyword, message));
        }
        Ok(Load {
            keyword,
            label,
            at,
            names,
        })
    }

    fn expressions(&mut self) -> Result<Expr, Fault> {
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

    fn test(&mut self) -> Result<Expr, Fault> {
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
        for argument in arguments {
            match argument {
                Argument::Positional(arg) if keyword.is_empty() => positional.push(arg),
                Argument::Positional(arg) => {
                    let message = "a positional argument follows a keyword argument: put \
                                   positional arguments first";
                    return Err(Fault::new(arg.span, message));
                }
                Argument::Keyword(name, _) if keyword.iter().any(|(n, _)| n.text == name.text) => {
                    let message = format!("`{}` is given twice: give it once", name.text);
                    return Err(Fault::new(name.span, message));
                }
                Argument::Keyword(name, arg) => keyword.push((name, arg)),
            }
        }
        let args = Args {
            positional,
            keyword,
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
                let message = refusal(
                    "`**` before an argument, which passes a dict's entries,",
                    "name each keyword argument",
                );
                return Err(Fault::new(self.peek().span, message));
            }
            _ => {}
        }
        if let (TokenKind::Name(name), Some(TokenKind::Punct(Punct::Eq))) =
            (&self.peek().kind, self.peek_next())
        {
            let text = name.clone();
            let span = self.advance();
            self.advance();
            return Ok(Argument::Keyword(Name { text, span }, self.test()?));
        }
        self.test().map(Argument::Positional)
    }

    fn operand(&mut self) -> Result<Expr, Fault> {
        let span = self.peek().span;
        let kind = match &self.peek().kind {
            TokenKind::Name(name) => ExprKind::Name(name.clone()),
            TokenKind::Int(int) => ExprKind::Int(*int),
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

    fn after<T>(
        &mut self,
        first: T,
        close: Punct,
        item: impl FnMut(&mut Self) -> Result<T, Fault>,
    ) -> Result<(Vec<T>, Span), Fault> {
        let mut items = Vec::from([first]);
        if !self.eat_punct(Punct::Comma) {
            let close = self.expect_punct(close, &format!("`,` or `{}`", close.text()))?;
            return Ok((items, close));
        }
        let (rest, close) = self.sequence(close, item)?;
        items.extend(rest);
        Ok((items, close))
    }

    fn sequence<T>(
        &mut self,
        close: Punct,
        mut item: impl FnMut(&mut Self) -> Result<T, Fault>,
    ) -> Result<(Vec<T>, Span), Fault> {
        let mut items = Vec::new();
        while !self.at_punct(close) {
            items.push(item(self)?);
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        let close = self.expect_punct(close, &format!("`,` or `{}`", close.text()))?;
        Ok((items, close))
    }

    // Reads `inner` one level deeper, and restores the depth.
    fn nested<T>(
        &mut self,
        at: Span,
        inner: impl FnOnce(&mut Self) -> Result<T, Fault>,
    ) -> Result<T, Fault> {
        let depth = self.depth;
        self.nest(at)?;
        let read = inner(self);
        self.depth = depth;
        read
    }

    // Goes one level deeper, refused past `MAX_NESTING`. The caller restores the depth.
    fn nest(&mut self, at: Span) -> Result<(), Fault> {
        self.depth += 1;
        self.deepest = self.deepest.max(self.depth);
        if self.depth <= MAX_NESTING {
            return Ok(());
        }
        let message = format!(
            "this nests {} deep: nest expressions and blocks at most {MAX_NESTING} deep, binding \
             inner parts to names",
            self.depth
        );
        Err(Fault::new(at, message))
    }

    fn name(&mut self, expected: &str) -> Result<Name, Fault> {
        let TokenKind::Name(text) = &self.peek().kind else {
            return Err(self.unexpected(expected));
        };
        let text = text.clone();
        Ok(Name {
            text,
            span: self.advance(),
        })
    }

    fn string(&mut self, expected: &str) -> Result<(String, Span), Fault> {
        let TokenKind::Str(text) = &self.peek().kind else {
            return Err(self.unexpected(expected));
        };
        let text = String::from(&**text);
        Ok((text, self.advance()))
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn advance(&mut self) -> Span {
        let span = self.peek().span;
        // Stays on `Eof`, the last token, which a failed read can then name.
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        span
    }

    fn peek_next(&self) -> Option<&TokenKind> {
        self.tokens.get(self.pos + 1).map(|token| &token.kind)
    }

    fn at_keyword(&self, keyword: Keyword) -> bool {
        matches!(self.peek().kind, TokenKind::Keyword(found) if found == keyword)
    }

    fn at_punct(&self, punct: Punct) -> bool {
        matches!(self.peek().kind, TokenKind::Punct(found) if found == punct)
    }

    fn eat_punct(&mut self, punct: Punct) -> bool {
        let at = self.at_punct(punct);
        if at {
            self.advance();
        }
        at
    }

    fn expect_punct(&mut self, punct: Punct, expected: &str) -> Result<Span, Fault> {
        if self.at_punct(punct) {
            Ok(self.advance())
        } else {
            Err(self.unexpected(expected))
        }
    }

    fn unexpected(&self, expected: &str) -> Fault {
        let Token { kind, span } = self.peek();
        let found = match kind {
            // The lexer's refusal ends the tokens, so it is the first fault.
            TokenKind::Error(message) => return Fault::new(*span, message.clone()),
            TokenKind::Name(_)
            | TokenKind::Int(_)
            | TokenKind::Keyword(_)
            | TokenKind::Punct(_) => {
                format!("`{}`", &self.text[span.start..span.end])
            }
            TokenKind::Str(_) => String::from("string"),
            TokenKind::Newline => String::from("end of line"),
            TokenKind::Indent => String::from("indentation"),
            TokenKind::Outdent => String::from("end of block"),
            TokenKind::Eof => String::from("end of file"),
        };
        Fault::new(*span, format!("unexpected {found}: expected {expected}"))
    }
}

// The problem with a token met at `at`, where the parser takes no token of its kind, if it has
// one: a refusal where Starlark's grammar takes the token and the subset leaves it out, or
// Starlark's own error of a statement that stands elsewhere. Any other such token is only
// unexpected, so that a file that is no Starlark is never told that it is only outside the subset.
#[expect(clippy::too_many_lines, reason = "one table of the parser's places")]
fn left_out(kind: &TokenKind, at: At) -> Option<String> {
    let message = match (kind, at) {
        (TokenKind::Indent, At::Line(Level::Top)) => String::from(
            "this line is indented, and no block holds it: start it at its line's start",
        ),
        (TokenKind::Indent, At::Line(Level::Body)) => String::from(
            "this line is indented more than the block it is in: indent it as the line above",
        ),
        (TokenKind::Keyword(Keyword::If), At::Line(Level::Top)) => String::from(
            "an `if` statement stands only in a function's body: choose a value with `a if c else \
             b`, or move the statement into a `def`",
        ),
        (TokenKind::Keyword(Keyword::For), At::Line(Level::Top)) => {
            format!("a `for` statement at a file's top level is not in Starlark: {COMPREHEND}")
        }
        (TokenKind::Keyword(Keyword::Def), At::Line(Level::Body)) => refusal(
            "a `def` inside a function",
            "write it at the top level, passing it what it uses",
        ),
        (TokenKind::Keyword(Keyword::For), At::Line(Level::Body)) => {
            refusal("a `for` statement", COMPREHEND)
        }
        (TokenKind::Keyword(Keyword::Return), At::Line(Level::Top) | At::Small(Level::Top)) => {
            String::from("`return` stands only in a function's body: move it into a `def`")
        }
        (TokenKind::Keyword(Keyword::Load), At::Line(Level::Body) | At::Small(Level::Body)) => {
            String::from(
                "a `load` stands only at a file's top level, above its other statements: move it \
                 there",
            )
        }
        (
            TokenKind::Keyword(keyword @ (Keyword::Break | Keyword::Continue)),
            At::Line(_) | At::Small(_),
        ) => format!(
            "`{}` stands only in a loop's body, and no loop holds it: filter a comprehension with \
             `if`",
            keyword.text()
        ),
        (
            TokenKind::Punct(
                punct @ (Punct::PlusEq
                | Punct::MinusEq
                | Punct::StarEq
                | Punct::SlashEq
                | Punct::SlashSlashEq
                | Punct::PercentEq
                | Punct::AmpEq
                | Punct::PipeEq
                | Punct::CaretEq
                | Punct::LessLessEq
                | Punct::GreaterGreaterEq),
            ),
            At::Target,
        ) => refusal(
            format_args!("`{}`", punct.text()),
            "bind the result with `=`",
        ),
        (TokenKind::Punct(Punct::Plus), At::Operand) => refusal("unary `+`", "write the int alone"),
        (TokenKind::Keyword(Keyword::Lambda), At::Operand) => refusal(
            "`lambda`",
            "write a `def` at a library's top level, and pass its name",
        ),
        (TokenKind::Punct(punct @ Punct::Tilde), At::Operand)
        | (
            TokenKind::Punct(
                punct @ (Punct::Amp | Punct::Caret | Punct::LessLess | Punct::GreaterGreater),
            ),
            At::Operator,
        ) => refusal(
            format_args!("`{}`, a bitwise operator,", punct.text()),
            "write the int it gives",
        ),
        (TokenKind::Punct(Punct::Slash), At::Operator) => {
            refusal("`/`, which divides into a float,", "divide with `//`")
        }
        (TokenKind::Punct(Punct::StarStar), At::Operator) => String::from(
            "`**` is no operator of Starlark's: multiply, or use `KiB`, `MiB` or `GiB`",
        ),
        _ => return None,
    };
    Some(message)
}

fn annotation(at: Span, mark: &str) -> Fault {
    let message = format!("type annotations are not in Starlark: drop the `{mark}` and the type");
    Fault::new(at, message)
}

fn locals(params: &[Param], body: &[Step]) -> Vec<String> {
    fn assigned<'s>(steps: &'s [Step], names: &mut Vec<&'s Name>) {
        for step in steps {
            match step {
                Step::Simple(Simple::Assign { target, .. }) => target.names(names),
                Step::Simple(Simple::Expr(_) | Simple::Pass(_)) | Step::Return { .. } => {}
                Step::If {
                    branches,
                    otherwise,
                } => {
                    for branch in branches {
                        assigned(&branch.steps, names);
                    }
                    assigned(otherwise, names);
                }
            }
        }
    }
    let mut names: Vec<&Name> = params.iter().map(|param| &param.name).collect();
    assigned(body, &mut names);
    let mut seen = BTreeSet::new();
    let names = names.into_iter().map(|name| &name.text);
    names
        .filter(|name| seen.insert(name.as_str()))
        .cloned()
        .collect()
}

// An assignment's left side is read as an expression, before its `=` shows that it is one.
fn target(expr: Expr) -> Result<Target, Fault> {
    let Expr { kind, span } = expr;
    let what = match kind {
        ExprKind::Name(text) => return Ok(Target::Name(Name { text, span })),
        ExprKind::Tuple(items) | ExprKind::List(items) => {
            let targets = items.into_iter().map(target).collect::<Result<_, _>>()?;
            return Ok(Target::Each { targets, span });
        }
        ExprKind::Index { .. } => {
            let message = refusal(
                "assigning to an element, which would change a value,",
                "make a new value, as `d | {k: v}` makes a dict",
            );
            return Err(Fault::new(span, message));
        }
        ExprKind::Int(_) => "an int",
        ExprKind::Str(_) => "a string",
        ExprKind::Dict(_) => "a dict",
        ExprKind::ListComp { .. } | ExprKind::DictComp { .. } => "a comprehension",
        ExprKind::Call { .. } | ExprKind::Method { .. } => "a call",
        ExprKind::Unary { .. }
        | ExprKind::Logic { .. }
        | ExprKind::Binary { .. }
        | ExprKind::Conditional { .. } => "an operation",
    };
    let message = format!("{what} can't be assigned to: assign to a name, or to names in a tuple");
    Err(Fault::new(span, message))
}

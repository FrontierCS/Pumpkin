//! The parser: a file's tokens into a [`Module`], by the grammar in [`crate::syntax`].

mod expr;

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

use super::lex::{Keyword, Punct, Token, TokenKind, lex};
use super::{
    Branch, Def, Expr, ExprKind, KeyType, Load, LoadName, Module, Name, Param, Simple, Statement,
    Step, Target, Type,
};
use crate::problem::{Fault, refusal};
use crate::value::{FileId, Hosted, Span};
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

const COMPREHEND: &str = "build the list with a comprehension, as `[f(x) for x in xs]`";

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
            let message = refusal("an annotation of a variable", "drop the `:` and the type");
            return Err(Fault::new(self.peek().span, message));
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
        let returns = if self.eat_punct(Punct::MinusGreater) {
            Some(self.type_expr()?)
        } else {
            None
        };
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
            returns,
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
        let message = if matches!(self.peek().kind, TokenKind::Name) {
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
        let annotation = if self.eat_punct(Punct::Colon) {
            Some(self.type_expr()?)
        } else {
            None
        };
        let default = if self.eat_punct(Punct::Eq) {
            Some(self.test()?)
        } else {
            None
        };
        Ok(Param {
            name,
            annotation,
            default,
        })
    }

    fn type_expr(&mut self) -> Result<Type, Fault> {
        let name = self.name("a type, as `str` or `list[int]`")?;
        // A name of this closed set, resolved as Bazel resolves one against its universe of types,
        // not evaluated to a value as starlark-rust does, so that `list` and `dict`, which the
        // subset has no value for, name a type only with the types they hold:
        // https://github.com/bazelbuild/bazel/blob/a765dc177f48efb4c9e1b980609b838c9ba27f42/src/main/java/net/starlark/java/eval/Module.java#L267-L305
        let found = match name.text.as_str() {
            "None" => Type::None,
            "bool" => Type::Bool,
            "int" => Type::Int,
            "float" => Type::Float,
            "str" => Type::Str,
            // The host's, named as Buck2 names its context's type, `ctx: AnalysisContext`:
            // https://github.com/facebook/buck2/blob/e3dc98f84acf5f70c40b857819556baa7ec9c929/app/buck2_build_api/src/interpreter/rule_defs/context.rs#L396-L398
            "Target" => Type::Hosted(Hosted::Target),
            "Run" => Type::Hosted(Hosted::Run),
            "Payload" => Type::Hosted(Hosted::Payload),
            "list" => {
                let takes = "its elements' type: write `list[str]`";
                let element = self.type_arguments(&name, takes, Self::type_expr)?;
                Type::List(Box::new(element))
            }
            "dict" => {
                let takes = "its keys' and values' types: write `dict[str, int]`";
                let (key, value) = self.type_arguments(&name, takes, |parser| {
                    let key = parser.key_type()?;
                    parser.expect_punct(Punct::Comma, "`,` and the values' type")?;
                    let value = parser.type_expr()?;
                    Ok((key, value))
                })?;
                Type::Dict(key, Box::new(value))
            }
            text => {
                let message = format!(
                    "`{text}` is no type: give `None`, `bool`, `int`, `float`, `str`, `list[…]`, \
                     `dict[…, …]`, `Target`, `Run` or `Payload`"
                );
                return Err(Fault::new(name.span, message));
            }
        };
        if self.at_punct(Punct::Pipe) {
            let message = refusal("`|` in a type", "take one type");
            return Err(Fault::new(self.peek().span, message));
        }
        Ok(found)
    }

    // A dict's key type, refused unless it is one that a key can be.
    fn key_type(&mut self) -> Result<KeyType, Fault> {
        let at = self.peek().span;
        match self.type_expr()? {
            Type::Int => Ok(KeyType::Int),
            Type::Float => Ok(KeyType::Float),
            Type::Str => Ok(KeyType::Str),
            other
            @ (Type::None | Type::Bool | Type::List(_) | Type::Dict(..) | Type::Hosted(_)) => {
                let message = format!(
                    "a dict's keys can't be {}: key a dict with strings or numbers, as `str`, \
                     `int` or `float`",
                    other.plural()
                );
                Err(Fault::new(at, message))
            }
        }
    }

    // The types that `inner` reads in the brackets after `name`, refused without them, in a
    // message that ends in `takes`: what `name` takes, and how to write it.
    fn type_arguments<T>(
        &mut self,
        name: &Name,
        takes: &str,
        inner: impl FnOnce(&mut Self) -> Result<T, Fault>,
    ) -> Result<T, Fault> {
        if !self.at_punct(Punct::LeftBracket) {
            let message = format!("`{}` takes {takes}", name.text);
            return Err(Fault::new(name.span, message));
        }
        let open = self.advance();
        let depth = self.depth;
        // A type can't be bound to a name, as a part of an expression can, so its depth alone is
        // what its writer can change.
        self.deeper().map_err(|deep| {
            let message = format!("this nests {deep} deep: nest a type at most {MAX_NESTING} deep");
            Fault::new(open, message)
        })?;
        let arguments = inner(self);
        self.depth = depth;
        let arguments = arguments?;
        self.expect_punct(Punct::RightBracket, "`]`")?;
        Ok(arguments)
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
            let local = self.keyword_name();
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
        self.deeper().map_err(|deep| {
            let message = format!(
                "this nests {deep} deep: nest expressions and blocks at most {MAX_NESTING} deep, \
                 binding inner parts to names"
            );
            Fault::new(at, message)
        })
    }

    // Goes one level deeper, or else gives the depth past `MAX_NESTING`. The caller restores the
    // depth.
    fn deeper(&mut self) -> Result<(), usize> {
        self.depth += 1;
        self.deepest = self.deepest.max(self.depth);
        if self.depth <= MAX_NESTING {
            return Ok(());
        }
        Err(self.depth)
    }

    fn keyword_name(&mut self) -> Option<Name> {
        let (TokenKind::Name, Some(TokenKind::Punct(Punct::Eq))) =
            (&self.peek().kind, self.peek_next())
        else {
            return None;
        };
        let text = self.spelling().into();
        let span = self.advance();
        self.advance();
        Some(Name { text, span })
    }

    fn name(&mut self, expected: &str) -> Result<Name, Fault> {
        let TokenKind::Name = self.peek().kind else {
            return Err(self.unexpected(expected));
        };
        let text = self.spelling().into();
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

    fn spelling(&self) -> &str {
        let span = self.peek().span;
        &self.text[span.start..span.end]
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
            TokenKind::Error(message) => return Fault::new(*span, String::from(message.as_ref())),
            TokenKind::Name
            | TokenKind::Int(_)
            | TokenKind::Float(_)
            | TokenKind::Keyword(_)
            | TokenKind::Punct(_) => {
                format!("`{}`", self.spelling())
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
        (TokenKind::Punct(Punct::Plus), At::Operand) => {
            refusal("unary `+`", "write the number alone")
        }
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
        (TokenKind::Punct(Punct::StarStar), At::Operator) => String::from(
            "`**` is no operator of Starlark's: multiply, or use `KiB`, `MiB` or `GiB`",
        ),
        _ => return None,
    };
    Some(message)
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
        ExprKind::Float(_) => "a float",
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

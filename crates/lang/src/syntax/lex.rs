//! A file's tokens, as the specification has them (spec.md, "Lexical elements"), but for what
//! the subset refuses at its text:
//! - a name is ASCII letters, digits and `_`, not starting with a digit, where the specification
//!   also allows Unicode letters;
//! - an int is decimal, `0x` or `0o`, and at most 2^63 - 1, since ints are 64-bit;
//! - floats, bytes literals and f-strings, which have no value in the subset;
//! - a `\` before a quote in a raw string, which Starlark's implementations read differently;
//! - a `\x`, `\u` or `\U` escape, which some of them lack, and an octal one over 127, which they
//!   read differently;
//! - a carriage return that doesn't end a line with the `\n` after it;
//! - a reserved word, wherever it appears;
//! - a tab outside a string or a comment, but on a line outside brackets that holds no token:
//!   indentation ("Special tokens") and the white space between tokens are spaces.
//!
//! A line's indentation is counted only outside brackets, inside which lines join.

use alloc::format;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::problem::refusal;
use crate::value::{FileId, Span};

pub(crate) struct Token {
    pub(crate) kind: TokenKind,
    pub(crate) span: Span,
}

pub(crate) enum TokenKind {
    Name(String),
    Int(i64),
    Str(Rc<str>),
    Keyword(Keyword),
    Punct(Punct),
    /// The end of a line outside brackets that holds a token.
    Newline,
    Indent,
    Outdent,
    Eof,
    /// Text that is no token, with the message that says why. It is the last token.
    Error(String),
}

/// Defines `$kind`, an enum of tokens, each written as its text: `text()`, and `ALL`, each token
/// in the order listed.
macro_rules! tokens {
    ($(#[$doc:meta])* $kind:ident { $($token:ident = $text:literal,)* }) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq)]
        pub(crate) enum $kind {
            $($token,)*
        }

        impl $kind {
            const ALL: &[Self] = &[$(Self::$token,)*];

            pub(crate) fn text(self) -> &'static str {
                match self {
                    $(Self::$token => $text,)*
                }
            }
        }
    };
}

tokens! {
    /// The specification's keywords.
    Keyword {
        And = "and", Break = "break", Continue = "continue", Def = "def", Elif = "elif",
        Else = "else", For = "for", If = "if", In = "in", Lambda = "lambda", Load = "load",
        Not = "not", Or = "or", Pass = "pass", Return = "return",
    }
}

tokens! {
    /// The specification's punctuation, the longest first, so that the first that matches is the
    /// token.
    Punct {
        SlashSlashEq = "//=", LessLessEq = "<<=", GreaterGreaterEq = ">>=", StarStar = "**",
        SlashSlash = "//", LessLess = "<<", GreaterGreater = ">>", LessEq = "<=", GreaterEq = ">=",
        EqEq = "==", BangEq = "!=", PlusEq = "+=", MinusEq = "-=", StarEq = "*=", SlashEq = "/=",
        PercentEq = "%=", AmpEq = "&=", PipeEq = "|=", CaretEq = "^=", Plus = "+", Minus = "-",
        Star = "*", Slash = "/", Percent = "%", Tilde = "~", Amp = "&", Pipe = "|", Caret = "^",
        Dot = ".", Comma = ",", Eq = "=", Semicolon = ";", Colon = ":", LeftParen = "(",
        RightParen = ")", LeftBracket = "[", RightBracket = "]", LeftBrace = "{", RightBrace = "}",
        Less = "<", Greater = ">",
    }
}

/// The words the specification reserves, which are neither keywords nor names.
const RESERVED: [&str; 18] = [
    "as", "assert", "async", "await", "class", "del", "except", "finally", "from", "global",
    "import", "is", "nonlocal", "raise", "try", "while", "with", "yield",
];

/// The tokens of `text`, the text of `file`, ending with [`TokenKind::Eof`], or with
/// [`TokenKind::Error`] at the first text that is no token.
pub(crate) fn lex(file: FileId, text: &str) -> Vec<Token> {
    let mut lexer = Lexer {
        file,
        text,
        pos: 0,
        tokens: Vec::new(),
        indents: Vec::new(),
        depth: 0,
        line_start: true,
    };
    match lexer.run() {
        Ok(()) => lexer.finish(),
        Err((start, message)) => {
            let span = lexer.span(start);
            lexer.tokens.push(Token {
                kind: TokenKind::Error(message),
                span,
            });
        }
    }
    lexer.tokens
}

/// Whether `text` is a name, as a keyword argument's is.
pub(crate) fn is_name(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(starts_name) && chars.all(in_name)
}

/// Whether a name may start with `char`: an ASCII letter or `_`.
fn starts_name(char: char) -> bool {
    char.is_ascii_alphabetic() || char == '_'
}

/// Whether a name may hold `char`: an ASCII letter, a digit or `_`.
fn in_name(char: char) -> bool {
    char.is_ascii_alphanumeric() || char == '_'
}

/// Where text that is no token starts, and the message that says why.
type LexError = (usize, String);

struct Lexer<'t> {
    file: FileId,
    text: &'t str,
    pos: usize,
    tokens: Vec<Token>,
    /// The indentation of each block the lexer is in, innermost last; the file's own is 0.
    indents: Vec<usize>,
    /// How many brackets are open.
    depth: usize,
    /// Whether the next token starts a line outside brackets, so its indentation counts.
    line_start: bool,
}

impl Lexer<'_> {
    fn run(&mut self) -> Result<(), LexError> {
        loop {
            if self.line_start && self.depth == 0 && !self.indentation()? {
                return Ok(());
            }
            self.skip_blanks()?;
            let start = self.pos;
            let Some(char) = self.peek() else {
                return Ok(());
            };
            if char == '\n' || char == '\r' {
                self.line_end()?;
                if self.depth == 0 {
                    self.push(TokenKind::Newline, start);
                    self.line_start = true;
                }
            } else if starts_name(char) {
                self.word(start)?;
            } else if char.is_ascii_digit() {
                self.number(start)?;
            } else if char == '\'' || char == '"' {
                let value = self.string(start, char, false)?;
                self.push(TokenKind::Str(value.into()), start);
            } else if char == '.'
                && self.text[start + 1..].starts_with(|c: char| c.is_ascii_digit())
            {
                return Err(self.float(start));
            } else {
                self.punctuation(start, char)?;
            }
        }
    }

    /// Reads the indentation of the next line that holds a token, and pushes the
    /// [`TokenKind::Indent`] or [`TokenKind::Outdent`]s it makes. Returns false at the end of
    /// the text.
    fn indentation(&mut self) -> Result<bool, LexError> {
        loop {
            let start = self.pos;
            self.skip(|char| char == ' ' || char == '\t');
            match self.peek() {
                None => return Ok(false),
                Some('\n' | '\r') => self.line_end()?,
                Some('#') => self.comment(),
                Some(_) => {
                    if let Some(tab) = self.text[start..self.pos].find('\t') {
                        let message = refusal("a tab in indentation", "indent with spaces only");
                        return Err((start + tab, message));
                    }
                    let width = self.pos - start;
                    self.line_start = false;
                    return self.indent(start, width).map(|()| true);
                }
            }
        }
    }

    fn indent(&mut self, start: usize, width: usize) -> Result<(), LexError> {
        let top = |indents: &[usize]| indents.last().copied().unwrap_or(0);
        if width > top(&self.indents) {
            self.indents.push(width);
            self.push(TokenKind::Indent, start);
            return Ok(());
        }
        while width < top(&self.indents) {
            self.indents.pop();
            self.push(TokenKind::Outdent, self.pos);
        }
        if width == top(&self.indents) {
            Ok(())
        } else {
            let message = "this line is indented as no line above it is: indent it as far as a \
                           line of the block it is in";
            Err((start, message.to_string()))
        }
    }

    /// Skips spaces and a comment, refusing a tab, and joins the next line where a `\` ends this
    /// one.
    fn skip_blanks(&mut self) -> Result<(), LexError> {
        loop {
            match self.peek() {
                Some(' ') => self.pos += 1,
                // starlark-rust refuses a tab between tokens, and so does the subset, so that
                // starlark-rust could evaluate any file the subset accepts.
                Some('\t') => {
                    let message = refusal("a tab between tokens", "separate them with spaces");
                    return Err((self.pos, message));
                }
                Some('#') => self.comment(),
                Some('\\') => {
                    let start = self.pos;
                    self.pos += 1;
                    if !matches!(self.peek(), Some('\n' | '\r')) {
                        let message = "a `\\` outside a string joins lines: end its line with it";
                        return Err((start, message.to_string()));
                    }
                    self.line_end()?;
                }
                _ => return Ok(()),
            }
        }
    }

    fn comment(&mut self) {
        self.skip(|char| !matches!(char, '\n' | '\r'));
    }

    /// Reads a line's end: `\n`, or `\r\n`.
    fn line_end(&mut self) -> Result<(), LexError> {
        if self.text[self.pos..].starts_with("\r\n") {
            self.pos += 2;
        } else if self.text[self.pos..].starts_with('\n') {
            self.pos += 1;
        } else {
            let message = refusal(
                "a carriage return that ends no line",
                "end lines with `\\n` or `\\r\\n`",
            );
            return Err((self.pos, message));
        }
        Ok(())
    }

    /// Reads a name, a keyword, a reserved word, or a string's prefix and its string.
    fn word(&mut self, start: usize) -> Result<(), LexError> {
        self.skip(in_name);
        let word = &self.text[start..self.pos];
        if let Some(quote @ ('\'' | '"')) = self.peek() {
            match word {
                "r" => {
                    let value = self.string(start, quote, true)?;
                    self.push(TokenKind::Str(value.into()), start);
                    return Ok(());
                }
                "b" | "rb" | "br" => {
                    let message = refusal(
                        format_args!("a bytes literal, which `{word}` starts,"),
                        "write a string",
                    );
                    return Err((start, message));
                }
                "f" | "rf" | "fr" => {
                    let message = format!(
                        "`{word}` starts an f-string, which Starlark lacks: join strings with `+`"
                    );
                    return Err((start, message));
                }
                _ => {}
            }
        }
        if let Some(&keyword) = Keyword::ALL.iter().find(|keyword| keyword.text() == word) {
            self.push(TokenKind::Keyword(keyword), start);
        } else if RESERVED.contains(&word) {
            let message = if word == "while" {
                String::from(
                    "`while` is not in Starlark: build the list with a comprehension, as \
                     `[f(x) for x in xs]`",
                )
            } else {
                format!("`{word}` is reserved in Starlark: use another word")
            };
            return Err((start, message));
        } else {
            self.push(TokenKind::Name(word.to_string()), start);
        }
        Ok(())
    }

    fn number(&mut self, start: usize) -> Result<(), LexError> {
        let rest = &self.text[start..];
        let radix = match rest.get(..2) {
            Some("0x" | "0X") => 16,
            Some("0o" | "0O") => 8,
            Some("0b" | "0B") => {
                self.skip(in_name);
                let literal = &self.text[start..self.pos];
                let message = format!(
                    "`{literal}` is binary, which Starlark's ints are not written in: write it \
                     in decimal, or after `0x` or `0o`"
                );
                return Err((start, message));
            }
            _ => 10,
        };
        if radix != 10 {
            self.pos += 2;
        }
        let digits_start = self.pos;
        self.digits(radix);
        let digits = &self.text[digits_start..self.pos];
        if radix == 10 && matches!(self.peek(), Some('.' | 'e' | 'E')) {
            return Err(self.float(start));
        }
        if self.peek().is_some_and(in_name) {
            let number_end = self.pos;
            self.skip(in_name);
            let literal = &self.text[start..self.pos];
            let word = &self.text[number_end..self.pos];
            // Starlark reads a keyword right after a number as a token of its own.
            let message = if Keyword::ALL.iter().any(|keyword| keyword.text() == word) {
                refusal(
                    format_args!("`{literal}`, a number and then `{word}`,"),
                    "put a space between them",
                )
            } else {
                format!(
                    "`{literal}` is no number: write digits of its base only, and a space \
                     between a number and a name"
                )
            };
            return Err((start, message));
        }
        let literal = &self.text[start..self.pos];
        if digits.is_empty() {
            let message = format!("`{literal}` has no digits: write some after it, as `0x1f`");
            return Err((start, message));
        }
        if radix == 10 && digits.len() > 1 && digits.starts_with('0') {
            let message = format!(
                "`{literal}` has a leading zero: write it without, or after `0o` if it is octal"
            );
            return Err((start, message));
        }
        let Ok(value) = i64::from_str_radix(digits, radix) else {
            let message = refusal(
                format_args!("`{literal}`, an int over 2^63 - 1,"),
                "write a smaller one, since ints are 64-bit",
            );
            return Err((start, message));
        };
        self.push(TokenKind::Int(value), start);
        Ok(())
    }

    /// Skips the characters for which `skipped` holds.
    fn skip(&mut self, skipped: impl Fn(char) -> bool) {
        let rest = &self.text[self.pos..];
        self.pos += rest.find(|char| !skipped(char)).unwrap_or(rest.len());
    }

    /// Skips the digits of `radix`.
    fn digits(&mut self, radix: u32) {
        self.skip(|char| char.is_digit(radix));
    }

    /// The refusal of the float that starts at `start`, after reading the rest of it, past its
    /// whole part: a fraction, then an exponent.
    fn float(&mut self, start: usize) -> LexError {
        if self.peek() == Some('.') {
            self.pos += 1;
            self.digits(10);
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some('+' | '-')) {
                self.pos += 1;
            }
            self.digits(10);
        }
        let literal = &self.text[start..self.pos];
        let message = refusal(format_args!("`{literal}`, a float,"), "write an int");
        (start, message)
    }

    /// Reads a string literal that starts at `start` and whose quote, `quote`, is next, and
    /// returns its value.
    fn string(&mut self, start: usize, quote: char, raw: bool) -> Result<String, LexError> {
        let three = if quote == '"' { "\"\"\"" } else { "'''" };
        let triple = self.text[self.pos..].starts_with(three);
        self.pos += if triple { 3 } else { 1 };
        let unclosed = || {
            let message = if triple {
                format!("this string is not closed: end it with `{three}`")
            } else {
                format!(
                    "this string is not closed on its line: end it with `{quote}`, or quote it \
                     with `{three}` to write several lines"
                )
            };
            (start, message)
        };
        let mut value = String::new();
        loop {
            let at = self.pos;
            match self.next() {
                None => return Err(unclosed()),
                Some(char) if char == quote => {
                    if !triple {
                        return Ok(value);
                    }
                    if self.text[at..].starts_with(three) {
                        self.pos = at + three.len();
                        return Ok(value);
                    }
                    value.push(char);
                }
                Some('\n' | '\r') => {
                    self.pos = at;
                    self.line_end()?;
                    if !triple {
                        return Err(unclosed());
                    }
                    value.push('\n');
                }
                Some('\\') if raw => self.raw_escape(at, &mut value)?,
                Some('\\') => self.escape(at, &mut value)?,
                Some(char) => value.push(char),
            }
        }
    }

    /// Reads what follows a `\` at `at` in a raw string, where both stand for themselves.
    fn raw_escape(&mut self, at: usize, value: &mut String) -> Result<(), LexError> {
        value.push('\\');
        match self.peek() {
            Some(quote @ ('\'' | '"')) => {
                let message = refusal(
                    format_args!(
                        "`\\{quote}` in a raw string, which means `{quote}` to some Starlark \
                         implementations and `\\{quote}` to others,"
                    ),
                    "quote the string with the other quote mark, or write it without `r`",
                );
                Err((at, message))
            }
            Some('\n' | '\r') => {
                self.line_end()?;
                value.push('\n');
                Ok(())
            }
            Some(char) => {
                self.pos += char.len_utf8();
                value.push(char);
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Reads the escape whose `\` is at `at` (spec.md, "String escapes"), adding what it stands
    /// for to `value`.
    fn escape(&mut self, at: usize, value: &mut String) -> Result<(), LexError> {
        let Some(char) = self.next() else {
            return Ok(());
        };
        let control = match char {
            '\n' => return Ok(()),
            '\r' => {
                self.pos -= 1;
                return self.line_end();
            }
            'a' => '\x07',
            'b' => '\x08',
            'f' => '\x0c',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'v' => '\x0b',
            '\\' | '\'' | '"' => char,
            '0'..='7' => {
                self.pos -= 1;
                return self.octal(at, value);
            }
            // Some of Starlark's implementations have no such escape, and refuse a file that
            // writes one.
            'x' | 'u' | 'U' => {
                let message = refusal(
                    format_args!("`\\{char}`, an escape that some Starlark implementations lack,"),
                    "write the character itself, or an ASCII one in octal, as `\\101` for `A`",
                );
                return Err((at, message));
            }
            _ => {
                let escape = &self.text[at..self.pos];
                // Some of Starlark's implementations keep the `\\` and the character after it.
                let message = refusal(
                    format_args!("`{escape}`, which some Starlark implementations read as itself,"),
                    "write `\\\\` for a `\\`",
                );
                return Err((at, message));
            }
        };
        value.push(control);
        Ok(())
    }

    /// Reads the 1 to 3 octal digits of an escape whose `\` is at `at`, the first of them next,
    /// and adds the character they number to `value`.
    fn octal(&mut self, at: usize, value: &mut String) -> Result<(), LexError> {
        let digits = self.text[self.pos..].chars().take(3);
        let (len, number) = (digits.map_while(|char| char.to_digit(8)))
            .fold((0, 0), |(len, number), digit| (len + 1, number * 8 + digit));
        // Each digit is ASCII, a byte.
        self.pos += len;
        // An octal escape numbers a byte, which Starlark's implementations take as a character
        // only up to 127.
        let Some(char) = char::from_u32(number).filter(char::is_ascii) else {
            let escape = &self.text[at..self.pos];
            let message = refusal(
                format_args!(
                    "`{escape}`, an octal escape over 127, which Starlark's implementations read \
                     differently,"
                ),
                "write the character itself, or an ASCII one, up to `\\177`",
            );
            return Err((at, message));
        };
        value.push(char);
        Ok(())
    }

    fn punctuation(&mut self, start: usize, char: char) -> Result<(), LexError> {
        let rest = &self.text[start..];
        let Some(&punct) = (Punct::ALL.iter()).find(|punct| rest.starts_with(punct.text())) else {
            // Some of Starlark's implementations take any Unicode letter in a name.
            let message = if char.is_alphabetic() {
                refusal(
                    format_args!("a letter outside ASCII in a name, `{char}`,"),
                    "write names in ASCII letters, digits and `_`",
                )
            } else {
                format!("`{char}` is no Starlark token: remove it")
            };
            return Err((start, message));
        };
        self.pos += punct.text().len();
        match punct {
            Punct::LeftParen | Punct::LeftBracket | Punct::LeftBrace => self.depth += 1,
            Punct::RightParen | Punct::RightBracket | Punct::RightBrace => {
                self.depth = self.depth.saturating_sub(1);
            }
            _ => {}
        }
        self.push(TokenKind::Punct(punct), start);
        Ok(())
    }

    /// Ends the tokens: a line's end if the last line lacks one, the end of each block, and
    /// the end of the file.
    fn finish(&mut self) {
        // Inside a bracket, the line goes on: the parser finds the end of the file there.
        let last_line_open =
            (self.tokens.last()).is_some_and(|token| !matches!(token.kind, TokenKind::Newline));
        if last_line_open && self.depth == 0 {
            self.push(TokenKind::Newline, self.pos);
        }
        for _ in 0..self.indents.len() {
            self.push(TokenKind::Outdent, self.pos);
        }
        self.push(TokenKind::Eof, self.pos);
    }

    fn peek(&self) -> Option<char> {
        self.text[self.pos..].chars().next()
    }

    fn next(&mut self) -> Option<char> {
        let char = self.peek()?;
        self.pos += char.len_utf8();
        Some(char)
    }

    /// The span from `start` to where the lexer is, or the first character's at `start` when
    /// the lexer is there.
    fn span(&self, start: usize) -> Span {
        let end = if self.pos > start {
            self.pos
        } else {
            let rest = &self.text[start..];
            start + rest.chars().next().map_or(0, char::len_utf8)
        };
        Span {
            file: self.file,
            start,
            end,
        }
    }

    fn push(&mut self, kind: TokenKind, start: usize) {
        let span = self.span(start);
        self.tokens.push(Token { kind, span });
    }
}

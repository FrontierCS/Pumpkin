//! Values that deserialization checks, so that each one in an IR or a sandbox protocol message
//! is well formed.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::{fmt, iter};

use serde::{Deserialize, Serialize};

/// Defines a string newtype that holds only the values `$check` accepts.
macro_rules! checked_string {
    ($(#[$attr:meta])* $name:ident, $check:ident) => {
        $(#[$attr])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(Cow<'static, str>);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = String;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                $check(&value)?;
                Ok(Self(Cow::Owned(value)))
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0.into_owned()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

checked_string!(
    /// The name of a role, slot or grant, a class of sandbox, or a sandbox within its run: 1 to
    /// 64 characters from a-z, 0-9, `_` and `-`, starting with a letter.
    Name,
    check_name
);

checked_string!(
    /// A workflow's name, such as `vendor/task`: 1 to 128 characters
    /// from A-Z, a-z, 0-9, `.`, `_`, `-` and `/`.
    WorkflowName,
    check_workflow_name
);

checked_string!(
    /// A digest-pinned image: `<repository>@sha256:<hex>`, or a local image ID,
    /// `sha256:<hex>`.
    Image,
    check_image
);

checked_string!(
    /// A content digest: `sha256:` and 64 lowercase hexadecimal digits.
    Sha256Digest,
    check_sha256_digest
);

checked_string!(
    /// An absolute path without empty, `.` or `..` components.
    AbsolutePath,
    check_absolute_path
);

checked_string!(
    /// A relative path of one or more components, none of them empty, `.` or `..`, so that it
    /// stays below the directory it is joined to.
    RelativePath,
    check_relative_path
);

checked_string!(
    /// A path in a sandbox that a workflow may put a directory or a file at: an
    /// [`AbsolutePath`] other than `/`, outside the directories that hold Pumpkin's own files
    /// ([`sandbox`](crate::sandbox)).
    RolePath,
    check_role_path
);

checked_string!(
    /// The name of a model the model provider serves.
    ModelName,
    check_model_name
);

checked_string!(
    /// A JSON pointer (RFC 6901): empty for the whole document, or `/`-prefixed reference
    /// tokens, in which `~0` stands for `~` and `~1` for `/`. Every `~` starts one of those
    /// escapes.
    JsonPointer,
    check_json_pointer
);

impl JsonPointer {
    /// The pointer to the whole document.
    pub fn root() -> Self {
        Self(Cow::Borrowed(""))
    }

    /// This pointer with one more reference token, `token`, escaped.
    #[must_use]
    pub fn token(&self, token: impl fmt::Display) -> Self {
        // `~` first, so that the `~` of a `~1` is not escaped again.
        let token = token.to_string().replace('~', "~0").replace('/', "~1");
        Self(Cow::Owned(format!("{}/{token}", self.0)))
    }

    /// The value the pointer names in `document`, if there is one. A token names an object's
    /// member, or an array's element by its index in decimal without leading zeros.
    pub fn resolve<'v>(&self, document: &'v serde_json::Value) -> Option<&'v serde_json::Value> {
        // serde_json resolves RFC 6901 pointers, and the check above has refused any other.
        document.pointer(self.as_str())
    }

    /// The reference tokens, unescaped, from the document's top down: none for the whole
    /// document.
    pub fn tokens(&self) -> impl Iterator<Item = String> {
        // `~1` first, so that the `~` a `~0` gives does not make an escape with what follows it.
        let tokens = self.0.split('/').skip(1);
        tokens.map(|token| token.replace("~1", "/").replace("~0", "~"))
    }
}

impl Name {
    /// `name`, a literal that must be a name: in a `const`, as the crate that writes it builds,
    /// as in `const AGENT: Name = Name::from_static("agent");`.
    ///
    /// # Panics
    ///
    /// If `name` isn't a name.
    pub const fn from_static(name: &'static str) -> Self {
        assert!(is_name(name), "not a name");
        Self(Cow::Borrowed(name))
    }
}

/// Whether `name` is a [`Name`].
const fn is_name(name: &str) -> bool {
    let [first, rest @ ..] = name.as_bytes() else {
        return false;
    };
    let mut index = 0;
    while index < rest.len() {
        let byte = rest[index];
        if !(byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-') {
            return false;
        }
        index += 1;
    }
    first.is_ascii_lowercase() && name.len() <= 64
}

fn check_name(name: &str) -> Result<(), String> {
    if is_name(name) {
        Ok(())
    } else {
        Err(format!(
            "`{}` is not a valid name: use 1 to 64 characters from a-z, 0-9, `_` and `-`, \
             starting with a letter",
            name.escape_debug()
        ))
    }
}

fn check_workflow_name(name: &str) -> Result<(), String> {
    let valid = (1..=128).contains(&name.len())
        && name
            .chars()
            .all(|char| char.is_ascii_alphanumeric() || matches!(char, '.' | '_' | '-' | '/'));
    if valid {
        Ok(())
    } else {
        Err(format!(
            "`{}` is not a valid workflow name: use 1 to 128 characters from A-Z, a-z, 0-9, \
             `.`, `_`, `-` and `/`",
            name.escape_debug()
        ))
    }
}

fn check_image(image: &str) -> Result<(), String> {
    let pinned = match image.split_once('@') {
        Some((repository, digest)) => {
            !repository.is_empty()
                && !repository.contains(|char: char| char.is_whitespace() || char == '\0')
                && check_sha256_digest(digest).is_ok()
        }
        None => check_sha256_digest(image).is_ok(),
    };
    if pinned {
        Ok(())
    } else {
        Err(format!(
            "`{}` is not a pinned image: use `<repository>@sha256:<hex>` or `sha256:<hex>`",
            image.escape_debug()
        ))
    }
}

fn check_sha256_digest(digest: &str) -> Result<(), String> {
    let valid = digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    });
    if valid {
        Ok(())
    } else {
        Err(format!(
            "`{}` is not a digest: use `sha256:` and 64 lowercase hexadecimal digits",
            digest.escape_debug()
        ))
    }
}

impl From<[u8; 32]> for Sha256Digest {
    /// The digest of SHA-256's 32 bytes, `bytes`: `sha256:` and each byte as two lowercase
    /// hexadecimal digits.
    fn from(bytes: [u8; 32]) -> Self {
        Self(Cow::Owned(format!("sha256:{}", hex(&bytes))))
    }
}

/// `bytes`, each as two lowercase hexadecimal digits.
pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    (bytes.iter())
        .flat_map(|byte| [byte >> 4, byte & 0xf])
        .map(|nibble| char::from(DIGITS[usize::from(nibble)]))
        .collect()
}

/// Whether `path` is a [`RelativePath`]: one or more `/`-separated components, none of them
/// empty, `.` or `..`, and no NUL byte.
const fn is_relative_path(path: &[u8]) -> bool {
    let (mut start, mut index) = (0, 0);
    while index <= path.len() {
        if index == path.len() || path[index] == b'/' {
            let (component, _) = path.split_at(index);
            let (_, component) = component.split_at(start);
            if matches!(component, b"" | b"." | b"..") {
                return false;
            }
            start = index + 1;
        } else if path[index] == 0 {
            return false;
        }
        index += 1;
    }
    true
}

/// Whether `path` is an [`AbsolutePath`]: `/`, or `/` and a relative path.
const fn is_absolute_path(path: &[u8]) -> bool {
    match path {
        [b'/'] => true,
        [b'/', relative @ ..] => is_relative_path(relative),
        _ => false,
    }
}

impl AbsolutePath {
    /// `/`.
    pub fn root() -> Self {
        Self(Cow::Borrowed("/"))
    }

    /// `path`, a literal that must be a normalized absolute path, as
    /// [`Name::from_static`] takes a name.
    ///
    /// # Panics
    ///
    /// If `path` isn't one.
    pub const fn from_static(path: &'static str) -> Self {
        assert!(is_absolute_path(path.as_bytes()), "not an absolute path");
        Self(Cow::Borrowed(path))
    }

    /// `path` below this one.
    #[must_use]
    pub fn join(&self, path: &RelativePath) -> Self {
        // Both are normalized, so `path`'s components follow this path's, a `/` between them;
        // only `/` itself ends with one.
        let parent = self.0.trim_end_matches('/');
        Self(Cow::Owned(format!("{parent}/{}", path.0)))
    }

    /// Whether `path` is this path or below it.
    pub(crate) fn holds(&self, path: &str) -> bool {
        let below = path.strip_prefix(self.0.trim_end_matches('/'));
        below.is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    }
}

impl RelativePath {
    /// `path`, a literal that must be a normalized relative path, as [`Name::from_static`]
    /// takes a name.
    ///
    /// # Panics
    ///
    /// If `path` isn't one.
    pub const fn from_static(path: &'static str) -> Self {
        assert!(is_relative_path(path.as_bytes()), "not a relative path");
        Self(Cow::Borrowed(path))
    }

    /// `path` below this one.
    #[must_use]
    pub fn join(&self, path: &Self) -> Self {
        // Both are normalized, so their components, a `/` between them, are.
        Self(Cow::Owned(format!("{}/{}", self.0, path.0)))
    }
}

impl From<&Name> for RelativePath {
    /// `name`, one component, as a name holds no `/` and starts with a letter.
    fn from(name: &Name) -> Self {
        Self(name.0.clone())
    }
}

impl From<u64> for RelativePath {
    /// `number`'s decimal digits, one component.
    fn from(number: u64) -> Self {
        Self(Cow::Owned(number.to_string()))
    }
}

impl RolePath {
    /// The path below `/`, which [`AbsolutePath::root`] joins back to this one.
    pub fn relative(&self) -> RelativePath {
        // Normalized and not `/`, so one `/` comes before its components.
        RelativePath(Cow::Owned(self.0[1..].to_owned()))
    }

    /// Whether this path is below `other`.
    pub(crate) fn is_below(&self, other: &Self) -> bool {
        (self.0.strip_prefix(other.as_str())).is_some_and(|rest| rest.starts_with('/'))
    }
}

fn check_role_path(path: &str) -> Result<(), String> {
    check_absolute_path(path)?;
    if path == "/" {
        return Err("`/` holds the whole sandbox: name a path below it".to_owned());
    }
    match (crate::sandbox::pumpkin_dirs().iter()).find(|dir| dir.holds(path)) {
        Some(dir) => Err(format!(
            "`{path}` is in `{dir}`, which holds Pumpkin's own files: name a path outside it"
        )),
        None => Ok(()),
    }
}

fn check_absolute_path(path: &str) -> Result<(), String> {
    if is_absolute_path(path.as_bytes()) {
        Ok(())
    } else {
        Err(format!(
            "`{}` is not a normalized absolute path: start with `/`, and use no empty, `.` or \
             `..` components and no NUL byte",
            path.escape_debug()
        ))
    }
}

fn check_relative_path(path: &str) -> Result<(), String> {
    if is_relative_path(path.as_bytes()) {
        Ok(())
    } else {
        Err(format!(
            "`{}` is not a normalized relative path: use `/`-separated components, none of them \
             empty, `.` or `..`, and no NUL byte",
            path.escape_debug()
        ))
    }
}

fn check_json_pointer(pointer: &str) -> Result<(), String> {
    let escaped = pointer
        .split('~')
        .skip(1)
        .all(|after| after.starts_with(['0', '1']));
    if (pointer.is_empty() || pointer.starts_with('/')) && escaped {
        Ok(())
    } else {
        Err(format!(
            "`{}` is not a JSON pointer: write `/` before each token, `~0` for `~` and `~1` \
             for `/`, or nothing for the whole document",
            pointer.escape_debug()
        ))
    }
}

fn check_model_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.contains(|char: char| char.is_whitespace() || char == '\0') {
        Err(format!(
            "`{}` is not a model name: use one word",
            name.escape_debug()
        ))
    } else {
        Ok(())
    }
}

/// Permission bits, from `0000` to `0777`: read, write and execute, for a file's owner, its
/// group and others. Written as four octal digits, as `chmod` takes them, such as `0644`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Mode(u16);

impl Mode {
    /// The mode of `bits`, such as `Mode::new(0o644)`.
    ///
    /// # Panics
    ///
    /// If `bits` is over `0o777`.
    pub const fn new(bits: u16) -> Self {
        assert!(bits <= 0o777, "not a mode");
        Self(bits)
    }

    pub fn bits(self) -> u32 {
        u32::from(self.0)
    }
}

impl TryFrom<String> for Mode {
    type Error = String;

    fn try_from(mode: String) -> Result<Self, Self::Error> {
        let digits = mode.strip_prefix('0').filter(|digits| {
            digits.len() == 3 && digits.bytes().all(|digit| (b'0'..=b'7').contains(&digit))
        });
        let bits = digits.map(|digits| {
            (digits.bytes()).fold(0, |bits, digit| bits << 3 | u16::from(digit - b'0'))
        });
        bits.map(Self).ok_or_else(|| {
            format!(
                "`{}` is not a mode: write `0` and three octal digits, such as `0644`",
                mode.escape_debug()
            )
        })
    }
}

impl From<Mode> for String {
    fn from(mode: Mode) -> Self {
        mode.to_string()
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04o}", self.0)
    }
}

/// A command's program and arguments. The program is not empty, and nothing holds a NUL byte.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<String>", into = "Vec<String>")]
pub struct Argv {
    program: String,
    args: Vec<String>,
}

impl Argv {
    pub fn program(&self) -> &str {
        &self.program
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }
}

impl TryFrom<Vec<String>> for Argv {
    type Error = String;

    fn try_from(command: Vec<String>) -> Result<Self, Self::Error> {
        let mut command = command.into_iter();
        let program = match command.next() {
            None => return Err("the command is empty: it needs a program".to_owned()),
            Some(program) if program.is_empty() => {
                return Err("the program is empty: name one".to_owned());
            }
            Some(program) => program,
        };
        let args: Vec<String> = command.collect();
        if let Some(arg) = iter::once(&program)
            .chain(&args)
            .find(|arg| arg.contains('\0'))
        {
            return Err(format!(
                "`{}` holds a NUL byte: no argument may",
                arg.escape_debug()
            ));
        }
        Ok(Self { program, args })
    }
}

impl From<Argv> for Vec<String> {
    fn from(argv: Argv) -> Self {
        let Argv { program, args } = argv;
        iter::once(program).chain(args).collect()
    }
}

/// An environment entry, written `NAME=value`. The name is a POSIX environment variable name,
/// and the value holds no NUL byte.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EnvVar {
    name: String,
    value: String,
}

impl EnvVar {
    /// `HOME`, set to `home`.
    pub(crate) fn home(home: &AbsolutePath) -> Self {
        // `HOME` is a variable's name, and a path holds no NUL byte.
        Self {
            name: "HOME".to_owned(),
            value: home.to_string(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn value(&self) -> &str {
        &self.value
    }
}

impl TryFrom<String> for EnvVar {
    type Error = String;

    fn try_from(entry: String) -> Result<Self, Self::Error> {
        let Some((name, value)) = entry.split_once('=') else {
            return Err(format!(
                "`{}` is not an environment entry: write `NAME=value`",
                entry.escape_debug()
            ));
        };
        let mut chars = name.chars();
        let valid_name = chars
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
            && chars.all(|char| char.is_ascii_alphanumeric() || char == '_');
        if !valid_name {
            return Err(format!(
                "`{}` is not a valid environment variable name: use A-Z, a-z, 0-9 and `_`, \
                 not starting with a digit",
                name.escape_debug()
            ));
        }
        if value.contains('\0') {
            return Err(format!(
                "the value of `{name}` holds a NUL byte: no value may"
            ));
        }
        Ok(Self {
            name: name.to_owned(),
            value: value.to_owned(),
        })
    }
}

impl From<EnvVar> for String {
    fn from(entry: EnvVar) -> Self {
        let EnvVar { name, value } = entry;
        format!("{name}={value}")
    }
}

/// Environment entries, each name set once.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<EnvVar>", into = "Vec<EnvVar>")]
pub struct Env(Vec<EnvVar>);

impl Env {
    /// This environment, with `default` before its entries unless one of them sets its name.
    #[must_use]
    pub(crate) fn with_default(&self, default: EnvVar) -> Self {
        let set = self.0.iter().any(|entry| entry.name == default.name);
        let default = (!set).then_some(default);
        Self(default.into_iter().chain(self.0.iter().cloned()).collect())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn as_slice(&self) -> &[EnvVar] {
        &self.0
    }
}

impl TryFrom<Vec<EnvVar>> for Env {
    type Error = String;

    fn try_from(entries: Vec<EnvVar>) -> Result<Self, Self::Error> {
        let mut names = BTreeSet::new();
        if let Some(entry) = entries.iter().find(|entry| !names.insert(entry.name())) {
            return Err(format!(
                "`{}` is set more than once: set each name once",
                entry.name()
            ));
        }
        Ok(Self(entries))
    }
}

impl From<Env> for Vec<EnvVar> {
    fn from(env: Env) -> Self {
        env.0
    }
}

/// The models a route allows: at least one, each listed once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<ModelName>", into = "Vec<ModelName>")]
pub struct Models(Vec<ModelName>);

impl Models {
    pub fn as_slice(&self) -> &[ModelName] {
        &self.0
    }
}

impl TryFrom<Vec<ModelName>> for Models {
    type Error = String;

    fn try_from(models: Vec<ModelName>) -> Result<Self, Self::Error> {
        if models.is_empty() {
            return Err("no models: a model route allows at least one".to_owned());
        }
        let mut seen = BTreeSet::new();
        if let Some(model) = models.iter().find(|model| !seen.insert(*model)) {
            return Err(format!(
                "`{}` is listed more than once: list each model once",
                model.as_str().escape_debug()
            ));
        }
        Ok(Self(models))
    }
}

impl From<Models> for Vec<ModelName> {
    fn from(models: Models) -> Self {
        models.0
    }
}

/// The fields of a judge's result that its caller's reply returns.
///
/// Each is a JSON pointer into the result, keyed in the reply's
/// [`Returned`](crate::sandbox::op::Returned) by its last reference token. So every pointer
/// names a field below the whole result, and no two share a key. Nor is any key empty.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<JsonPointer>", into = "Vec<JsonPointer>")]
pub struct Returns(Vec<(String, JsonPointer)>);

impl Returns {
    /// Each field's key, with the pointer to its value, in the IR's order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &JsonPointer)> {
        self.0.iter().map(|(key, pointer)| (key.as_str(), pointer))
    }
}

impl TryFrom<Vec<JsonPointer>> for Returns {
    type Error = String;

    fn try_from(pointers: Vec<JsonPointer>) -> Result<Self, Self::Error> {
        let mut fields: Vec<(String, JsonPointer)> = Vec::with_capacity(pointers.len());
        for pointer in pointers {
            let Some(key) = pointer.tokens().last() else {
                let message = "the empty pointer names the whole result, with no key: point to \
                               a field";
                return Err(message.to_owned());
            };
            if key.is_empty() {
                return Err(format!(
                    "`{pointer}` would return the empty key: point to a field with a name"
                ));
            }
            if let Some((_, earlier)) = fields.iter().find(|(seen, _)| *seen == key) {
                return Err(format!(
                    "`{earlier}` and `{pointer}` would both return `{key}`: return each key once"
                ));
            }
            fields.push((key, pointer));
        }
        Ok(Self(fields))
    }
}

impl From<Returns> for Vec<JsonPointer> {
    fn from(returns: Returns) -> Self {
        returns.0.into_iter().map(|(_, pointer)| pointer).collect()
    }
}

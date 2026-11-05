//! Values that deserialization checks, so that each one in an IR or a sandbox protocol message
//! is well formed.

use std::collections::BTreeSet;
use std::{fmt, iter};

use serde::{Deserialize, Serialize};

/// Defines a string newtype that holds only the values `$check` accepts.
macro_rules! checked_string {
    ($(#[$attr:meta])* $name:ident, $check:ident) => {
        $(#[$attr])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = String;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                $check(&value)?;
                Ok(Self(value))
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
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
    /// The name of a role, slot or grant: 1 to 64 characters from a-z, 0-9, `_` and `-`,
    /// starting with a letter.
    Name,
    check_name
);

checked_string!(
    /// A workflow's name, such as `frontier-cs/lwe_structured_recovery`: 1 to 128 characters
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
    /// The value the pointer names in `document`, if there is one. A token names an object's
    /// member, or an array's element by its index in decimal without leading zeros.
    pub fn resolve<'v>(&self, document: &'v serde_json::Value) -> Option<&'v serde_json::Value> {
        // serde_json resolves RFC 6901 pointers, and the check above has refused any other.
        document.pointer(self.as_str())
    }

    /// The last reference token, unescaped: `None` for the whole document.
    fn last_token(&self) -> Option<String> {
        let (_, token) = self.0.rsplit_once('/')?;
        Some(token.replace("~1", "/").replace("~0", "~"))
    }
}

fn check_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let valid = name.len() <= 64
        && chars.next().is_some_and(|first| first.is_ascii_lowercase())
        && chars.all(|char| {
            char.is_ascii_lowercase() || char.is_ascii_digit() || char == '_' || char == '-'
        });
    if valid {
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

/// Whether `path` is one or more `/`-separated components, none of them empty, `.` or `..`.
fn has_normal_components(path: &str) -> bool {
    path.split('/')
        .all(|component| !matches!(component, "" | "." | ".."))
}

fn check_absolute_path(path: &str) -> Result<(), String> {
    let normalized = path == "/" || path.strip_prefix('/').is_some_and(has_normal_components);
    if normalized && !path.contains('\0') {
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
    if has_normal_components(path) && !path.contains('\0') {
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
            let Some(key) = pointer.last_token() else {
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

//! Each Chat tool index owns one Responses output position. A call's ID and function name
//! may arrive in pieces; opening its item waits for arguments, or for the end of the choice.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::super::response::{call, call_item_id, message};
use super::{Error, Events, optional};
use crate::usage::MAX_HELD_BYTES;

#[derive(Default)]
pub(super) struct Output {
    items: Vec<Item>,
    message_index: Option<usize>,
    empty_part: Option<PartKind>,
    calls: BTreeMap<u64, usize>,
    call_ids: BTreeSet<String>,
    // A prefix of the output: SDKs append items as they hear output_item.added.
    added: usize,
    held_bytes: usize,
}

enum Item {
    Message {
        id: String,
        parts: Vec<Part>,
    },
    Call {
        item_id: String,
        id: String,
        name: String,
        arguments: String,
    },
}

struct Part {
    kind: PartKind,
    text: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PartKind {
    Text,
    Refusal,
}

impl PartKind {
    fn value(self, text: &str) -> Value {
        match self {
            Self::Text => json!({"type":"output_text","text":text,"annotations":[],"logprobs":[]}),
            Self::Refusal => json!({"type":"refusal","refusal":text}),
        }
    }

    fn event(
        self,
        kind: &str,
        id: &str,
        index: usize,
        part: usize,
        text: &str,
        events: &mut Events<'_>,
    ) -> Result<(), Error> {
        let mut body = json!({"item_id":id,"output_index":index,"content_index":part});
        let (prefix, field) = match self {
            Self::Text => {
                body["logprobs"] = json!([]);
                ("output_text", "text")
            }
            Self::Refusal => ("refusal", "refusal"),
        };
        body[if kind == "delta" { "delta" } else { field }] = json!(text);
        events.send(&format!("response.{prefix}.{kind}"), body)?;
        Ok(())
    }

    fn part(
        self,
        kind: &str,
        id: &str,
        index: usize,
        part: usize,
        text: &str,
        events: &mut Events<'_>,
    ) -> Result<(), Error> {
        events.send(
            &format!("response.content_part.{kind}"),
            json!({"item_id":id,"output_index":index,"content_index":part,"part":self.value(text)}),
        )?;
        Ok(())
    }
}

impl Item {
    fn value(&self, status: &str, empty: bool) -> Value {
        match self {
            Self::Message { id, parts } => {
                let parts = if empty {
                    Vec::new()
                } else {
                    parts.iter().map(|p| p.kind.value(&p.text)).collect()
                };
                message(id, status, Value::Array(parts))
            }
            Self::Call {
                item_id,
                id,
                name,
                arguments,
            } => call(
                item_id,
                id,
                name,
                if empty { "" } else { arguments },
                status,
            ),
        }
    }

    fn call_delta(
        index: usize,
        item_id: &str,
        arguments: &str,
        events: &mut Events<'_>,
    ) -> Result<(), Error> {
        events.send(
            "response.function_call_arguments.delta",
            json!({"item_id":item_id,"output_index":index,"delta":arguments}),
        )?;
        Ok(())
    }
}

impl Output {
    pub(super) fn has_calls(&self) -> bool {
        !self.calls.is_empty()
    }

    fn hold(&mut self, bytes: usize) -> Result<(), Error> {
        if bytes > MAX_HELD_BYTES - self.held_bytes {
            return Err(Error::Invalid("Chat output exceeds the route's byte limit"));
        }
        self.held_bytes += bytes;
        Ok(())
    }

    pub(super) fn delta(
        &mut self,
        delta: &Value,
        response_id: &str,
        events: &mut Events<'_>,
    ) -> Result<(), Error> {
        let fields = delta
            .as_object()
            .ok_or(Error::Invalid("expected a Chat delta object"))?;
        for (key, value) in fields {
            if !value.is_null()
                && !matches!(key.as_str(), "role" | "content" | "refusal" | "tool_calls")
            {
                return Err(Error::Invalid("unsupported Chat output in delta"));
            }
        }
        if optional(delta, "role")?.is_some_and(|role| role != "assistant") {
            return Err(Error::Invalid("expected an assistant Chat delta"));
        }
        for (field, kind) in [("content", PartKind::Text), ("refusal", PartKind::Refusal)] {
            match optional(delta, field)? {
                Some("") => self.empty_part = Some(kind),
                Some(text) => self.text(kind, text, response_id, events)?,
                None => {}
            }
        }
        if !delta["tool_calls"].is_null() {
            for tool in delta["tool_calls"]
                .as_array()
                .ok_or(Error::Invalid("expected an array of Chat tool calls"))?
            {
                self.tool(tool, response_id, events)?;
            }
        }
        Ok(())
    }

    fn text(
        &mut self,
        kind: PartKind,
        delta: &str,
        response_id: &str,
        events: &mut Events<'_>,
    ) -> Result<(), Error> {
        self.hold(delta.len())?;
        let index = *self.message_index.get_or_insert_with(|| {
            let index = self.items.len();
            self.items.push(Item::Message {
                id: format!("msg_{response_id}"),
                parts: Vec::new(),
            });
            index
        });
        let Item::Message { id, parts } = &mut self.items[index] else {
            unreachable!("the message index addresses the message");
        };
        let part = parts.iter().position(|part| part.kind == kind);
        let part = if let Some(part) = part {
            part
        } else {
            let at = parts.len();
            parts.push(Part {
                kind,
                text: String::new(),
            });
            if index < self.added {
                kind.part("added", id, index, at, "", events)?;
            }
            at
        };
        parts[part].text.push_str(delta);
        if index < self.added {
            kind.event("delta", id, index, part, delta, events)?;
        } else {
            self.publish(false, events)?;
        }
        Ok(())
    }

    fn tool(
        &mut self,
        tool: &Value,
        response_id: &str,
        events: &mut Events<'_>,
    ) -> Result<(), Error> {
        let tool_index = tool["index"]
            .as_u64()
            .ok_or(Error::Invalid("Chat tool delta has no index"))?;
        if optional(tool, "type")?.is_some_and(|kind| kind != "function")
            || !(tool["function"].is_null() || tool["function"].is_object())
        {
            return Err(Error::Invalid("expected a Chat function tool delta"));
        }
        let id_delta = optional(tool, "id")?.unwrap_or_default();
        let name_delta = optional(&tool["function"], "name")?.unwrap_or_default();
        let arguments_delta = optional(&tool["function"], "arguments")?.unwrap_or_default();
        self.hold(id_delta.len() + name_delta.len() + arguments_delta.len())?;
        let index = if let Some(&index) = self.calls.get(&tool_index) {
            index
        } else {
            let index = self.items.len();
            let item_id = call_item_id(response_id, index);
            self.hold(size_of::<Item>() + item_id.len())?;
            self.items.push(Item::Call {
                item_id,
                id: String::new(),
                name: String::new(),
                arguments: String::new(),
            });
            self.calls.insert(tool_index, index);
            index
        };
        let Item::Call {
            item_id,
            id,
            name,
            arguments,
        } = &mut self.items[index]
        else {
            unreachable!("every tool index addresses a call");
        };
        if index < self.added && (!id_delta.is_empty() || !name_delta.is_empty()) {
            return Err(Error::Invalid(
                "Chat call ID or name changed after its arguments began",
            ));
        }
        id.push_str(id_delta);
        name.push_str(name_delta);
        arguments.push_str(arguments_delta);
        if index < self.added {
            if !arguments_delta.is_empty() {
                Item::call_delta(index, item_id, arguments_delta, events)?;
            }
        } else {
            self.publish(false, events)?;
        }
        Ok(())
    }

    fn publish(&mut self, ending: bool, events: &mut Events<'_>) -> Result<(), Error> {
        while let Some(item) = self.items.get(self.added) {
            let index = self.added;
            if let Item::Call {
                id,
                name,
                arguments,
                ..
            } = item
            {
                if id.is_empty() || name.is_empty() {
                    if ending {
                        return Err(Error::Invalid(
                            "Chat call ended without an ID or function name",
                        ));
                    }
                    break;
                }
                if !ending && arguments.is_empty() {
                    break;
                }
                if !self.call_ids.insert(id.clone()) {
                    return Err(Error::Invalid("Chat stream repeats a tool call ID"));
                }
            }
            events.send(
                "response.output_item.added",
                json!({"output_index":index,"item":item.value("in_progress",true)}),
            )?;
            self.added += 1;
            match item {
                Item::Message { id, parts } => {
                    for (part, p) in parts.iter().enumerate() {
                        p.kind.part("added", id, index, part, "", events)?;
                        p.kind.event("delta", id, index, part, &p.text, events)?;
                    }
                }
                Item::Call {
                    item_id, arguments, ..
                } if !arguments.is_empty() => {
                    Item::call_delta(index, item_id, arguments, events)?;
                }
                Item::Call { .. } => {}
            }
        }
        Ok(())
    }

    pub(super) fn close(
        &mut self,
        status: &str,
        response_id: &str,
        events: &mut Events<'_>,
    ) -> Result<(), Error> {
        if self.items.is_empty() {
            // A tool-only turn may start with an empty role delta. Only an otherwise empty
            // answer gets a message for it.
            if let Some(kind) = self.empty_part {
                self.text(kind, "", response_id, events)?;
            } else if status == "completed" {
                return Err(Error::Invalid("Chat choice finished without output"));
            }
        }
        self.publish(true, events)?;
        for (index, item) in self.items.iter().enumerate() {
            match item {
                Item::Message { id, parts } => {
                    for (part, p) in parts.iter().enumerate() {
                        p.kind.event("done", id, index, part, &p.text, events)?;
                        p.kind.part("done", id, index, part, &p.text, events)?;
                    }
                }
                Item::Call {
                    item_id,
                    name,
                    arguments,
                    ..
                } => {
                    events.send("response.function_call_arguments.done",json!({"item_id":item_id,"output_index":index,"name":name,"arguments":arguments}))?;
                }
            }
            events.send(
                "response.output_item.done",
                json!({"output_index":index,"item":item.value(status,false)}),
            )?;
        }
        Ok(())
    }

    pub(super) fn snapshot(&self, status: &str) -> Value {
        Value::Array(
            self.items[..self.added]
                .iter()
                .map(|item| item.value(status, false))
                .collect(),
        )
    }
}

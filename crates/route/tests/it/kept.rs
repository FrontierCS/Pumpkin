//! Model exchanges kept from real upstreams: Codex's over the Responses API, and a client's of
//! Chat Completions, each served by Z.ai's GLM, cut short, their ids made up; and exchanges of the
//! Messages API, which no record keeps, written from its documentation's forms:
//! <https://platform.claude.com/docs/en/api/messages> and
//! <https://platform.claude.com/docs/en/build-with-claude/streaming>.

/// A Codex request of the Responses API, its texts and tools cut short.
pub(crate) const RESPONSES_REQUEST: &str = r##"{"client_metadata":{"session_id":"25e1ab03-9a58-4703-9e58-4d0369258fab"},"include":["reasoning.encrypted_content"],"input":[{"content":[{"text":"You are a coding","type":"input_text"}],"id":"msg_4703cdef-bcd0-6b2d-f470-e925cd4703cd","role":"developer","type":"message"},{"content":[{"text":"Solve the evaluation task","type":"input_text"}],"id":"msg_58b4de36-2d81-7036-258d-f0369e58bcde","role":"user","type":"message"},{"content":[{"text":"We need inspect README","type":"reasoning_text"}],"encrypted_content":null,"id":"rs_resp_6925814703692581e7abc92f8b4703","summary":[],"type":"reasoning"},{"content":[{"annotations":[],"text":"I’ll read the task","type":"output_text"}],"id":"msg_resp_6925814703692581e7abc92f8b4703","role":"assistant","status":"completed","type":"message"},{"arguments":"{\"cmd\":\"cat /frontier/task/README.md\",\"workdir\":\"/workspace\",\"yield_time_ms\":10000,\"max_output_tokens\":30000}","call_id":"call_7036f25cd4703cd25abc703a","id":"fc_call_7036f25cd4703cd25abc703a","name":"exec_command","status":"completed","type":"function_call"},{"call_id":"call_7036f25cd4703cd25abc703a","id":"fco_81e7ab69-fab4-0369-def4-cd6fabc1e703","output":"Chunk ID: d1afae Wall time: 0.0000 seconds Process exited","type":"function_call_output"}],"model":"glm-5.3-flash","parallel_tool_calls":true,"prompt_cache_key":"25e1ab03-9a58-4703-9e58-4d0369258fab","reasoning":{"effort":"xhigh","summary":"auto"},"store":false,"stream":true,"tool_choice":"auto","tools":[{"description":"Runs a command in","name":"exec_command","parameters":{"type":"object","properties":{}},"strict":false,"type":"function"},{"description":"Writes characters to an","name":"write_stdin","parameters":{"type":"object","properties":{}},"strict":false,"type":"function"}]}"##;

/// Its answer, a stream of reasoning, text and a call, most deltas left out.
pub(crate) const RESPONSES_STREAM: &str = r##"event: response.created
data: {"response":{"created_at":1794701812,"id":"resp_14703692581470d69258147a369e58","model":"glm-5.3-flash","object":"response","output":[],"prompt_cache_key":"25e1ab03-9a58-4703-9e58-4d0369258fab","status":"in_progress"},"sequence_number":0,"type":"response.created"}

event: response.in_progress
data: {"response":{"created_at":1794701812,"id":"resp_14703692581470d69258147a369e58","model":"glm-5.3-flash","object":"response","output":[],"prompt_cache_key":"25e1ab03-9a58-4703-9e58-4d0369258fab","status":"in_progress"},"sequence_number":1,"type":"response.in_progress"}

event: response.output_item.added
data: {"item":{"content":[],"id":"rs_resp_14703692581470d69258147a369e58","status":"in_progress","summary":[],"type":"reasoning"},"output_index":0,"sequence_number":2,"type":"response.output_item.added"}

event: response.content_part.added
data: {"content_index":0,"item_id":"rs_resp_14703692581470d69258147a369e58","output_index":0,"part":{"text":"","type":"reasoning_text"},"sequence_number":3,"type":"response.content_part.added"}

event: response.reasoning_text.delta
data: {"content_index":0,"delta":"Train","item_id":"rs_resp_14703692581470d69258147a369e58","output_index":0,"sequence_number":4,"type":"response.reasoning_text.delta"}

event: response.reasoning_text.delta
data: {"content_index":0,"delta":" submit","item_id":"rs_resp_14703692581470d69258147a369e58","output_index":0,"sequence_number":5,"type":"response.reasoning_text.delta"}

event: response.reasoning_text.delta
data: {"content_index":0,"delta":" one","item_id":"rs_resp_14703692581470d69258147a369e58","output_index":0,"sequence_number":6,"type":"response.reasoning_text.delta"}

event: response.reasoning_text.done
data: {"content_index":0,"item_id":"rs_resp_14703692581470d69258147a369e58","output_index":0,"sequence_number":8,"text":"Train submit one","type":"response.reasoning_text.done"}

event: response.content_part.done
data: {"content_index":0,"item_id":"rs_resp_14703692581470d69258147a369e58","output_index":0,"part":{"text":"Train submit one","type":"reasoning_text"},"sequence_number":9,"type":"response.content_part.done"}

event: response.output_item.done
data: {"item":{"content":[{"text":"Train submit one","type":"reasoning_text"}],"encrypted_content":null,"id":"rs_resp_14703692581470d69258147a369e58","status":"completed","summary":[],"type":"reasoning"},"output_index":0,"sequence_number":10,"type":"response.output_item.done"}

event: response.output_item.added
data: {"item":{"content":[],"id":"msg_resp_14703692581470d69258147a369e58","role":"assistant","status":"in_progress","type":"message"},"output_index":1,"sequence_number":11,"type":"response.output_item.added"}

event: response.content_part.added
data: {"content_index":0,"item_id":"msg_resp_14703692581470d69258147a369e58","output_index":1,"part":{"annotations":[],"text":"","type":"output_text"},"sequence_number":12,"type":"response.content_part.added"}

event: response.output_text.delta
data: {"content_index":0,"delta":"The","item_id":"msg_resp_14703692581470d69258147a369e58","logprobs":[],"output_index":1,"sequence_number":13,"type":"response.output_text.delta"}

event: response.output_text.delta
data: {"content_index":0,"delta":" first","item_id":"msg_resp_14703692581470d69258147a369e58","logprobs":[],"output_index":1,"sequence_number":14,"type":"response.output_text.delta"}

event: response.output_text.delta
data: {"content_index":0,"delta":" usable","item_id":"msg_resp_14703692581470d69258147a369e58","logprobs":[],"output_index":1,"sequence_number":15,"type":"response.output_text.delta"}

event: response.output_text.done
data: {"content_index":0,"item_id":"msg_resp_14703692581470d69258147a369e58","output_index":1,"sequence_number":47,"text":"The first usable","type":"response.output_text.done"}

event: response.content_part.done
data: {"content_index":0,"item_id":"msg_resp_14703692581470d69258147a369e58","output_index":1,"part":{"annotations":[],"text":"The first usable","type":"output_text"},"sequence_number":48,"type":"response.content_part.done"}

event: response.output_item.done
data: {"item":{"content":[{"annotations":[],"text":"The first usable","type":"output_text"}],"id":"msg_resp_14703692581470d69258147a369e58","role":"assistant","status":"completed","type":"message"},"output_index":1,"sequence_number":49,"type":"response.output_item.done"}

event: response.output_item.added
data: {"item":{"arguments":"","call_id":"call_3ef2b81470369ef8b470f69c","id":"fc_call_3ef2b81470369ef8b470f69c","name":"exec_command","status":"in_progress","type":"function_call"},"output_index":2,"sequence_number":50,"type":"response.output_item.added"}

event: response.function_call_arguments.delta
data: {"delta":"{\"cmd\"","item_id":"fc_call_3ef2b81470369ef8b470f69c","output_index":2,"sequence_number":51,"type":"response.function_call_arguments.delta"}

event: response.function_call_arguments.delta
data: {"delta":":\"ls\"","item_id":"fc_call_3ef2b81470369ef8b470f69c","output_index":2,"sequence_number":52,"type":"response.function_call_arguments.delta"}

event: response.function_call_arguments.delta
data: {"delta":"}","item_id":"fc_call_3ef2b81470369ef8b470f69c","output_index":2,"sequence_number":53,"type":"response.function_call_arguments.delta"}

event: response.function_call_arguments.done
data: {"arguments":"{\"cmd\":\"ls\"}","item_id":"fc_call_3ef2b81470369ef8b470f69c","name":"exec_command","output_index":2,"sequence_number":90,"type":"response.function_call_arguments.done"}

event: response.output_item.done
data: {"item":{"arguments":"{\"cmd\":\"ls\"}","call_id":"call_3ef2b81470369ef8b470f69c","id":"fc_call_3ef2b81470369ef8b470f69c","name":"exec_command","status":"completed","type":"function_call"},"output_index":2,"sequence_number":91,"type":"response.output_item.done"}

event: response.completed
data: {"response":{"conversation_id":"","created_at":1794701812,"error":null,"id":"resp_14703692581470d69258147a369e58","incomplete_details":null,"instructions":"","metadata":null,"model":"glm-5.3-flash","object":"response","output":[{"content":[{"text":"Train submit one","type":"reasoning_text"}],"encrypted_content":null,"id":"rs_resp_14703692581470d69258147a369e58","status":"completed","summary":[],"type":"reasoning"},{"content":[{"annotations":[],"text":"The first usable","type":"output_text"}],"id":"msg_resp_14703692581470d69258147a369e58","role":"assistant","status":"completed","type":"message"},{"arguments":"{\"cmd\":\"ls\"}","call_id":"call_3ef2b81470369ef8b470f69c","id":"fc_call_3ef2b81470369ef8b470f69c","name":"exec_command","status":"completed","type":"function_call"}],"parallel_tool_calls":true,"previous_response_id":"","prompt_cache_key":"25e1ab03-9a58-4703-9e58-4d0369258fab","status":"completed","usage":{"input_tokens":102967,"input_tokens_details":{"cached_tokens":102720},"output_tokens":88,"output_tokens_details":{"reasoning_tokens":4},"total_tokens":103055}},"sequence_number":92,"type":"response.completed"}

"##;

/// A stream of the Responses API that its upstream broke off in its reasoning.
pub(crate) const RESPONSES_CUT: &str = r##"event: response.created
data: {"response":{"created_at":1794647920,"id":"resp_92581470369258f4bc3e9a58d47abc","model":"glm-5.3","object":"response","output":[],"prompt_cache_key":"03a9c581-b036-25c1-fa36-2f8147ef6925","status":"in_progress"},"sequence_number":0,"type":"response.created"}

event: response.in_progress
data: {"response":{"created_at":1794647920,"id":"resp_92581470369258f4bc3e9a58d47abc","model":"glm-5.3","object":"response","output":[],"prompt_cache_key":"03a9c581-b036-25c1-fa36-2f8147ef6925","status":"in_progress"},"sequence_number":1,"type":"response.in_progress"}

event: response.output_item.added
data: {"item":{"content":[],"id":"rs_resp_92581470369258f4bc3e9a58d47abc","status":"in_progress","summary":[],"type":"reasoning"},"output_index":0,"sequence_number":2,"type":"response.output_item.added"}

event: response.content_part.added
data: {"content_index":0,"item_id":"rs_resp_92581470369258f4bc3e9a58d47abc","output_index":0,"part":{"text":"","type":"reasoning_text"},"sequence_number":3,"type":"response.content_part.added"}

event: response.reasoning_text.delta
data: {"content_index":0,"delta":"Oops","item_id":"rs_resp_92581470369258f4bc3e9a58d47abc","output_index":0,"sequence_number":4,"type":"response.reasoning_text.delta"}

event: response.reasoning_text.delta
data: {"content_index":0,"delta":" remaining","item_id":"rs_resp_92581470369258f4bc3e9a58d47abc","output_index":0,"sequence_number":5,"type":"response.reasoning_text.delta"}

event: response.reasoning_text.delta
data: {"content_index":0,"delta":"_seconds","item_id":"rs_resp_92581470369258f4bc3e9a58d47abc","output_index":0,"sequence_number":6,"type":"response.reasoning_text.delta"}

"##;

/// A stream of the Responses API that failed before its output, as its upstream limited its rate.
pub(crate) const RESPONSES_FAILED: &str = r##"event: response.failed
data: {"response":{"created_at":0,"error":{"code":"rate_limit_exceeded","message":"Rate limit reached for requests[147036925814703692581470369258]"},"id":"resp_147036925814703692581470369258","incomplete_details":null,"metadata":{},"model":"glm-5.3","object":"response","output":[],"previous_response_id":null,"status":"failed","usage":null},"sequence_number":0,"type":"response.failed"}

event: error
data: {"code":"rate_limit_exceeded","message":"Rate limit reached for requests[147036925814703692581470369258]","param":null,"sequence_number":1,"type":"error"}

"##;

/// A request of Chat Completions with tool calls and their outputs, its texts cut short.
pub(crate) const CHAT_REQUEST: &str = r##"{"max_tokens":4096,"messages":[{"content":"You are an AI language model","role":"system"},{"content":"Please take a look at the","role":"user"},{"content":"I'll start by finding the file","role":"assistant","tool_calls":[{"function":{"arguments":"{\"filename\": \"team-building-activities.docx\"}","name":"search_files_by_filename"},"id":"call_-4703692581470369258","type":"function"}]},{"content":"ValidationError: The tool 'search_files_by_filename' is not","name":"search_files_by_filename","role":"tool","tool_call_id":"call_-4703692581470369258"}],"model":"glm-5.3-flash","tool_choice":"auto","tools":[{"function":{"description":"Sends an email with","name":"send_email","parameters":{"type":"object","properties":{}}},"type":"function"},{"function":{"description":"Deletes the email with","name":"delete_email","parameters":{"type":"object","properties":{}}},"type":"function"}]}"##;

/// Its answer, no stream, with a tool call.
pub(crate) const CHAT_ANSWER: &str = r##"{"choices":[{"finish_reason":"tool_calls","index":0,"message":{"content":"Let me try searching for files by content instead.","reasoning_content":"Both search_files_by_filename and list_files are unavailable. Let me try search_files with a query, or get_file_by_id (but I don't know the ID).","role":"assistant","tool_calls":[{"function":{"arguments":"{\"query\":\"team-building\"}","name":"search_files"},"id":"call_-2581470369258147036","index":0,"type":"function"}]}}],"created":1794615295,"id":"369258147036925e1a7cde9ab8147a","model":"glm-5.3-flash","object":"chat.completion","request_id":"369258147036925e1a7cde9ab8147a","usage":{"completion_tokens":57,"completion_tokens_details":{"reasoning_tokens":34},"prompt_tokens":3323,"prompt_tokens_details":{"cached_tokens":0},"total_tokens":3380}}"##;

/// A stream of Chat Completions of reasoning and text, most deltas left out.
pub(crate) const CHAT_STREAM: &str = r##"data: {"id":"147036925814703c9e58b47e36925e","created":1794610560,"object":"chat.completion.chunk","model":"glm-5.3-flash","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"The"}}]}

data: {"id":"147036925814703c9e58b47e36925e","created":1794610560,"object":"chat.completion.chunk","model":"glm-5.3-flash","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":" user"}}]}

data: {"id":"147036925814703c9e58b47e36925e","created":1794610560,"object":"chat.completion.chunk","model":"glm-5.3-flash","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":" is"}}]}

data: {"id":"147036925814703c9e58b47e36925e","created":1794610560,"object":"chat.completion.chunk","model":"glm-5.3-flash","choices":[{"index":0,"delta":{"role":"assistant","content":"pumpkin"}}]}

data: {"id":"147036925814703c9e58b47e36925e","created":1794610560,"object":"chat.completion.chunk","model":"glm-5.3-flash","choices":[{"index":0,"finish_reason":"stop","delta":{"role":"assistant","content":""}}],"usage":{"prompt_tokens":20,"completion_tokens":57,"total_tokens":77,"prompt_tokens_details":{"cached_tokens":0},"completion_tokens_details":{"reasoning_tokens":52}}}

data: [DONE]

"##;

/// A stream of the Messages API of thinking, text and a tool call: its usage in `message_start`,
/// then the stream's output count in `message_delta`.
pub(crate) const MESSAGES_STREAM: &str = r##"event: message_start
data: {"type":"message_start","message":{"id":"msg_01X","type":"message","role":"assistant","content":[],"model":"m-large","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":12,"cache_creation_input_tokens":300,"cache_read_input_tokens":2000,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}

event: ping
data: {"type":"ping"}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"The tests"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":" fail"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"EqQBCgIYAhIM"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Running"}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":" them."}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: content_block_start
data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_01B","name":"Bash","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}

event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":" \"cargo test\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":2}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":89}}

event: message_stop
data: {"type":"message_stop"}

"##;

/// A stream of the Messages API that its upstream broke off within a tool call's input.
pub(crate) const MESSAGES_CUT: &str = r##"event: message_start
data: {"type":"message_start","message":{"id":"msg_01Y","type":"message","role":"assistant","content":[],"model":"m-large","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":40,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01C","name":"Read","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}

"##;

/// A request of the Messages API: a system prompt in blocks, two custom tools, a user's text and
/// image, an assistant's thinking, text and tool call, the call's result, which failed, and two
/// messages of the system's, a text and, as Claude Code sends them, a block marked for caching.
pub(crate) const MESSAGES_REQUEST: &str = r##"{"max_tokens":32000,"messages":[{"content":[{"text":"Solve the task","type":"text"},{"source":{"data":"AAAA","media_type":"image/png","type":"base64"},"type":"image"}],"role":"user"},{"content":[{"signature":"EqQBCgIYAhIM","thinking":"I should list the files","type":"thinking"},{"text":"Listing the files.","type":"text"},{"id":"toolu_01A","input":{"command":"ls"},"name":"Bash","type":"tool_use"}],"role":"assistant"},{"content":[{"content":[{"text":"ls: cannot open","type":"text"}],"is_error":true,"tool_use_id":"toolu_01A","type":"tool_result"},{"text":"Go on","type":"text"}],"role":"user"},{"content":"Finish soon","role":"system"},{"content":[{"cache_control":{"type":"ephemeral"},"text":"Primary working directory: /workspace","type":"text"}],"role":"system"},{"content":"Done.","role":"assistant"}],"model":"m-large","output_config":{"effort":"high"},"stop_sequences":["</done>"],"stream":true,"system":[{"cache_control":{"type":"ephemeral"},"text":"You are Claude Code","type":"text"},{"text":"Work in /workspace","type":"text"}],"temperature":1,"thinking":{"type":"adaptive"},"tool_choice":{"type":"auto"},"tools":[{"description":"Runs a command","input_schema":{"properties":{"command":{"type":"string"}},"type":"object"},"name":"Bash"},{"input_schema":{"type":"object"},"name":"Read","type":"custom"}]}"##;

/// A stream of the Messages API that failed as its upstream was overloaded.
pub(crate) const MESSAGES_FAILED: &str = r##"event: message_start
data: {"type":"message_start","message":{"id":"msg_01Z","type":"message","role":"assistant","content":[],"model":"m-large","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":40,"output_tokens":1}}}

event: error
data: {"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}

"##;

/// An answer of the Messages API that is no stream: text and a tool call.
pub(crate) const MESSAGES_ANSWER: &str = r##"{"content":[{"text":"I'll read it.","type":"text"},{"id":"toolu_01D","input":{"path":"README.md"},"name":"Read","type":"tool_use"}],"id":"msg_01W","model":"m-large","role":"assistant","stop_reason":"tool_use","stop_sequence":null,"type":"message","usage":{"cache_creation_input_tokens":0,"cache_read_input_tokens":1500,"input_tokens":20,"output_tokens":30}}"##;

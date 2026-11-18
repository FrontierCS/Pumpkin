# Models: routes, budgets and prices

A role reaches a model only through its model route: the list of entries in its egress that say
which APIs and which models its requests may use, and how much each entry may spend. The
workflow writes the route; the machine's configuration says which upstream serves each model, at
what price, with which key. This page is for a task author writing a route and for the operator,
who runs `pumpkin` and holds the machine's configuration and keys, configuring the upstreams.

![In a sandbox, the harness sends its request, with no credential, to the model endpoint that sandboxd serves; over the session, the driver's model route admits it only through an entry that allows its model while the entry's budget lasts, the upstream adds the key and sends it to the model provider, and the route keeps each exchange in the record.](figures/model-route.svg)

## A route

A role's model egress is its route's entries, each for one API:

```python
"egress": {"model": [
    {"api": "responses", "models": ["model-a"], "budget": {"microusd": 15 * USD}},
    {"api": "chat", "models": ["model-b"]},
]},
```

`api` is `responses` (OpenAI's Responses API), `chat` (Chat Completions) or `messages`
(Anthropic's Messages API). `models` names the models that requests through the entry may name;
an API may have several entries, and a model is in one entry alone for its API. A request goes
through the entry for its API that allows the model it names; one that names a model no entry
allows is refused, with a message that names every model they allow.

The route admits a request only in a closed form: it reaches the models its workflow allows and
nothing else of the provider's, none of the state it keeps, the tools it runs or the URLs it
fetches. It sends upstream only the body it admitted, and only the headers of its API's own and
those its entry lists in `request_headers`; `response_headers` lists those it passes back. A
header that carries a credential, picks an account or signs a request can't be listed.

A harness of Pumpkin's library, such as `@pumpkin//harness/codex/codex.star`, gives an agent's
image, its runtime and its route together, with the headers its client sends and reads. The code
in the sandbox reaches the model at the endpoint `sandboxd` serves, `MODEL_URL` in the language,
with no credential.

## A budget

`budget` bounds what the role spends on the entry's models through that entry, in millionths of a
US dollar: `USD` is 10^6, so `15 * USD` is $15. Once it is spent, the route refuses the role's
requests that the entry takes, with a 400 that names the entry's models, and takes those of its
other entries as before. An entry without a budget spends without a bound.

Spend is counted at the prices of the machine's configuration, from the tokens each response
reports, so `pumpkin run` refuses a budget whose models lack a price for a kind of token.
`pumpkin wait`'s summary has a `budget <role> (<models>)` line for each budgeted entry, which
says what the role spent of its bound.

## Upstreams and prices

The machine's configuration names each upstream a route may reach, in an `[upstreams.<name>]`
table: its base URL, the file that holds its key, and each model it serves, by the name a
workflow uses, with the name the upstream knows it by and its price per million tokens of each
kind. `pumpkin help config` says every key of the table, with an example.

The key stays in the driver: it reads the key file itself, and the route adds the key to each
request it admits. The record keeps each model exchange as it passed, and the prices the run
started with; [records](record.md) says where.

### A Responses caller of a Chat model

Set `responses_via_chat = true` on the model in `[[upstreams.<name>.models]]` to send its
Responses requests to Chat Completions at the same base URL. Other models in the same entry can
serve Responses directly. Admission, the budget and prices use the workflow's entry and model.

Send `store: false`, text instructions and either a text input or a list of messages,
function calls and matching results. A message with just `role` and `content` can omit its type.
Text parts and string function arguments pass through.
All pending calls need results before the next turn. Declare ordinary function tools with an
explicit boolean `strict`: Responses and Chat default an omitted value differently. Text,
function calls, refusals and length-limited answers can be returned whole or as a stream. A
stream requests token usage from Chat; a provider that omits it leaves spend unknown.

The bridge accepts `tool_choice`, `parallel_tool_calls`, `temperature`, `top_p`,
`max_output_tokens`, `prompt_cache_key`, `metadata`, `reasoning.effort`, `text.verbosity` and
plain text output format. The `reasoning.encrypted_content` include selects a field on reasoning
items; a text or function answer has none. Codex's function-tool requests can carry its session
and turn telemetry, which stays local. Reasoning items, reasoning summaries, encrypted inputs,
custom or hosted tools and media require a native Responses route. Unsupported requests receive
a 400 before the upstream receives them.

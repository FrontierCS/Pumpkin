//! The agent's model route, as a run serves it ([`Served`]), by the rules of
//! [`model_route`](crate::model_route).

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Mutex;

use pumpkin_gateway::IncomingModelRequest;
use pumpkin_schema::ir::Name;
use pumpkin_schema::workflow::Workflow;
use stdx::{Deadline, unpoisoned};

use crate::model_route::{Answer, Refusal, Route, Serving, Upstream};

/// The agent's model route, as the run serves it: the route that closes its requests, and the
/// upstream that they reach.
pub(super) struct Served {
    route: Route,
    upstream: Upstream,
    /// The name that the deployment gives the upstream.
    name: Name,
    /// The run log's note on which upstream serves the route, by its name, and the name that it
    /// takes for each model.
    started: String,
    /// How the model provider is failing the agent.
    health: Mutex<Health>,
}

/// What the route knows of the model provider's failure of the agent's requests.
#[derive(Debug, Default)]
struct Health {
    /// Whether the route has ended ([`Served::end`]), after which no answer counts.
    ended: bool,
    /// The failure, if the last request answered while the route was open got one, and no 2xx
    /// came after it.
    failing: Option<Failing>,
}

/// The model provider's failure of the agent's requests, as the last one answered shows.
#[derive(Debug)]
struct Failing {
    /// The number of the first request of the failure.
    since: u64,
    /// The number of the last request answered, and how it failed.
    last: (u64, String),
}

impl fmt::Debug for Served {
    // The upstream shows nothing, as it holds the key.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (f.debug_struct("Served").field("route", &self.route)).finish_non_exhaustive()
    }
}

impl Served {
    /// The agent's route in `workflow`, if it has one, served as `serving` says, which goes
    /// unused for a workflow without one.
    ///
    /// # Errors
    ///
    /// Fails if the workflow has a route but `serving` is `None`, or doesn't name a model that
    /// the route allows.
    pub(super) fn new(
        workflow: &Workflow,
        serving: Option<Serving>,
    ) -> Result<Option<Self>, String> {
        let agent = &workflow.agent().sandbox;
        let Some(route) = &agent.model_route else {
            return Ok(None);
        };
        let Some(Serving {
            name,
            upstream,
            models,
        }) = serving
        else {
            return Err(format!(
                "`{}` has a model route, but no upstream serves it: give one, or remove the \
                 role's `egress.model` to run without one",
                agent.name
            ));
        };
        let mut names = BTreeMap::new();
        for model in route.models.as_slice() {
            let Some(upstream_name) = models.get(model) else {
                return Err(format!(
                    "`{}`'s model route allows `{model}`, which its upstream doesn't serve: give \
                     an upstream that serves every model the route allows",
                    agent.name
                ));
            };
            names.insert(model.clone(), upstream_name.clone());
        }
        let taken: Vec<String> = (names.iter())
            .map(|(model, upstream_name)| format!("`{model}` as `{upstream_name}`"))
            .collect();
        let started = format!(
            "the agent's model route goes to the upstream `{name}`, which takes {}",
            taken.join(", ")
        );
        let route = Route::new(names);
        Ok(Some(Self {
            route,
            upstream,
            name,
            started,
            health: Mutex::new(Health::default()),
        }))
    }

    /// The run log's note on which upstream serves the route, as the run starts.
    pub(super) fn started(&self) -> &str {
        &self.started
    }

    /// Answers `incoming`, the agent's model request numbered `number`, by `by`: refuses it
    /// unless `allowed`, or once the model provider has refused the key, and else closes it and
    /// forwards it upstream, which is given up on at `by` too. It gives `note` a note on a
    /// request that it forwards, before it does, and tells `answered` how it answered the
    /// request, before the answer's end is written.
    pub(super) fn serve(
        &self,
        number: u64,
        incoming: IncomingModelRequest,
        allowed: bool,
        by: Deadline,
        note: impl FnOnce(String),
        answered: impl FnOnce(Answer),
    ) {
        let taken = if !allowed {
            Err(Refusal::ended())
        } else if let Some(refused) = self.upstream.key_refused() {
            Err(refused)
        } else {
            self.route.close(&incoming.request)
        };
        // Whether the answer reached the agent isn't told: writing it fails only for a
        // response's head too long for a frame, or as the agent's session ends, which the run
        // log shows.
        let _ = match taken {
            Ok(closed) => {
                note(format!("model request {number} goes upstream"));
                let answered = |answer| {
                    self.bear(number, &answer);
                    answered(answer);
                };
                self.upstream.forward(&closed, incoming, by, by, answered)
            }
            Err(refusal) => {
                answered(refusal.answered(None, None));
                refusal.answer(incoming, by)
            }
        };
    }

    /// Notes how the route answered request `number`, if the route is open, in what the
    /// provider is failing the agent in.
    fn bear(&self, number: u64, answer: &Answer) {
        let mut health = unpoisoned(self.health.lock());
        if health.ended {
            return;
        }
        health.failing = match failure(answer) {
            Some(how) => {
                let since = health
                    .failing
                    .as_ref()
                    .map_or(number, |failing| failing.since);
                Some(Failing {
                    since,
                    last: (number, how),
                })
            }
            None => None,
        };
    }

    /// Checks that the model provider did not fail the agent: that it never refused the route's
    /// key, and, if `exited_unfinalized`, that the agent's last request answered while the route
    /// was open got no failure from it. `exited_unfinalized` says that the agent's exit ended
    /// the work phase with no submission finalized.
    ///
    /// # Errors
    ///
    /// Fails, naming the upstream or the fault, if the provider failed the agent so.
    pub(super) fn upstream_held(&self, exited_unfinalized: bool) -> Result<(), String> {
        if self.upstream.key_refused().is_some() {
            return Err(format!(
                "the upstream `{}` refused its key, with a 401, which ended the agent's model \
                 route: give it a key that it takes, and run again",
                self.name
            ));
        }
        // An agent that ran to the deadline, or finalized, is scored as usual: Harbor scores an
        // agent that timed out in its provider's backoff, and Inspect's `time_limit` is wall
        // clock, while none of the harnesses studied voids such a run.
        let health = unpoisoned(self.health.lock());
        let Some(Failing { since, last }) = health.failing.as_ref().filter(|_| exited_unfinalized)
        else {
            return Ok(());
        };
        let (number, how) = last;
        Err(format!(
            "the upstream `{}` failed the agent's last model request, {number} ({how}; failing \
             since request {since}), and the agent exited: a deployment fault, not the agent's \
             result; run it again",
            self.name
        ))
    }

    /// Ends the route's every exchange in flight and every later one ([`Upstream::shutdown`]).
    pub(super) fn end(&self) {
        unpoisoned(self.health.lock()).ended = true;
        self.upstream.shutdown();
    }
}

/// How the provider failed the request that `answer` answers, or `None` if it didn't: a
/// provider's answer that the request caused, or a 2xx, clears a failure.
fn failure(answer: &Answer) -> Option<String> {
    // Inspect, Harbor and the OpenHands benchmarks keep out of their scores an agent whose model
    // calls failed past their retries. The route fixes the path and the deployment the model, so
    // a redirect, a 402 or a 404 (`model_not_found`) is the deployment's fault, as Harbor and
    // LiteLLM treat a 404; the other 4xx are the request's.
    match answer {
        Answer::Forwarded { status, .. } => match status {
            0..=299 | 400 | 405..=407 | 409..=428 | 430..=499 => None,
            300..=399 | 401..=404 | 408 | 429 | 500..=u16::MAX => Some(status.to_string()),
        },
        Answer::Cut { status, .. } => Some(format!("{status}, cut short")),
        Answer::Refused {
            upstream_status: Some(status @ (401 | 403)),
            ..
        } => Some(status.to_string()),
        Answer::Refused {
            upstream_status: Some(status),
            ..
        } => Some(format!("{status}, coded")),
        Answer::Refused {
            upstream_status: None,
            status,
            ..
        } => Some(format!("{status}, no answer")),
    }
}

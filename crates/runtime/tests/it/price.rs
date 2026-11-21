//! What a model's tokens cost at its prices, as `provider.json` keeps them: [`TokenPrices::usd`].

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use std::collections::BTreeMap;

use expect_test::{Expect, expect};
use runtime::{ModelUse, TokenPrices, Tokens};
use schema::ir::Api;
use serde_json::{Value, json};

/// A case: prices, and the tokens `input`, `cached`, `cache_write` and `output`.
type Case = (Value, u64, Option<u64>, Option<u64>, u64);

/// Shows what each case's tokens cost at its prices.
fn check(cases: impl IntoIterator<Item = Case>, expect: &Expect) {
    let lines = cases
        .into_iter()
        .map(|(prices, input, cached, cache_write, output)| {
            let tokens = Tokens {
                reported: 1,
                input,
                output,
                cached,
                cache_write,
                reasoning: Some(output),
            };
            let priced: TokenPrices = serde_json::from_value(prices.clone()).unwrap();
            let cost = priced
                .usd(&tokens)
                .map_or("no cost".to_owned(), |usd| format!("${usd}"));
            let key = (Api::Responses, "model".to_owned());
            let usage = ModelUse {
                by_model: BTreeMap::from([(key, tokens)]),
                ..ModelUse::default()
            };
            let estimate = usage.usd(&BTreeMap::from([("model".to_owned(), priced)]));
            let estimate = if estimate.unpriced.is_empty() {
                let usd = estimate.usd.unwrap();
                assert_eq!(cost, format!("${usd}"));
                String::new()
            } else {
                assert_eq!(estimate.unpriced, ["model"]);
                estimate.usd.map_or_else(
                    || ", unknown estimate".to_owned(),
                    |usd| format!(", priced subtotal ${usd}"),
                )
            };
            format!(
                "at {prices}, input {input}, cached {cached:?}, cache_write {cache_write:?}, \
             output {output}: {cost}{estimate}"
            )
        });
    expect.assert_eq(&lines.collect::<Vec<_>>().join("\n"));
}

#[test]
fn usd() {
    let responses = json!({ "input": 1.25, "cached": 0.125, "output": 10 });
    let messages = json!({ "input": 3, "cached": 0.3, "cache_write": 3.75, "output": 15 });
    let uncached = json!({ "input": 1.25, "output": 10 });
    check(
        [
            // A Responses answer, whose cached input is within its input.
            (responses.clone(), 12, Some(8), None, 5),
            // A Messages answer, whose input counts what was read from and written to the cache.
            (messages.clone(), 2312, Some(2000), Some(300), 5),
            (messages, 2000, Some(2000), Some(300), 5),
            (uncached.clone(), 12, Some(0), None, 5),
            (uncached.clone(), 12, Some(8), None, 5),
            (uncached.clone(), 12, None, Some(8), 5),
            (responses.clone(), 12, Some(3), Some(8), 5),
            (uncached, 8, Some(8), None, 0),
            (json!({ "input": 0, "output": 0 }), 9, Some(8), None, 0),
            (responses, 0, None, None, 0),
        ],
        &expect![[r#"
            at {"cached":0.125,"input":1.25,"output":10}, input 12, cached Some(8), cache_write None, output 5: $0.000056
            at {"cache_write":3.75,"cached":0.3,"input":3,"output":15}, input 2312, cached Some(2000), cache_write Some(300), output 5: $0.001836
            at {"cache_write":3.75,"cached":0.3,"input":3,"output":15}, input 2000, cached Some(2000), cache_write Some(300), output 5: no cost, unknown estimate
            at {"input":1.25,"output":10}, input 12, cached Some(0), cache_write None, output 5: $0.000065
            at {"input":1.25,"output":10}, input 12, cached Some(8), cache_write None, output 5: no cost, priced subtotal $0.000055
            at {"input":1.25,"output":10}, input 12, cached None, cache_write Some(8), output 5: no cost, priced subtotal $0.000055
            at {"cached":0.125,"input":1.25,"output":10}, input 12, cached Some(3), cache_write Some(8), output 5: no cost, priced subtotal $0.000051625
            at {"input":1.25,"output":10}, input 8, cached Some(8), cache_write None, output 0: no cost, unknown estimate
            at {"input":0,"output":0}, input 9, cached Some(8), cache_write None, output 0: no cost, priced subtotal $0
            at {"cached":0.125,"input":1.25,"output":10}, input 0, cached None, cache_write None, output 0: $0"#]],
    );
}

//! What a model's tokens cost: the prices that the operator's settings state, in US dollars a
//! million tokens of each kind, and the one computation from a model's tokens to their cost.
//!
//! The kinds and the arithmetic are `LiteLLM`'s: input is priced by whether the upstream read it
//! from its cache, wrote it to its cache, or neither, and reasoning as output,
//! <https://github.com/BerriAI/litellm/blob/df78b16941697991a736fc224182fa17270420ed/litellm/litellm_core_utils/llm_cost_calc/utils.py#L1642>.
//! The unit is the one providers publish, as Inspect AI's `ModelCost` takes it,
//! <https://github.com/UKGovernmentBEIS/inspect_ai/blob/7cf09c929b3ea9138762ca9e4d8d23f119554ee6/src/inspect_ai/model/_model_data/model_data.py#L8-L29>.

use serde::{Deserialize, Serialize};

use crate::Tokens;

/// The most a price may be, in US dollars a million tokens, as [`schema::workflow::UsdPerHour`]
/// bounds a sandbox's.
const MAX_USD_PER_MTOK: f64 = 1e6;

/// What a model's tokens cost, each kind in US dollars a million tokens, from 0 to a million.
///
/// `input` prices the input that the upstream neither read from its cache nor wrote to it;
/// `cached` and `cache_write`, if given, the input it read from its cache and wrote to it; and
/// `output` the output, the reasoning's included.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(try_from = "Stated")]
pub struct TokenPrices {
    input: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    cached: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_write: Option<f64>,
    output: f64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Stated {
    input: f64,
    cached: Option<f64>,
    cache_write: Option<f64>,
    output: f64,
}

impl TryFrom<Stated> for TokenPrices {
    type Error = String;

    fn try_from(stated: Stated) -> Result<Self, Self::Error> {
        let Stated {
            input,
            cached,
            cache_write,
            output,
        } = stated;
        let price = |kind: &str, usd: f64| {
            // Refuses NaN, too, which no range contains.
            if (0.0..=MAX_USD_PER_MTOK).contains(&usd) {
                Ok(usd)
            } else {
                Err(format!(
                    "`{kind}` is {usd}, not a price in US dollars a million tokens: give one from \
                     0 to 1000000, such as 1.25"
                ))
            }
        };
        Ok(Self {
            input: price("input", input)?,
            cached: cached.map(|usd| price("cached", usd)).transpose()?,
            cache_write: cache_write
                .map(|usd| price("cache_write", usd))
                .transpose()?,
            output: price("output", output)?,
        })
    }
}

impl TokenPrices {
    /// What `tokens`, a model's, cost in US dollars at these prices; `None` if a kind that they
    /// count tokens of has no price, or they count more input read from and written to the
    /// cache than input. A count that no response gave is 0.
    pub fn usd(&self, tokens: &Tokens) -> Option<f64> {
        let Tokens {
            reported: _,
            input,
            output,
            cached,
            cache_write,
            reasoning: _,
        } = *tokens;
        let (cached, cache_write) = (cached.unwrap_or(0), cache_write.unwrap_or(0));
        let uncached = input.checked_sub(cached)?.checked_sub(cache_write)?;
        // A kind without a price costs nothing only if none of its tokens were used.
        let cost = |count: u64, price: Option<f64>| match (count, price) {
            (0, _) => Some(0.0),
            (_, None) => None,
            #[expect(
                clippy::cast_precision_loss,
                reason = "exact below 2^53 tokens, past what any run uses"
            )]
            (count, Some(price)) => Some(count as f64 * price),
        };
        let usd = cost(uncached, Some(self.input))?
            + cost(cached, self.cached)?
            + cost(cache_write, self.cache_write)?
            + cost(output, Some(self.output))?;
        Some(usd / 1e6)
    }
}

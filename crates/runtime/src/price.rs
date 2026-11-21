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

/// Prices of every kind of token, as [`TokenPrices`] holds them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AllPrices {
    input: f64,
    cached: f64,
    cache_write: f64,
    output: f64,
}

impl AllPrices {
    /// As [`TokenPrices::usd`].
    pub(crate) fn usd(self, tokens: &Tokens) -> Option<f64> {
        let Self {
            input,
            cached,
            cache_write,
            output,
        } = self;
        let prices = TokenPrices {
            input,
            cached: Some(cached),
            cache_write: Some(cache_write),
            output,
        };
        prices.usd(tokens)
    }
}

/// The cost in US dollars of the reported tokens whose prices are known.
pub(crate) enum TokenCost {
    Complete(f64),
    Partial(f64),
    Unknown,
}

impl TokenPrices {
    /// These prices, if they price every kind of token; else the kinds they give no price, by
    /// their names in the configuration, as a list for a message (`` `cached` or `cache_write` ``).
    pub(crate) fn all(&self) -> Result<AllPrices, &'static str> {
        let Self {
            input,
            cached,
            cache_write,
            output,
        } = *self;
        match (cached, cache_write) {
            (Some(cached), Some(cache_write)) => Ok(AllPrices {
                input,
                cached,
                cache_write,
                output,
            }),
            (None, Some(_)) => Err("`cached`"),
            (Some(_), None) => Err("`cache_write`"),
            (None, None) => Err("`cached` or `cache_write`"),
        }
    }

    /// What `tokens`, a model's, cost in US dollars at these prices; `None` if a kind that they
    /// count tokens of has no price, or they count more input read from and written to the
    /// cache than input. A count that no response gave is 0.
    pub fn usd(&self, tokens: &Tokens) -> Option<f64> {
        match self.cost(tokens) {
            TokenCost::Complete(usd) => Some(usd),
            TokenCost::Partial(_) | TokenCost::Unknown => None,
        }
    }

    pub(crate) fn cost(&self, tokens: &Tokens) -> TokenCost {
        let Tokens {
            reported: _,
            input,
            output,
            cached,
            cache_write,
            reasoning: _,
        } = *tokens;
        let (cached, cache_write) = (cached.unwrap_or(0), cache_write.unwrap_or(0));
        let Some(uncached) = input
            .checked_sub(cached)
            .and_then(|remaining| remaining.checked_sub(cache_write))
        else {
            return TokenCost::Unknown;
        };
        let kinds = [
            (uncached, Some(self.input)),
            (cached, self.cached),
            (cache_write, self.cache_write),
            (output, Some(self.output)),
        ];
        let mut subtotal = None;
        let mut complete = true;
        for (count, price) in kinds {
            if count == 0 {
                continue;
            }
            if let Some(price) = price {
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "exact below 2^53 tokens, past what any run uses"
                )]
                let amount = count as f64 * price;
                *subtotal.get_or_insert(0.0) += amount;
            } else {
                complete = false;
            }
        }
        match (complete, subtotal) {
            (true, sum) => TokenCost::Complete(sum.unwrap_or(0.0) / 1e6),
            (false, Some(sum)) => TokenCost::Partial(sum / 1e6),
            (false, None) => TokenCost::Unknown,
        }
    }
}

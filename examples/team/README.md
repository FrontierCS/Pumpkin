# Two agents score public LWE predictions

Alice and Bob compute two sample values of the Learning With Errors (LWE) equation. All
inputs are public, including the vector `[2, 3]` and both errors. For a row `[a, b]` and
error `e`, the prediction is `(2*a + 3*b + e) % 13`.

| Row | Coefficients | Offset | Prediction |
| --- | --- | --- | --- |
| amber | 3, 4 | −1 | 4 |
| blue | 2, 5 | 1 | 7 |

Alice posts the amber prediction, selects her answer and finishes. Bob reads her authenticated
message, checks the arithmetic, sends an acknowledgement and adds the blue prediction. One
sandbox hosts the board across both participants' calls. The judge reads each role's selected
JSON file and takes the highest individual score.

Copy the whole `examples/` directory into a task repository with its Pumpkin launcher.

## Run two Codex agents

Set `MODEL` to a configured Responses API model that supports the stock Codex harness's native
tools. Each role has its own sandbox, prompt and $0.25 model budget. Both agents share a
five-minute work deadline.

```sh
task=examples/team/codex.star
./pumpkin check "$task" --arg "model=${MODEL:?set MODEL}" &&
team=$(./pumpkin run "$task" --arg "model=$MODEL") &&
./pumpkin wait --outcome "$team" &&
python3 examples/team/verify.py ".pumpkin/runs/$team"
```

The checker requires successful Responses output for each agent, a post from Alice that Bob
reads and acknowledges, both agents' selection calls, one judge result and four sandbox deletion
receipts. It reports the scores earned by the selected predictions. A wrong integer prediction can
yield a successful check and a lower score. An answer with malformed JSON or the wrong row
names fails the check.

## Run the scripted reference

The reference needs no model. It also checks that the board refuses a forged sender and that
each agent can use only its own selection grant. The default run scores Alice 1 and Bob 2.

```sh
task=examples/team/task.star
./pumpkin check "$task" &&
team=$(./pumpkin run "$task") &&
./pumpkin wait --outcome "$team" &&
python3 examples/team/verify.py ".pumpkin/runs/$team"
```

To see Bob leave without selecting, give the run and its checker the same case.

```sh
missing=$(./pumpkin run examples/team/task.star --arg omit=bob) &&
./pumpkin wait --outcome "$missing" &&
python3 examples/team/verify.py ".pumpkin/runs/$missing" --omit bob
```

Use `--arg omit=all` and `--omit all` to see the judge receive two missing selections. Each
missing role has a null score. If all selections are missing, the team score is null.

For the reference, the checker also requires the exact selected predictions and clean exits
from agents that omit their answers.

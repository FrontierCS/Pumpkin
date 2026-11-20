# Two agents score public predictions

Alice and Bob predict the residue of a dot product with a public offset. For a row
`[a, b]` and offset `e`, the answer is `(2*a + 3*b + e) % 13`.

| Row | Coefficients | Offset | Prediction |
| --- | --- | --- | --- |
| amber | 3, 4 | −1 | 4 |
| blue | 2, 5 | 1 | 7 |

Alice posts the amber prediction, selects her answer and finishes. Bob reads her authenticated
message, checks the arithmetic, sends an acknowledgement and adds the blue prediction. One
sandbox hosts the board across both participants' calls. The agents also check that the board
refuses a forged sender and that each can use only its own selection grant.

## Run and check the record

Copy the whole `examples/` directory into a task repository with its Pumpkin launcher. The
agents are scripted and need no model. The default run scores Alice 1 and Bob 2, with a team
score of 2.

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

The checker reads the selected bytes by their recorded digests, matches Alice's post to Bob's
read and acknowledgement, and checks clean exits for omitted selections. It verifies the ordered
scores, one final call and each sandbox's deletion against its physical receipt. It reports the
observed order of Alice locking her selection and Bob reading her message. Model routing,
independent model usage and provider capacity require a workflow with live model sessions on
the intended provider.

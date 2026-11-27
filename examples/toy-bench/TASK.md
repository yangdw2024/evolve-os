# Task: integer classifier

Edit `solution.py`. `classify(n)` receives a positive integer and returns a bool.
The verifier compares it with a hidden rule over a fixed set of integers.

Scoring (deterministic, higher is better):

- `primary_score = accuracy - 0.001 * loc`
- `loc` counts non-blank lines that are not comments (removing comments does NOT change loc)
- you are also evaluated on hidden integers from a different range, so do not hard-code answers

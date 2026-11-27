"""Train verifier: integers 1..200, diagnostic feedback enabled."""
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _bench import evaluate  # noqa: E402

evaluate(list(range(1, 201)), with_feedback=True, pass_threshold=0.5)

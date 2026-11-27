"""Holdout verifier: disjoint integers 1000..1400, NO feedback, stricter threshold."""
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _bench import evaluate  # noqa: E402

evaluate(list(range(1000, 1401)), with_feedback=False, pass_threshold=0.75)

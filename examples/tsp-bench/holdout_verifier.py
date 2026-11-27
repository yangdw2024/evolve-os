"""Holdout verifier: disjoint instances (seeds 1000..1011), NO feedback."""
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _bench import evaluate  # noqa: E402

evaluate(list(range(1000, 1012)), with_feedback=False, pass_ratio=0.5)

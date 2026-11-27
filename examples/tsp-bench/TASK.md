# Task: Euclidean TSP heuristic

Edit `solution.py`. `solve(points)` receives a list of `(x, y)` float tuples (25–60 points,
coordinates in [0, 1000]) and must return a permutation of `range(len(points))` describing
a closed tour.

Scoring (deterministic, higher is better):

- per instance: `ratio = nearest_neighbour_tour_length / your_tour_length`
  (1.0 means you match the greedy nearest-neighbour baseline; >1.0 beats it)
- `primary_score = mean(ratio) - 0.0002 * loc` where `loc` counts non-blank, non-comment lines
- an invalid permutation, crash or timeout scores 0; the whole evaluation must finish within
  a few seconds of wall-clock time (pure Python, standard library only)

You are scored on hidden instances too, so do not special-case inputs.
Good directions: nearest-neighbour construction, 2-opt / Or-opt local search, multiple starts.

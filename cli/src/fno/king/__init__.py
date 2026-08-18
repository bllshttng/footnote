"""The king's board: the one input every arm of the king loop reads.

A target driver asks whether its one deliverable shipped. A king driver asks
whether the board is clean, which is a queue-empty read over six sources that
existing verbs already answer. This package owns that read; the loop arms in
``crates/fno-agents`` consume it and decide nothing on their own.
"""

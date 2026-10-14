"""Shared Kev option vocabulary for GiTrust routing.

Loaded from gitrust/src/kev_ops.json, the same file the binary embeds, so the
eval, the training data and the shipped router can never drift apart.
"""
import json, os

_DOC = json.load(open(os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                   "..", "..", "gitrust", "src", "kev_ops.json")))
OPS = {name: desc for name, desc in _DOC["ops"]}  # insertion order = JSON order
INSTRUCTIONS = _DOC["instructions"]
# Mirrors kev::DESTRUCTIVE_OPS in gitrust/src/kev.rs (incl. in-progress skip/abort).
DESTRUCTIVE = {"cleanup", "discard_local", "force_push", "squash", "amend", "undo", "revert", "delete_repo",
               "rebase_skip", "rebase_abort", "merge_abort", "cherry_pick_abort", "revert_abort"}
IN_PROGRESS = _DOC["in_progress"]

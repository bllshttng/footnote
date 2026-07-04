"""Skill-diff proposer: observer failure patterns -> cited SKILL.md diff -> PR.

Consumes the observer harness's ``skill_eval_run_complete`` /
``skill_eval_finding`` events, synthesizes an evidence-cited diff to a skill's
files, and ships it as a normal PR through the normal review gate (a human
always merges - this loop is assisted, never unattended). When no diff helps
(local-maxima ceiling, or every hunk drops as uncited) it files a backlog node
instead. All coordination routes through events.jsonl - no new state files.
"""

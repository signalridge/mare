# Sources of the eval fixtures

Fixtures for the tasks under `tasks/`. The two codexradar tasks use the task pack's files byte-for-byte (not rewritten):

- `qin-shi-huang/instruction.md` = https://codexradar.com/assets/local-task-pack/combined.txt
  ("秦始皇": a detailed SVG saved as `result.svg` plus a variant of the candy question; expected
  answer 21 = 9 round + 12 star, verified by exhaustive search — the same program gives 15 for the
  original candy question).
- `pompeii/instruction.md` = https://codexradar.com/assets/local-task-pack/pompeii.txt (the same text
  is the `README.md` inside the zip).
- `pompeii/environment/pompeii-rp040.zip` =
  https://codexradar.com/assets/local-task-pack/pompeii-rp040.zip (`question.png`,
  `output_schema.json`, a placeholder `model_answer.json`, `README.md`), unzipped into `/app` at
  image build time.
- `pompeii/tests/pompeii-gold.json` — the gold adjacency (`gold_edges`, 15 edges over 9 fragments)
  and `labels` from codexradar's public `/data/pompeii-rp040-comparison.json`; its `source` points
  at the original dataset record https://zenodo.org/records/15800029 (puzzle_0000041_RP_group_40).
  It is only in `tests/`, which the agent never sees.

The `tools-*` tasks are mare's own, modeled on DeepSeek Harness
`apps/cli/tests/profiles/headless/tests/real-model.e2e.ts` (temporary workspace, files checked from
outside the agent; prompts for items 1 and 6 follow its wording).

# Task Benchmarking and Review

`statsai task benchmark` measures the current deterministic grouper against
simple baselines using local manual verifications as ground truth. It is a
local evaluation tool, not a sync feature.

## What It Measures

The benchmark scores the raw grouper output and then separately checks whether
rebuilt user-facing work items still preserve explicit manual constraints.

Run it with:

```sh
cargo run -p statsai -- task benchmark
cargo run -p statsai -- task benchmark --json
```

The report includes:

- adjacent continuation precision, recall, and F1
- cluster precision, recall, and F1 on verified spans
- meta rejection precision, recall, and F1
- whether manual split and merge constraints are preserved
- whether the current grouper beats all shipped baselines

## Building Useful Ground Truth

The benchmark only becomes meaningful after you record local manual truth with
`task verify ...`.

Recommended loop:

1. Run `scan` to persist spans and rebuild work items.
2. Use `task list` and `task show --include-evidence` to inspect candidates.
3. Record `accept`, `reject`, `split`, `merge`, and `rename` actions.
4. Re-run `task benchmark`.

Two top-level fields tell you how complete the local truth set is:

- `has_verified_ground_truth`: at least one verified span exists.
- `has_verified_pairwise_ground_truth`: at least one adjacent verified span pair exists.

Pairwise ground truth usually requires either:

- verifying a multi-span work item
- recording a split
- recording a merge

Without pairwise ground truth, `task benchmark` still runs, but it should not be
treated as a shipping gate.

## Reading the Shipping Gate

The benchmark report exposes these gate signals:

- `manual_constraints_preserved`: every explicit split or merge constraint still holds after rebuild.
- `beats_all_baselines`: the current grouper's adjacent F1 is strictly better than every baseline.
- `shipping_gate_ready`: no gate blocker remains.
- `failing_baselines`: baseline names the current grouper did not beat.
- `gate_blockers`: machine-readable blockers such as `missing_verified_ground_truth`, `missing_pairwise_ground_truth`, `manual_constraints_not_preserved`, and `baseline_regressions`.

The shipped baselines are:

- `gap_only_2h`
- `gap_only_6h`
- `gap_only_12h`
- `gap_only_24h`
- `repo_plus_title`
- `repo_plus_branch_plus_title`

## Status and Review Signals

Benchmarking works best when the review queue is healthy. `task list` and
`task show` surface work items that are:

- `auto`
- `needs_review`
- `verified`
- `rejected_meta`

Items commonly fall into `needs_review` when they have weak or mixed evidence,
for example:

- no git anchor
- generic or weak titles
- low title specificity inside the project bucket
- cross-provider merges
- low-signal exchanges
- multi-day continuation without a strong anchor

## Method Notes

The current grouper is intentionally:

- local-only
- deterministic
- explainable

Instead of a learned model, it combines continuity signals such as project
bucket, branch family, session/thread continuity, normalized title overlap,
todo overlap, time gap, and title specificity or phraseness inside a local
project bucket.

Manual verification is part of evaluation, not part of the scored model output.
That means benchmark metrics reflect the raw grouper prediction, while manual
constraints are checked separately as a correctness boundary.

## Literature Notes

The current heuristics are informed by a few research directions rather than a
single paper or trained model:

- topic segmentation with lexical cohesion and conversational structure
- conversation disentanglement and boundary detection
- unsupervised keyphrase extraction and title ranking

Representative references:

- Marti Hearst, *Multi-Paragraph Segmentation of Expository Text*
- Shafiq Rayhan Joty, Giuseppe Carenini, Raymond T. Ng, *Topic Segmentation and Labeling in Asynchronous Conversations*
- Reshmi Ghosh et al., *Topic Segmentation in the Wild: Towards Segmentation of Semi-structured & Unstructured Chats*
- Jonathan K. Kummerfeld et al., *A Large-Scale Corpus for Conversation Disentanglement*
- Marina Danilevsky et al., *KERT: Automatic Extraction and Ranking of Topical Keyphrases from Content-Representative Document Titles*
- Kamil Bennani-Smires et al., *Simple Unsupervised Keyphrase Extraction using Sentence Embeddings*
- Xinnian Liang et al., *Unsupervised Keyphrase Extraction by Jointly Modeling Local and Global Context*

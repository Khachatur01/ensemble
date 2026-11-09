# Event-stream fixtures

One file per CLI version: `<cli>/<version>.jsonl`, the lines the CLI prints in
headless JSON mode, saved as they came. The unit tests in `src/external/`
replay every file here through the adapter's parser and check that it still
finds the answer, the model and the steps.

`synthetic.jsonl` is written by hand from the event shapes the adapters read.
It is not a capture. When a real CLI changes its output, `tests/real_cli.rs`
fails first; capture the new stream into `<cli>/<version>.jsonl` and fix the
parser until both the new and the old files pass.

Capture:

    claude -p "reply with the word ok" --output-format stream-json --verbose > tests/fixtures/claude-code/<version>.jsonl
    opencode run --format json "reply with the word ok" > tests/fixtures/opencode/<version>.jsonl

Remove anything private (paths, keys) from a capture before committing it.

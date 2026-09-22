# DCO fixtures

Fixture commit messages for `tools/quality/check_dco.sh --self-test`, in the
shape `tools/lint/fixtures/` uses for the tier lint: every `good_*.txt` must be
accepted and every `bad_*.txt` must be rejected.

A fixture is a commit message with one extra first line, `Author: Name <email>`,
naming who the commit would have been authored by. The script strips that line
before applying its rules, so the rest of the file is exactly what
`git log -1 --format=%B` would print.

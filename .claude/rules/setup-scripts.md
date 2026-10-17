---
paths:
  - ".claude/skills/*/scripts/**"
---

# Setup scripts

- A script runs from anywhere in the checkout, and every path in the setup's text names it from
  the repository root.
- A success message states what was compared, never more than that.
- A `--self-test` plants each defect class the script exists for, and the hostile input it
  reads: spaces and non-ASCII in names, renames, an unresolvable base, an inherited `INSTA_*` or
  `CI`, a global git config, `.git/info/exclude`, `$XDG_CONFIG_HOME/git/ignore`.
- Each guard a self-test exists for (an override flag, a source filter, a negation filter, an
  exit-code check) has a case that fails when that guard alone is removed.
- A script that rewrites tracked files touches only what it proved unchanged in content,
  validates every path it is given, and restores what it touched when it fails.
- A missing tool is NOT RUN with its reason, never a traceback and never a pass.

# 0004. Contribution and landing model

Status: accepted
Date: 2026-10-14

## Context

- Organisation rulesets on `main` require `ci / required` and linear, unforced history, but their admin bypass has merged an unchecked commit.
- Every push publishes for good and runs the workflows it carries.
- Contributions come under a one-way grant and DCO 1.1.
- `reuse lint` accepts a wrong expression.

## Decision

1. Every change lands as a candidate `land/owner/<slug>` rebased onto `main`, which fast-forwards to it once its `ci / required` passes. At most three are in flight; a red one is fixed and landed anew, never re-run.
2. A repository ruleset on `main` without bypass actors repeats the required check, linear history and no force-push or deletion: bypass is per ruleset and the strictest rule applies (https://github.com/github/docs/blob/bcb11fe40765ec7935bdcff0da8cd3cee0bfba50/content/repositories/configuring-branches-and-merges-in-your-repository/managing-rulesets/about-rulesets.md#L19-L64). Only rebase merge stays on, unused; Update branch is on.
3. Step 0: a run receives only the secrets its workflow references (https://github.com/github/docs/blob/bcb11fe40765ec7935bdcff0da8cd3cee0bfba50/content/actions/concepts/security/secrets.md#L20); only `release.yml`'s `publish-crates` does, the `crates-io` environment's `CRATES_IO_PUBLISH_NEW_TOKEN` for the first final tag. Before the first that does and the first release, organisation Actions and Dependabot secrets visible to all repositories narrow to selected private ones. The owner decides which organisation Apps keep access. No App or agent acts on issue or pull-request events.
4. Workflow `ci` runs on `pull_request` and `push` to `land/owner/**`, never `pull_request_target`. Final job `required` always runs and fails unless every need succeeded, as a skipped job reports success (https://github.com/github/docs/blob/bcb11fe40765ec7935bdcff0da8cd3cee0bfba50/data/reusables/actions/workflows/skipped-job-status-checks-passing.md). `ci.yml` cancels its ref's in-progress runs on every event.
5. Fork runs are approved after reading their gate-surface diff. An accepted pull request, kept current by Update with rebase, lands at its exact sha as `land/owner/pr-<n>`, which marks it merged (https://github.com/github/docs/blob/bcb11fe40765ec7935bdcff0da8cd3cee0bfba50/content/pull-requests/reference/pull-request-merges.md#L65-L69); without maintainer edits it is rebased, keeping author and trailers, and closed with a link. Fork changes to `.github/` land as maintainer rewrites. A Dependabot update becomes the maintainer's own signed-off candidate; its pull request closes with a link.
6. Every maintainer change is independently reviewed before its push, and lands without waiting for the owner, who delegates development and landing to the maintaining agent. A gate-surface diff (`.github/`, `.cargo/`, `Makefile`, `*.toml`, lockfiles, `build.rs`, `fuzz/`, `.git*`, licensing files and every script a gate target runs) is reviewed for what each gate it touches refuses before and after it.
7. Job `dco` requires each commit a pull request or candidate adds to carry its author's `Signed-off-by` and a subject of at most 72 columns alone on its first line, a blank line or the message's end after it, and to name no assistant (`claude`, `anthropic` or `llm` between non-letters) as its author or committer, except, on a pull request, commits by `dependabot[bot]`. The maintainer's `commit-msg` hook refuses, never adds, a missing sign-off.
8. Gate jobs run `make` targets, so `make ci` is the local pre-flight, reporting what the host cannot run as not run.
9. The `hygiene` target, in job `policy` (ADR 0003 decision 2), refuses home paths, private addresses, personal e-mail addresses except the licensor's published contact and third-party licence texts, and tracked ignored files in the tree and branch name; the maintainer's pre-push hook scans each push for secrets and private patterns. Public trailers are `Signed-off-by`, `Co-authored-by`, `Fixes` and `Tier` (change risk); the maintainer's other trailers and branches stay local. A leak reachable from `main` stays public (https://github.com/github/docs/blob/bcb11fe40765ec7935bdcff0da8cd3cee0bfba50/content/authentication/keeping-your-account-and-data-secure/removing-sensitive-data-from-a-repository.md#L149-L158): fix forward, delete logs, rotate credentials.
10. The Claude Code setup (`CLAUDE.md`, `.claude/`) is tracked and holds no machine or personal fact. Local files (`CLAUDE.local.md`, `.claude/settings.local.json`, `.worktreeinclude`, and Claude Code's `.claude/worktrees` and `.claude/agent-memory-local`) and every other assistant's files stay local: `.gitignore` names their directories without a trailing slash, which also ignores a symlink.
11. Outside code its author has not signed off (an unsigned patch, a snippet from an issue) is rewritten test-first, signed off under DCO clause (b), and its author credited by `Co-authored-by`. REUSE.toml's MIT-bearing annotations (`MIT`, `BUSL-1.1 AND MIT`) keep their paths and copyright lines while the files exist: `make dco`, in job `dco`, resolves each file in the root REUSE.toml, whose annotations all override, at the merge base with `main` and in the tree, and refuses a file that leaves an MIT-bearing annotation or whose annotation loses a copyright line, an MIT-bearing annotation without `precedence = "override"`, and a file that comes under an `MIT`-only one it did not have; with no merge base it judges the tree's annotations alone.
12. Security fixes are prepared privately, landed at disclosure under a neutral slug and tagged after rehearsal; the advisory follows.
13. Releases are on-demand SemVer 0.x tags on a landed `main` sha, pushed with the maintainer's credential after reading the `.github/` diff since the previous tag. ADR 0003 owns the rest; environments allow self-review.

## Consequences

- After the root commit, every sha `main` moves to heads a green candidate run. Falsifier: a `main` push lacking that run.
- A stolen landing credential lands anything, as a candidate defines its judge; the pre-tag read, tag rulesets and environment approval stand before a registry; landed code outside `.github/` ships with the next tag. Push protection blocks known secret formats; only the pre-push scan stops a private pattern before publication.
- Dependabot pull requests and those from forks refusing maintainer edits show Closed, not Merged.
- Decision 2's ruleset, push protection and the hooks precede every candidate.
- No human reads a maintainer change before its push: a weakened gate its review misses publishes with that push, judges its own candidate and is fixed forward.

## Alternatives considered

- Button merges: unjudged shas.
- Merge queue: a second path into `main`.
- `cancel-in-progress: false`: delays the fresh verdict.
- Dependabot's sign-off as an exception on `main`: a rebase voids it, and Dependabot stops rebasing once commits are added (https://github.com/github/docs/blob/bcb11fe40765ec7935bdcff0da8cd3cee0bfba50/content/code-security/how-tos/secure-your-supply-chain/manage-your-dependency-security/manage-dependabot-prs.md#L37).
- Signatures as proof: GitHub signs any user's web-interface commit (https://github.com/github/docs/blob/bcb11fe40765ec7935bdcff0da8cd3cee0bfba50/content/authentication/managing-commit-signature-verification/about-commit-signature-verification.md#L91).
- A second list of MIT paths beside REUSE.toml: two records of one fact drift.
- Scripts checking settings, or checking that gate jobs run only `make`: nothing requires them.

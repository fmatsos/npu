---
name: release
description: Cuts an `npu` release — decides the next version from the Conventional Commits since the last tag, writes the `CHANGELOG.md` section (Keep a Changelog headings, one bullet per commit, each linking to its commit and the whole section closing on a compare link), bumps `Cargo.toml`, commits, tags `vX.Y.Z` and pushes, which is what triggers the Release workflow that builds the binaries and publishes the GitHub release. Use it to release, to prepare a changelog, or to check what a release would contain.
when_to_use: >
  Trigger on "release npu", "cut a release", "publish a version", "bump the
  version", "prepare the changelog", "what would go in the next release", or
  on any request naming a version number to ship.
model: inherit
effort: high
allowed-tools: Read Write Edit Grep Bash(git:*) Bash(make:*) Bash(gh:*) Bash(cargo:*)
---

# Releasing `npu`

A release is a tag. Everything after it is automated: pushing `vX.Y.Z` runs
`.github/workflows/release.yml`, which re-runs `make qa`, builds the eight
release archives and raw update binaries, generates `npu-update.json` with
their SHA-256 checksums, and publishes the GitHub release with the notes taken
from `CHANGELOG.md`. Your job is the part a machine cannot do — deciding the
version and describing the change.

## The sequence

1. **Refuse to start on a dirty or diverged tree.**

   ```sh
   git status --porcelain     # must be empty
   git rev-parse --abbrev-ref HEAD   # must be main
   git fetch && git status -sb       # must not be behind
   ```

   A release built on uncommitted work is unreproducible from the tag. Stop
   and say so rather than committing someone else's work in progress.

2. **`make qa` must pass locally first.** The workflow runs it again, but
   finding out after the tag is pushed means deleting a tag other people may
   already have fetched.

3. **List what is being released.**

   ```sh
   previous=$(git describe --tags --abbrev=0 2>/dev/null || true)
   git log --no-merges --format='%H%x09%s' ${previous:+$previous..}HEAD
   ```

   No previous tag (first release): the range is the whole history.

4. **Decide the version** — see *Choosing the number*.

5. **Write the `CHANGELOG.md` section** — see *The changelog*. Read the
   commit bodies, not just the subjects: the subject says what changed, the
   body says why it matters, and the changelog is read by someone who has
   neither.

6. **Bump the version and refresh the lockfile.**

   ```sh
   # Cargo.toml: version = "X.Y.Z"
   cargo update --workspace   # rewrites npu's own version in Cargo.lock
   ```

   `Cargo.lock` is committed and the workflow builds with `--locked`: a stale
   lockfile fails the release build, not the local one. The workflow also
   rejects a tag whose `vX.Y.Z` does not exactly match this package version;
   that invariant keeps `npu update` from repeatedly installing a binary that
   reports a different version from its release manifest.

   Also update the quoted `$ npu --version` console blocks in `README.md` and
   `docs/cli.md` to `npu X.Y.Z` — build the binary and paste what it actually
   prints, never reconstruct it by hand. `tests/docs_quote_the_binary.rs`
   enforces the README block against `CARGO_PKG_VERSION`; nothing enforces
   the `docs/cli.md` one, so it only stays correct if this step is done.

7. **Commit, tag, push.**

   ```sh
   git add Cargo.toml Cargo.lock CHANGELOG.md README.md docs/cli.md
   git commit -m "chore(release): vX.Y.Z"
   git tag -a vX.Y.Z -m "vX.Y.Z"
   git push origin main
   git push origin vX.Y.Z
   ```

   The tag is annotated, and pushed **after** the branch: a tag pointing at a
   commit that is not on `main` yet would start a build of something nobody
   can check out.

8. **Watch the workflow, and report the release URL.**

   ```sh
   gh run watch --exit-status   # or: gh run list --workflow=Release --limit 1
   gh release view vX.Y.Z --web
   ```

   The run is green or the release does not exist — never announce a release
   whose workflow you did not see finish.

## Choosing the number

`npu` is pre-1.0, so the major stays `0` and a breaking change bumps the
**minor**. Read the commits, not the diff:

| Since the last tag | Next |
| --- | --- |
| any `!` or `BREAKING CHANGE:` | minor (`0.1.3` -> `0.2.0`) |
| at least one `feat:` | minor |
| only `fix:` / `perf:` | patch (`0.1.3` -> `0.1.4`) |
| only `docs:` / `chore:` / `test:` / `ci:` | patch, and say out loud that the binary is unchanged |

Anything that changes the **exit-code contract**, the **stdout contract**, a
**reserved name**, or a configuration key's meaning is breaking, whatever
its commit type says. Those are the CLI's API — the frontmatter delimiter
moving from `+++` to `---` was breaking even though it was committed as
`feat`.

## The changelog

`CHANGELOG.md` at the repository root, newest version first. The release
workflow extracts the section whose heading is `## [X.Y.Z]` and publishes it
verbatim as the release notes, so what you write here *is* what readers get.

```markdown
# Changelog

Every notable change to `npu`, newest first. Versions follow
[semantic versioning](https://semver.org); pre-1.0, a breaking change bumps
the minor.

## [0.2.0] - 2026-09-21

### Added

- `npu serve`, `stop`, `status` and `logs`: the container lifecycle of a
  backend declaring a `[docker]` table
  ([`aef66c0`](https://github.com/fmatsos/npu/commit/aef66c03fbe6d2b08701b5370480fd1c0c1ce299))
- `--verbose error|warn|info`, global, default `warn`; no level ever changes
  stdout
  ([`aef66c0`](https://github.com/fmatsos/npu/commit/aef66c03fbe6d2b08701b5370480fd1c0c1ce299))

### Changed

- **Breaking**: command frontmatter is fenced by `---` instead of `+++`; a
  file still using `+++` is rejected with a message naming both delimiters
  ([`aef66c0`](https://github.com/fmatsos/npu/commit/aef66c03fbe6d2b08701b5370480fd1c0c1ce299))

### Fixed

- `npu-backend`'s skill description, cut short by an unquoted YAML scalar
  ([`03b5ee5`](https://github.com/fmatsos/npu/commit/03b5ee5cddf07a7ec3cbe66c5032230d03c38ec8))

**Full changelog**: [`v0.1.0...v0.2.0`](https://github.com/fmatsos/npu/compare/v0.1.0...v0.2.0)
```

Rules, in order of how often they are got wrong:

- **Every bullet links to its commit.** Short sha as the label, full sha in
  the URL (`https://github.com/fmatsos/npu/commit/<full-sha>`) — a short sha
  can become ambiguous as the repository grows, a full one never does. A
  change spread over several commits links to each of them.
- **The section ends with the compare link**, `<previous tag>...<new tag>`,
  so a reader can see everything that is not worth a bullet.
- **Headings are Keep a Changelog's**, and only the ones you need:
  `Added`, `Changed`, `Deprecated`, `Removed`, `Fixed`, `Security`.
  Mapping from commit type: `feat` -> Added (Changed when it alters existing
  behaviour), `fix` -> Fixed, `refactor`/`perf`/`i18n` -> Changed,
  `docs`/`chore`/`test`/`ci` -> omitted unless a user would notice.
- **A breaking bullet starts with `**Breaking**:`** and says what to change
  in existing configuration, not just what moved.
- **Write for someone who does not read commits.** `feat: phase 4 —
  structured output` becomes "commands can declare a JSON Schema their
  output must satisfy; a violation exits `4`". Copying subjects verbatim
  produces a changelog nobody reads twice.
- The date is the release date, `YYYY-MM-DD`, in the heading.

## Pointing the bullets at the right commits

```sh
previous=$(git describe --tags --abbrev=0)
git log --no-merges --format='%H%x09%s' "$previous..HEAD"
```

Keep the full sha from the first column for the URL, cut it to seven
characters for the label. When a bullet covers several commits, list each
link rather than picking one — the point of the link is to find the change,
and half a change is worse than none.

## What this skill deliberately does not do

- **It does not generate the changelog from subjects.** A tool can do that,
  and the result reads like a tool wrote it. The commits are the source; the
  changelog is a rewrite for a different reader.
- **It does not publish to crates.io.** `Cargo.toml` carries
  `publish = false`. Changing that is a decision, not a release step.
- **It does not delete or move a tag.** A published tag is someone else's
  input. A mistake is fixed by the next version, with a `fix:` entry saying
  what was wrong.
- **It does not create the release by hand.** `gh release create` lives in
  the workflow, which also runs the gate. Creating one locally would skip
  `make qa` and publish binaries built on your machine, from a tree nobody
  checked.

## Reference

This skill is a summary. When a case is not covered here, or when the
behaviour it describes does not match what the repository does, the
workflows are authoritative:

- [`.github/workflows/release.yml`](https://github.com/fmatsos/npu/blob/main/.github/workflows/release.yml)
- [`.github/workflows/qa.yml`](https://github.com/fmatsos/npu/blob/main/.github/workflows/qa.yml)
- [Exit codes](https://github.com/fmatsos/npu/blob/main/README.md#exit-codes) — the contract a
  release must not break silently

Related skills: **npu-doctor** when the gate fails on a configuration issue.

<!-- model/effort: inherit/high — deciding a version and rewriting commits for a reader who has not seen them is judgement, and the tag it produces is not revocable. -->

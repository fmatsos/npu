# Claude Code skills

Seven [Agent Skills](https://code.claude.com/docs/en/skills): five that teach
Claude Code how to write and repair an `npu` configuration, and two that find
and export a model for the host's NPU. They are documentation Claude loads
only when it needs it — nothing runs at startup.

An eighth, [`release`](../.claude/skills/release/SKILL.md), lives directly in
`.claude/skills/` rather than here: it cuts a release of THIS repository and
is useless in any other project, so it is not part of what this directory
offers for installation.

| Skill | Covers |
| --- | --- |
| [`npu-config`](npu-config/SKILL.md) | the `.npu/` layout, scope precedence, merge semantics, bootstrapping a project |
| [`npu-backend`](npu-backend/SKILL.md) | `backends/*.toml` — `id`, `type`, `base_url`, `[operations.*]`, `[runtime]` (`type = "docker"` or `"process"`) |
| [`npu-model`](npu-model/SKILL.md) | `models/*.toml` — `id`, `backend`, `operation`, `model`, `[generation]` |
| [`npu-command`](npu-command/SKILL.md) | `commands/*.md` — frontmatter, input modes, `[args.*]`, templating, `[output]` |
| [`npu-doctor`](npu-doctor/SKILL.md) | reading `npu doctor`, the exit-code contract, `--verbose`, degraded mode, symptom → cause |
| [`npu-discover`](npu-discover/SKILL.md) | searching Hugging Face for models the host's Intel NPU can actually run, and only those |
| [`npu-export`](npu-export/SKILL.md) | exporting a Hugging Face model with `optimum-cli`/`optimum-intel`, verifying it, building the GPU twin, wiring both into model files |

`npu-config` routes to the three format skills; `npu-doctor` routes back to
whichever one owns the file that failed. `npu-discover` feeds a model id to
`npu-export`, which in turn writes the model files `npu-model` owns the format of.

## Model and effort

Each skill pins the effort its work actually needs, rather than inheriting a
session level chosen for something else.

| Skill | `model` | `effort` | Why |
| --- | --- | --- | --- |
| `npu-backend` | `sonnet` | `low` | five keys and a table of operations |
| `npu-model` | `sonnet` | `low` | five keys and two optional generation fields |
| `npu-command` | `sonnet` | `medium` | a prompt, an input mode and an output contract — judgement, and a templating mistake only surfaces at load time |
| `npu-config` | `sonnet` | `medium` | choosing a scope and reasoning about replacement is a design call |
| `npu-doctor` | `inherit` | `high` | diagnosis: read the report, form a hypothesis, test it against the exit code |
| `npu-discover` | `sonnet` | `medium` | judgement in choosing search terms and reading architecture-support docs, but no irreversible action |
| `npu-export` | `sonnet` | `medium` | a fixed procedure plus one judgement call (reading the sanity-check output); external commands are sometimes slow but the steps themselves are not ambiguous |

`npu-doctor` inherits deliberately — you picked the session model for the
debugging you are already doing. Both fields are one line each in the skill's
frontmatter if these defaults do not suit you.

Every skill ends with a **Reference** section linking back to the repository
documentation, which stays authoritative: a skill is a summary, and when the
two disagree the binary and `docs/` win. For the keys themselves, `npu config
schema backend|model|command|test` prints the JSON Schema derived from the
parser, so a skill points at it rather than restating the full format.

## Installing

Working **in this repository**, nothing to do: `.claude/skills/` already
symlinks all seven, so they load in every session here.

Elsewhere, per user, available in every project:

```sh
cp -r skills/npu-* ~/.claude/skills/
```

Or for one project only, so teammates get them with the repository:

```sh
mkdir -p .claude/skills && cp -r /path/to/npu/skills/npu-* .claude/skills/
```

Claude picks them up on the next session. `/npu-doctor` invokes one
explicitly; otherwise Claude loads whichever one matches what you asked for.

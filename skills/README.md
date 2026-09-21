# Claude Code skills

Five [Agent Skills](https://code.claude.com/docs/en/skills) that teach Claude
Code how to write and repair an `npu` configuration. They are documentation
Claude loads only when it needs it — nothing runs at startup.

| Skill | Covers |
| --- | --- |
| [`npu-config`](npu-config/SKILL.md) | the `.npu/` layout, scope precedence, merge semantics, bootstrapping a project |
| [`npu-backend`](npu-backend/SKILL.md) | `backends/*.toml` — `id`, `type`, `base_url`, `[operations.*]` |
| [`npu-model`](npu-model/SKILL.md) | `models/*.toml` — `id`, `backend`, `operation`, `model`, `[generation]` |
| [`npu-command`](npu-command/SKILL.md) | `commands/*.md` — frontmatter, input modes, `[args.*]`, templating, `[output]` |
| [`npu-doctor`](npu-doctor/SKILL.md) | reading `npu doctor`, the exit-code contract, degraded mode, symptom → cause |

`npu-config` routes to the three format skills; `npu-doctor` routes back to
whichever one owns the file that failed.

## Model and effort

Each skill pins the effort its work actually needs, rather than inheriting a
session level chosen for something else.

| Skill | `model` | `effort` | Why |
| --- | --- | --- | --- |
| `npu-backend` | `sonnet` | `low` | four keys and a table of operations |
| `npu-model` | `sonnet` | `low` | five keys and two optional generation fields |
| `npu-command` | `sonnet` | `medium` | a prompt, an input mode and an output contract — judgement, and a templating mistake only surfaces at load time |
| `npu-config` | `sonnet` | `medium` | choosing a scope and reasoning about replacement is a design call |
| `npu-doctor` | `inherit` | `high` | diagnosis: read the report, form a hypothesis, test it against the exit code |

`npu-doctor` inherits deliberately — you picked the session model for the
debugging you are already doing. Both fields are one line each in the skill's
frontmatter if these defaults do not suit you.

Every skill ends with a **Reference** section linking back to the repository
documentation, which stays authoritative: a skill is a summary, and when the
two disagree the binary and `docs/` win.

## Installing

Per user, available in every project:

```sh
cp -r skills/npu-* ~/.claude/skills/
```

Or for one project only, so teammates get them with the repository:

```sh
mkdir -p .claude/skills && cp -r /path/to/npu/skills/npu-* .claude/skills/
```

Claude picks them up on the next session. `/npu-doctor` invokes one
explicitly; otherwise Claude loads whichever one matches what you asked for.

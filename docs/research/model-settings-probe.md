# Model, effort and fast mode: how the tested CLIs take them (TOB-297 probe)

Run on 2026-11-02 with Claude Code 2.1.316 and codex-cli 0.166.0-alpha.5.1, by
`scripts/probe-model-settings.py`. Raw output: [model-settings-probe.json](model-settings-probe.json).

This probe spends real tokens. It ran on the user's own logins in a disposable git
repository, with one-word turns on the cheapest models the catalogs list and one Claude
Opus turn with fast mode asked for. Claude's API calls went through a local proxy
(`ANTHROPIC_BASE_URL`) that records only each call's model, `output_config.effort`,
`thinking` and `speed`, so the effort Claude actually sent is observed, not inferred.
Nothing else of the user's configuration is recorded.

## Claude Code 2.1.316

**Catalog.** The `initialize` answer that Keystone already reads for its model menu has,
per model, `supportsEffort`, `supportedEffortLevels` (for example
`["low","medium","high","xhigh","max"]`, or without `xhigh` for Opus 4.6 and Sonnet 4.6),
and `supportsFastMode` (only the Opus models). Haiku 4.5 has none of these. No default
effort is listed. The `default` entry carries the capabilities of the model it resolves to.
The answer also has `fast_mode_state` (`off`, `cooldown`, `on`) and
`fast_mode_disabled_reason`.

**At launch.**

- `--effort <level>` sets the effort. A `max` launch sent `effort: max`.
- Fast mode is a setting, not a flag: `--settings '{"fastMode":true}'` puts it in the flag
  settings layer. Without it, `initialize` reports `fast_mode_disabled_reason:
  sdk_opt_in_required`, so a process started for the SDK never runs fast on its own.
  With it, on Opus, `initialize` reported `fast_mode_state: on`.
- `--resume` with `--model` and `--effort` runs the resumed conversation on that model and
  effort.

**Mid-session, between turns.**

- `set_model` (control request) switches the model. The answer names the resolved model
  (`{"model":"claude-sonnet-5-5"}`), and the next turn's `system/init` and assistant
  messages report it. `default` resets to the session's default.
- `apply_flag_settings` with `{"effortLevel": …}` changes the effort of the next API call:
  `high` sent `high`, and `max` sent `max`. `get_settings` reports `high` but drops `max`
  (its settings schema stops at `xhigh`), so `get_settings` cannot confirm `max`.
- `{"effortLevel": null}` made the next call send `medium`, the model's own default.
  Whether a user's own `effortLevel` setting would come back is not shown: the probe
  account has none. T3 Code starts a new process to clear an effort for this reason.
- A catalog model with no effort support drops `--effort` itself. Haiku 4.5 (`claude-haiku-4-5-20251001`, listed in `initialize` with no `supportsEffort`) launched in stream-json mode with `--effort xhigh` answered normally, and its API call carried no effort (checked through the same proxy after the scripted run).
- The effort survives `set_model`: after `low` at launch, the Sonnet turn still sent `low`.
- `apply_flag_settings` with `{"fastMode": true}` cleared the SDK opt-in gate. The next
  Opus turn's `system/init` reported `fast_mode_state: off` with
  `fast_mode_disabled_reason: extra_usage_disabled`, since the probe account has extra
  usage turned off, and the API call carried no `speed`. So the account, not only the
  request, decides whether fast mode runs, and Claude says which in `system/init` and
  `result`.

## Codex 0.166.0-alpha.5.1

**Catalog.** `model/list` gives each model `supportedReasoningEfforts`
(`[{reasoningEffort, description}]`), `defaultReasoningEffort`, and `serviceTiers`
(`[{id, name, description}]`; every listed model had `{"id":"priority","name":"Fast"}`),
plus `defaultServiceTier` (null for all) and the deprecated `additionalSpeedTiers`
(`["fast"]`).

**`thread/start`** reports the effective `model`, `reasoningEffort` and `serviceTier` from
the user's configuration (here `high` and `priority`).

**`turn/start`** takes `model`, `effort` and `serviceTier`, each "for this turn and
subsequent turns":

- With the `experimentalApi` capability, Codex sends `thread/settings/updated` when a
  turn changes any of them, with the thread's `model`, `effort` and `serviceTier`. A turn
  that changes nothing gets none. `thread/read` also reports `model` and
  `reasoningEffort`.
- A field left out keeps the thread's value. `serviceTier: null` is not the same: it
  reset the thread to `default`.
- `serviceTier: "default"` is standard speed, as the schema says.
- Codex accepted `effort: "ultra"` for a model whose catalog does not list it, with no
  error, and reported it back.
- An unknown tier (`flex`) failed the turn with the API's 400, and the thread kept it:
  the next `thread/resume` reported `serviceTier: flex`.
- `thread/resume` with no settings reports the thread's last overrides: the model,
  effort and tier the last turn set, the failed one's included.

## What this means for Keystone

- Effort levels and the fast option come from these catalogs, never from a list in
  Keystone.
- Claude can change model, effort and fast mode in place before a turn, with `set_model`
  and `apply_flag_settings`. Going back to "no effort chosen" cannot be done in place
  with certainty.
- Codex takes all three on `turn/start`, keeps them on the thread, and confirms them in
  `thread/settings/updated`. Keystone must leave a field out, never send `null`, to keep
  the thread's value.
- A tier or effort the catalog does not offer must not be sent: Codex keeps it on the
  thread even after the turn fails.

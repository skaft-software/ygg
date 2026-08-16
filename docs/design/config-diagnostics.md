# Configuration diagnostics

Ygg configuration is compatibility-first: unknown TOML keys are visible, but
they do not make an otherwise usable configuration fail unless strict mode is
explicitly enabled.

## Sources and trust

Diagnostics retain the source that introduced a key:

- **global** — the user-level configuration file; and
- **project** — `<workspace>/.ygg/config.toml`, loaded only after
  `--workspace-trusted` grants project trust.

A missing home directory disables global configuration. Ygg never substitutes
the invocation directory as user scope, because that would let an untrusted
project provide user-trust configuration.

Normal value precedence remains:

1. global configuration;
2. trusted project configuration, subject to project-layer restrictions;
3. environment variables; and
4. explicit CLI arguments.

Diagnostics are collected from each loaded TOML layer before values are merged,
so an overridden typo is still reported with its original source.

## Unknown keys

TOML is deserialized with `serde_ignored`. Each unknown path is normalized,
sorted, and deduplicated per source. A diagnostic contains:

- `global` or `project` source class;
- the concrete configuration path;
- one-based line and column of the key;
- the full dotted key, including supported nested `compaction.*` paths; and
- a bounded edit-distance suggestion when a nearby supported key is
  unambiguous enough.

Example:

```text
warning: project config /repo/.ygg/config.toml:8:1: unknown configuration key "compaction.keep_recent_turn"; did you mean "compaction.keep_recent_tokens"?
```

Known compatibility aliases are part of the schema and do not warn. These
include `compaction.policy` for `compaction.mode` and `exec_timeout_secs` for
`bash_timeout_secs`; their CLI/environment compatibility forms remain accepted
at the same enforcement boundary.

Malformed TOML, invalid UTF-8, unsafe files, oversized files, and invalid values
are not unknown-key compatibility cases. They continue to fail immediately.
Configuration files are bounded regular files read through Ygg's secure file
helper.

## Strict mode

Unknown keys become fatal only when strict mode is opted into through one of:

```text
--strict-config
strict_config = true
YGG_STRICT_CONFIG=true
```

Strictness is resolved after global, trusted-project, and environment layers are
merged. The resulting error lists every collected diagnostic rather than
stopping at the first typo:

```text
strict configuration rejected unknown keys:
  - global config ...: unknown configuration key ...
  - project config ...: unknown configuration key ...
```

Without strict mode, the same diagnostics are warnings and recognized settings
continue to load. This is intentional for forward and backward compatibility:
upgrading or downgrading one Ygg binary does not silently hide a typo, but a
newer key does not disable an older binary by default.

## Adding a setting

A new TOML setting should update all applicable pieces together:

1. the deserialized `ConfigLayer` field and merge policy;
2. the supported top-level or `compaction` diagnostic schema;
3. environment and CLI forms, if exposed;
4. compatibility aliases only when a real previous spelling exists; and
5. tests for source, location, suggestion, warning/default behavior, and strict
   rejection.

Do not add aliases merely to suppress diagnostics. An alias is a maintained
configuration contract; a typo should receive a diagnostic instead.

# Module roadmap

This roadmap separates runtime stability from future packaging and UX.

1. **PR 1:** ADR and specifications only.
2. **PR 2:** minimal v3 runtime: `default_command`, command custom-emoji
   entities, `message.created`, request-scoped `message_ref`, `message.react`,
   and tests.
3. **PR 3:** `.lmod` and repository inspection, staging, source digests, and
   confirmation plans; no builds.
4. **PR 4:** Nix build/substitution worker, output validation, imperative
   profile/registry, and rollback.
5. **PR 5:** built-in `,lm` UX, reply-to-file and repository references, plus
   install/update/remove/rollback.
6. **PR 6:** opt-in declarative integration through a managed file or generated
   patch, never arbitrary configuration rewriting.

## Acceptance gates

PR 1 is documentation-only. PR 2 must retain v2 compatibility, scoped opaque
references, bounded typed actions, hardened process cleanup, and no raw MTProto
authority for modules. Later work may proceed only after its preceding contract
and tests are stable.

The installer, `.lmod` extraction, Nix orchestration, cache configuration,
profiles, registry generations, rollback, `,lm`, and declarative configuration
changes are deferred until PR 3 or later.

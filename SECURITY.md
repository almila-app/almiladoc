# Security policy

Report privately through [GitHub vulnerability reporting](https://github.com/almila-app/almiladoc/security/advisories/new), never public issues; include version/commit, format and minimal input or its generator.

In scope for untrusted inputs:

- crashes, panics or aborts;
- unbounded memory/CPU/output or archive/entity expansion beyond configured limits;
- reads/writes outside supplied input;
- network access not configured by the caller.

Fixes receive GitHub advisories naming their release; supported version is latest release, or main before first release.

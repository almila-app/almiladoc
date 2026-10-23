# Security policy

Report a vulnerability privately through [GitHub private vulnerability reporting](https://github.com/almila-app/almiladoc/security/advisories/new). Never open a public issue for one.

almiladoc parses untrusted documents. In scope:

- a crash, panic or abort on any input;
- unbounded memory, CPU or output growth;
- archive or entity expansion past the configured limits;
- reading or writing outside the given input;
- any network access the caller did not configure.

Include the almiladoc version or commit, the format, and a minimal input or how to build one. Fixed vulnerabilities are published as GitHub security advisories with the release that fixes them.

Supported versions: the latest release; before the first release, `main`.

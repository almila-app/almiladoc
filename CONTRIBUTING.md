# Contributing

Issues and pull requests are welcome. Report vulnerabilities privately, as [SECURITY.md](SECURITY.md) describes.

## Terms

almiladoc is Fair Source software under FSL-1.1-MIT, with the MIT-licensed portions REUSE.toml lists under MIT ([NOTICE](NOTICE)).

By submitting a contribution you grant Harun Erkurt, as licensor of almiladoc, and the licensor's successors and assigns a perpetual, worldwide, non-exclusive, royalty-free, irrevocable licence to use, reproduce, modify, distribute and sublicense the contribution, under FSL-1.1-MIT, the MIT License or any other terms, together with a licence under any patent you hold that the contribution would necessarily infringe. You confirm that the contribution is your own work, or that you have the right to submit it on these terms. These terms grant nothing to anyone else: others receive your contribution under the project's licence.

Sign off every commit (`git commit -s`) and give it a subject of at most 72 columns alone on its first line, then a blank line before any body. The `Signed-off-by` line certifies the [Developer Certificate of Origin 1.1](https://developercertificate.org/) for the contribution, made on the terms above.

## Changes

- One change per pull request, with a test that fails without it.
- `make ci` passes.
- A fixture is a file you may publish; never a confidential document.

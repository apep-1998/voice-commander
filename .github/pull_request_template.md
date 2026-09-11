## What this adds

<!-- The one-sentence version. -->

## Why it looks like this

<!-- Design decisions a reviewer would otherwise have to reverse-engineer from the diff. -->

## Deliberately left for later

<!-- What a reviewer might expect to find here but won't, and which PR it belongs to.
     This keeps review focused on what is actually in scope. -->

## How to verify by hand

<!-- Commands to run. `cargo test` is assumed; describe anything beyond it. -->

## Checklist

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets --all-features`
- [ ] `cargo test --workspace --all-features`
- [ ] No test requires a microphone or network access

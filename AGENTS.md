# Repository guidance

## Commit subjects

Use a lowercase intent prefix for every commit:

- `docs:` for documentation, roadmap, and repository guidance.
- `feat:` for new capabilities.
- `test:` for test-only changes.
- `fix:` for bug fixes and protocol-correctness changes.

Choose the prefix from the primary intent when a commit changes multiple kinds
of files. Keep the subject concise and descriptive. Do not leave unprefixed
subjects or merge-message commits in the published PR history; flatten merges
when rebasing. When rewording existing commits, preserve the body, author, and
trailers.

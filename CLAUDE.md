# CLAUDE.md

## This repository is public

Everything committed here, including commit messages, PR descriptions, workflow logs and release
notes, is public.

- Do not copy anything from a private repository into this one: no source files, scripts,
  test vectors, fixtures, keys (not even throwaway test keys), and no `SOURCE.json`-style
  vendoring records that point at one.
- Do not describe private systems that use this image: their names, internal hosts, addresses,
  file layouts, protocols or release procedures. Document the agent's own interface only
  (environment variables, HTTP API, image contents).
- Integration with a private system belongs in that system's repository, which consumes the
  published image (`ghcr.io/trimmedsky/ffmpeg-docker-playground`) and does its own signing and
  releasing.

## Checks

Run `make check` before pushing (README.md, "Checks before pushing").

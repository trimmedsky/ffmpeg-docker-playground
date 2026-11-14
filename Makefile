# `make check` is the job of .github/workflows/test-agent.yml: run it before pushing.
# The image's own suites (test-build.yml) need `docker build`; see README.md "Testing".
VENDORED := third_party/hng-cd-sign third_party/hng-fixtures

.PHONY: check
check:
	scripts/check-vendored.sh $(VENDORED)
	scripts/check-rust-version.sh
	cd agent && cargo fmt --check
	cd agent && cargo clippy --locked --all-targets -- -D warnings
	cd agent && cargo test --locked
	cd agent && cargo build --locked
	cd agent && python3 tests/integration.py target/debug/ffmpeg-agent

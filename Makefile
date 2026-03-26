build:
	@cargo build

test:
	@cargo nextest run --all-features

format:
	@cargo +nightly fmt --all

fmt-check:
	@cargo +nightly fmt --all --check

clippy:
	@cargo clippy --all-targets --all-features -- -D warnings -W clippy::pedantic

audit:
	@cargo audit

deny:
	@cargo deny check

check: build test fmt-check clippy audit deny

release:
	@cargo release tag --execute
	@git cliff -o CHANGELOG.md
	@git commit -a -n -m "Update CHANGELOG.md" || true
	@git push origin master
	@cargo release push --execute

update-submodule:
	@git submodule update --init --recursive --remote

.PHONY: build test format fmt-check clippy audit deny check release update-submodule

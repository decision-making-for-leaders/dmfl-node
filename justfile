default:
    @just --list

build-provision:
    cargo build --release -p iota-provision

test-provision:
    cargo test -p iota-provision

check:
    cargo test --workspace
    bash -n tests/*.sh

provision *args: build-provision
    sudo ./target/release/iota-provision {{args}}

build-image version="1.0.0":
    sudo mkosi --image-version={{version}} --force build

clean:
    cargo clean

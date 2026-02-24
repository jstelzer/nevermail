release:
    cargo build --release

build:
    cargo build

reset:
    rm ~/.local/share/nevermail/*.db

run:
    cargo run

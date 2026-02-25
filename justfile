prefix := env("PREFIX", "$HOME/.local")
bin_dir := prefix / "bin"
icon_dir := prefix / "share/icons/hicolor"
app_dir := prefix / "share/applications"

release: build
    cargo build --release

build:
    cargo clippy --bin "nevermail" -p nevermail
    cargo build
    cargo test

reset:
    rm ~/.local/share/nevermail/*.db

run:
    cargo run

install: release
    install -Dm755 target/release/nevermail {{bin_dir}}/nevermail
    install -Dm644 nevermail.desktop {{app_dir}}/nevermail.desktop
    install -Dm644 images/nevermail-16.png  {{icon_dir}}/16x16/apps/nevermail.png
    install -Dm644 images/nevermail-32.png  {{icon_dir}}/32x32/apps/nevermail.png
    install -Dm644 images/nevermail-48.png  {{icon_dir}}/48x48/apps/nevermail.png
    install -Dm644 images/nevermail-64.png  {{icon_dir}}/64x64/apps/nevermail.png
    install -Dm644 images/nevermail-128.png {{icon_dir}}/128x128/apps/nevermail.png
    install -Dm644 images/nevermail-256.png {{icon_dir}}/256x256/apps/nevermail.png
    install -Dm644 images/nevermail-512.png {{icon_dir}}/512x512/apps/nevermail.png
    -gtk-update-icon-cache {{icon_dir}}

uninstall:
    rm -f {{bin_dir}}/nevermail
    rm -f {{app_dir}}/nevermail.desktop
    for size in 16 32 48 64 128 256 512; do rm -f {{icon_dir}}/${size}x${size}/apps/nevermail.png; done
    -gtk-update-icon-cache {{icon_dir}}

icons:
    cd images && for size in 512 256 128 64 48 32 16; do magick nevermail.png -resize ${size}x${size} nevermail-${size}.png; done

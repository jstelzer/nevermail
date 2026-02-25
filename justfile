app_id := "com.neverlight.email"
prefix := env("PREFIX", "$HOME/.local")
bin_dir := prefix / "bin"
icon_dir := prefix / "share/icons/hicolor"
app_dir := prefix / "share/applications"
metainfo_dir := prefix / "share/metainfo"

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
    install -Dm644 resources/{{app_id}}.desktop {{app_dir}}/{{app_id}}.desktop
    install -Dm644 resources/{{app_id}}.metainfo.xml {{metainfo_dir}}/{{app_id}}.metainfo.xml
    for size in 16 32 48 64 128 256 512; do \
        install -Dm644 resources/icons/hicolor/${size}x${size}/apps/{{app_id}}.png \
            {{icon_dir}}/${size}x${size}/apps/{{app_id}}.png; \
    done
    -gtk-update-icon-cache {{icon_dir}}
    -update-desktop-database {{app_dir}}

uninstall:
    rm -f {{bin_dir}}/nevermail
    rm -f {{app_dir}}/{{app_id}}.desktop
    rm -f {{metainfo_dir}}/{{app_id}}.metainfo.xml
    for size in 16 32 48 64 128 256 512; do \
        rm -f {{icon_dir}}/${size}x${size}/apps/{{app_id}}.png; \
    done
    -gtk-update-icon-cache {{icon_dir}}
    -update-desktop-database {{app_dir}}

icons:
    cd images && for size in 512 256 128 64 48 32 16; do \
        magick nevermail.png -resize ${size}x${size} nevermail-${size}.png; \
    done
    for size in 16 32 48 64 128 256 512; do \
        cp images/nevermail-${size}.png resources/icons/hicolor/${size}x${size}/apps/{{app_id}}.png; \
    done

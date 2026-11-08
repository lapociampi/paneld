paneld VERSION for TARGET

One static binary: nothing else to install. Check the download first,
where the archive and SHA256SUMS from the release page sit:

    sha256sum -c SHA256SUMS --ignore-missing

Install on a Debian-like system with systemd (Raspberry Pi OS Lite,
for instance), from the unpacked folder:

    sudo install -m 0755 paneld /usr/local/bin/paneld
    sudo install -m 0644 paneld.service /etc/systemd/system/paneld.service
    sudo useradd --system --no-create-home --shell /usr/sbin/nologin \
        --groups video,input paneld
    sudo install -d -m 0750 -o paneld -g paneld /etc/paneld /var/lib/paneld

Then write your config to /etc/paneld/paneld.conf. The examples are a
good start (examples/ in the repository, at this version):

    https://github.com/lapociampi/paneld/tree/vVERSION/examples

Check it and start the service:

    sudo -u paneld paneld check -c /etc/paneld/paneld.conf
    sudo systemctl daemon-reload
    sudo systemctl enable --now paneld

To upgrade, install the new binary over the old one and run
`sudo systemctl restart paneld`.

paneld finds the display (DRM, then /dev/fb0) and the touchscreen by
itself. The config format, the screen settings (dimming, night mode),
the PIN and secrets, and the rest of the setup are in the docs:

    https://github.com/lapociampi/paneld/blob/vVERSION/docs/deploy.md

paneld is MIT licensed (LICENSE). The licenses of what is built into
it (LVGL, libdrm, the fonts, musl, the Rust standard library and
crates) are in licenses/.

{
  testers,
  stash,
  sway-unwrapped,
}:
testers.runNixOSTest {
  name = "stash-wayland";

  nodes.machine = {
    users.users.alice = {
      isNormalUser = true;
      uid = 1000;
    };

    environment.systemPackages = [stash]; # no need to enable systemd service
    hardware.graphics.enable = true;

    services = {
      graphical-desktop.enable = false;
      greetd = {
        enable = true;
        settings.default_session = {
          command = "${sway-unwrapped}/bin/sway --config /dev/null";
          user = "alice";
        };
      };
    };

    virtualisation.qemu.options = ["-vga none" "-device virtio-gpu-pci"];
  };

  testScript = ''
    import shlex

    WAYLAND_CLIENT = (
        "env XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-1"
    )

    def alice(command):
        return "su - alice -c " + shlex.quote(command)

    start_all()
    machine.wait_for_unit("greetd.service")
    machine.wait_for_file("/run/user/1000/wayland-1")

    with subtest("stash multicall binaries round-trip clipboard bytes"):
        machine.succeed(
            alice(
                "printf '\\001stash-vm\\377' | "
                f"{WAYLAND_CLIENT} wl-copy --type application/x-stash-vm "
                ">/dev/null 2>&1"
            )
        )
        machine.wait_until_succeeds(
            alice(
                f"{WAYLAND_CLIENT} wl-paste --no-newline "
                "--type application/x-stash-vm "
                "> /tmp/stash-clipboard && "
                "printf '\\001stash-vm\\377' | cmp - /tmp/stash-clipboard"
            )
        )

    with subtest("stash watch persists a compositor clipboard change"):
        machine.succeed(
            alice(
                f"printf baseline | {WAYLAND_CLIENT} "
                "wl-copy --type text/plain >/dev/null 2>&1"
            )
        )
        machine.succeed(
            alice(
                f"{WAYLAND_CLIENT} stash --db-path /tmp/stash.sqlite "
                "watch --mime-type text "
                "> /tmp/stash-watch.log 2>&1 & echo $! > /tmp/stash-watch.pid"
            )
        )
        machine.sleep(1)
        machine.succeed(
            alice(
                f"printf stash-vm-watch | {WAYLAND_CLIENT} "
                "wl-copy --type text/plain >/dev/null 2>&1"
            )
        )
        machine.wait_until_succeeds(
            alice(
                "stash --db-path /tmp/stash.sqlite list --format json | "
                "grep -F stash-vm-watch"
            )
        )
        machine.succeed("kill $(cat /tmp/stash-watch.pid)")

    with subtest("stash persistence preserves uri-list"):
        machine.succeed(
            alice(
                f"printf baseline | {WAYLAND_CLIENT} "
                "wl-copy --type text/plain >/dev/null 2>&1"
            )
        )
        machine.succeed(
            alice(
                f"{WAYLAND_CLIENT} stash "
                "--db-path /tmp/stash-persist.sqlite watch --persist "
                "> /tmp/stash-persist.log 2>&1 & "
                "echo $! > /tmp/stash-persist.pid"
            )
        )
        machine.sleep(1)
        machine.succeed(
            "systemd-run --quiet --unit=stash-uri-source --uid=alice "
            "--setenv=XDG_RUNTIME_DIR=/run/user/1000 "
            "--setenv=WAYLAND_DISPLAY=wayland-1 /bin/sh -c "
            + shlex.quote(
                "printf 'file:///tmp/stash-uri-list.webp\\n' | "
                "/run/current-system/sw/bin/wl-copy --foreground "
                "--paste-once --omit-additional-text-mime-types "
                "--type text/uri-list"
            )
        )
        machine.wait_for_unit("stash-uri-source.service")
        machine.wait_until_fails(
            "systemctl is-active stash-uri-source.service"
        )
        machine.wait_until_succeeds(
            alice(
                f"{WAYLAND_CLIENT} wl-paste --list-types | "
                "grep -Fx text/uri-list"
            )
        )
        machine.succeed(
            alice(
                f"{WAYLAND_CLIENT} wl-paste --no-newline "
                "--type text/uri-list > /tmp/stash-uri-list && "
                "printf 'file:///tmp/stash-uri-list.webp\\n' | "
                "cmp - /tmp/stash-uri-list"
            )
        )
        machine.succeed(
            alice(f"{WAYLAND_CLIENT} wl-copy --clear >/dev/null 2>&1")
        )
        machine.succeed("kill $(cat /tmp/stash-persist.pid)")
  '';
}

{
  pkgs,
  stash,
}:
pkgs.testers.runNixOSTest {
  name = "stash-wayland";

  nodes.machine = {pkgs, ...}: {
    users.users.alice = {
      isNormalUser = true;
      uid = 1000;
    };

    environment.systemPackages = [ stash ];

    services.cage = {
      enable = true;
      user = "alice";
      program = "${pkgs.coreutils}/bin/sleep infinity";
    };

    virtualisation.qemu.options = [ "-vga none" "-device virtio-gpu-pci" ];
  };

  testScript = ''
    import shlex

    WAYLAND_ENV = "XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0"

    def alice(command):
        return "su - alice -c " + shlex.quote(f"{WAYLAND_ENV} {command}")

    start_all()
    machine.wait_for_unit("cage-tty1.service")
    machine.wait_for_file("/run/user/1000/wayland-0")

    with subtest("stash multicall binaries round-trip clipboard bytes"):
        machine.succeed(
            alice(
                "printf '\\001stash-vm\\377' | "
                "wl-copy --type application/x-stash-vm"
            )
        )
        machine.wait_until_succeeds(
            alice(
                "wl-paste --no-newline --type application/x-stash-vm "
                "> /tmp/stash-clipboard && "
                "printf '\\001stash-vm\\377' | cmp - /tmp/stash-clipboard"
            )
        )

    with subtest("stash watch persists a compositor clipboard change"):
        machine.succeed(alice("printf baseline | wl-copy --type text/plain"))
        machine.succeed(
            alice(
                "stash --db-path /tmp/stash.sqlite watch --mime-type text "
                "> /tmp/stash-watch.log 2>&1 & echo $! > /tmp/stash-watch.pid"
            )
        )
        machine.sleep(1)
        machine.succeed(
            alice("printf stash-vm-watch | wl-copy --type text/plain")
        )
        machine.wait_until_succeeds(
            alice(
                "stash --db-path /tmp/stash.sqlite list --format json | "
                "grep -F stash-vm-watch"
            )
        )
        machine.succeed("kill $(cat /tmp/stash-watch.pid)")
  '';
}

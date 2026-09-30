{ pkgs, btrmaps }:
pkgs.testers.runNixOSTest {
  name = "btrmaps";
  nodes.machine = {
    virtualisation.emptyDiskImages = [ 1024 ];
    # sway needs a GPU device; it renders with pixman and egui with llvmpipe.
    virtualisation.qemu.options = [ "-vga none -device virtio-gpu-pci" ];
    hardware.graphics.enable = true;
    environment.systemPackages = [
      btrmaps
      pkgs.btrfs-progs
      pkgs.grim
    ];
    environment.variables = {
      SWAYSOCK = "/tmp/sway-ipc.sock";
      WLR_RENDERER = "pixman";
      LIBGL_ALWAYS_SOFTWARE = "1";
    };
    programs.sway.enable = true;
    # A virtual mouse, to click through the viewer.
    programs.ydotool.enable = true;
    users.users.alice = {
      isNormalUser = true;
      extraGroups = [ "wheel" ];
      password = "hunter2";
    };
    services.getty.autologinUser = "alice";
    programs.bash.loginShellInit = ''
      if [ "$(tty)" = /dev/tty1 ]; then exec sway; fi
    '';
  };
  testScript = ''
    import json

    def mib(name, n):
        machine.succeed(f"dd if=/dev/urandom of={name} bs=1M count={n} status=none && sync")

    machine.succeed(
        "mkfs.btrfs -q /dev/vdb",
        "mkdir -p /top /root-subvol",
        "mount /dev/vdb /top",
        "btrfs subvolume create /top/@",
    )
    mib("/top/@/snapped", 24)
    machine.succeed("btrfs subvolume snapshot -r /top/@ /top/@snap")
    mib("/top/@/unique", 24)
    mib("/top/@/big", 24)
    machine.succeed("cp --reflink=always /top/@/big /top/@/big-reflink && sync")
    mib("/top/@/hard1", 24)
    machine.succeed("ln /top/@/hard1 /top/@/hard2")
    mib("/top/@/overwritten", 24)
    machine.succeed("dd if=/dev/urandom of=/top/@/overwritten bs=1M seek=8 count=8 conv=notrunc status=none && sync")
    mib("/top/top-level-file", 8)
    machine.succeed(
        "mount -o remount,compress=zstd /top",
        "{ yes compressible-line || true; } | head -c 32M > /top/@/text && sync",
    )

    # Point it at a non-top-level subvolume so it has to mount the top level itself.
    machine.succeed("mount -o subvol=@ /dev/vdb /root-subvol")
    # Let the test user delete in @, for the delete-from-the-menu check below.
    machine.succeed("chmod 777 /root-subvol")
    machine.succeed("btrmaps scan --order 7 -o /tmp/scan.jsonl /root-subvol")
    machine.succeed("test -z \"$(ls /tmp | grep btrmaps-)\"")
    machine.succeed("! grep -q /tmp/btrmaps- /proc/mounts")
    machine.copy_from_machine("/tmp/scan.jsonl", "")

    msgs = [json.loads(l) for l in machine.succeed("cat /tmp/scan.jsonl").splitlines()]
    header = msgs[0]
    assert header["t"] == "header" and header["order"] == 7, header
    assert msgs[-1] == {"t": "done"}, msgs[-1]
    sets = [m for m in msgs if m["t"] == "set"]
    assert [s["id"] for s in sets] == list(range(len(sets))), "set ids not dense and ordered"
    levels = [m["level"] for m in msgs if m["t"] == "cells"]
    assert levels == sorted(levels) and levels[0] == 6 and levels[-1] == 7, levels

    # Every set arrives before its first use.
    seen = set()
    for m in msgs:
        if m["t"] == "set":
            seen.add(m["id"])
        if m["t"] == "cells":
            assert all(c[1] in seen for c in m["cells"]), "cell before its set"

    # The finest level covers the whole curve exactly once.
    final = {}
    for m in msgs:
        if m["t"] == "cells" and m["level"] == 7:
            for d, s, algo, ratio, _ in m["cells"]:
                assert d not in final, f"cell {d} twice"
                final[d] = (sets[s], algo, ratio)
    assert sorted(final) == list(range(4 ** 7))

    by_key = {}
    for s, _, _ in final.values():
        key = (s["kind"], tuple(s.get("paths", [])))
        by_key[key] = by_key.get(key, 0) + header["cell"]
    for key, size in sorted(by_key.items(), key=lambda kv: -kv[1]):
        print(f"{size >> 20:6} MiB  {key}")

    def size(kind, *files):
        return by_key.get((kind, tuple(sorted(files))), 0) >> 20

    def near(got, want, what):
        assert abs(got - want) <= want * 0.15 + 1, f"{what}: {got} MiB, expected ~{want}"

    near(size("data", "@/unique"), 24, "unique file")
    near(size("data", "@/snapped", "@snap/snapped"), 24, "snapshot-shared file")
    near(size("data", "@/big", "@/big-reflink"), 24, "reflinked pair")
    near(size("data", "@/hard1", "@/hard2"), 24, "hardlinked pair")
    near(size("data", "@/overwritten"), 24, "overwritten file")
    near(size("unreachable", "@/overwritten"), 8, "overwritten extent tail")
    near(size("data", "top-level-file"), 8, "top-level file")
    assert size("free") > 0, "no free space found"
    assert size("metadata") > 0, "no metadata found"
    assert not any(s["kind"] == "error" for s in sets), "probe errors"

    def the_set(*files):
        return next(s for s in sets if s.get("paths") == list(files))

    snapped = the_set("@/snapped", "@snap/snapped")
    assert (snapped["files"], snapped["subvolumes"]) == (2, 2), snapped
    hard = the_set("@/hard1", "@/hard2")
    assert (hard["files"], hard["subvolumes"], hard["path_count"]) == (1, 1, 2), hard

    # Shared bytes belong to the deepest directory holding every path.
    assert snapped["dominator"] == "", snapped
    assert the_set("@/big", "@/big-reflink")["dominator"] == "@", "reflink dominator"
    assert hard["dominator"] == "@", hard

    def cells_of(*files):
        return [(a, r) for s, a, r in final.values() if s.get("paths") == list(files) and s["kind"] == "data"]

    text = cells_of("@/text")
    assert text and all(a == "zstd" and r > 500 for a, r in text), text
    plain = cells_of("@/unique")
    assert plain and all(a == "none" and r == 100 for a, r in plain), plain
    assert all(a == "unknown" for s, a, _ in final.values() if s["kind"] in ("free", "metadata", "system"))

    # Ages: every extent's generation is known, later writes have later generations,
    # and the calibration btrfs records is ordered and ends at the scan.
    gens = {d: g for m in msgs if m["t"] == "cells" and m["level"] == 7 for d, _, _, _, g in m["cells"]}
    def gen_of(*files):
        return {gens[d] for d, (s, _, _) in final.items() if s.get("paths") == list(files) and s["kind"] == "data"}
    assert min(gen_of("@/text")) > max(gen_of("@/unique")) > max(gen_of("@/snapped", "@snap/snapped")) > 0
    cal = header["calibration"]
    assert len(cal) >= 2 and cal == sorted(cal), cal
    assert all(t1 >= t0 for (_, t0), (_, t1) in zip(cal, cal[1:])), cal

    # Full resolution: probes sent on stdin come back as whole runs (extents, free
    # stretches) in bytes along the curve, owned by exactly the files there.
    cell = header["cell"]
    def middle_of(kind, *files):
        ds = sorted(d for d, (s, _, _) in final.items() if s["kind"] == kind and s.get("paths", []) == list(files))
        return ds[len(ds) // 2] * cell + cell // 2
    unique_at, free_at = middle_of("data", "@/unique"), middle_of("free")
    request = json.dumps({"t": "probe", "positions": [unique_at, free_at]})
    machine.succeed(f"echo '{request}' | btrmaps scan --order 7 /root-subvol > /tmp/probe.jsonl")
    answers = [json.loads(l) for l in machine.succeed("cat /tmp/probe.jsonl").splitlines()]
    probe_sets = {m["id"]: m for m in answers if m["t"] == "set"}
    runs = [r for m in answers if m["t"] == "runs" for r in m["runs"]]
    def run_over(pos):
        return next(r for r in runs if r[0] <= pos < r[0] + r[1])
    start, length, sid, _, _, _ = run_over(unique_at)
    assert probe_sets[sid].get("paths") == ["@/unique"], probe_sets[sid]
    assert length >= 1 << 20, f"a data answer covers its whole extent, got {length} bytes"
    start, length, sid, _, _, _ = run_over(free_at)
    assert probe_sets[sid]["kind"] == "free" and length > 4096, (probe_sets[sid], length)

    # The app: launch it, pick the filesystem, let polkit run the scan.
    machine.wait_for_file("/tmp/sway-ipc.sock")
    alice = "su - alice -c "
    window = alice + "'swaymsg -t get_tree' | grep -q '\"app_id\": \"btrmaps\"'"
    ydotool = "YDOTOOL_SOCKET=/run/ydotoold/socket ydotool "
    machine.succeed(alice + "'swaymsg input type:pointer accel_profile flat'")

    def shot(name):
        machine.succeed(alice + f"'XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-1 grim -c /tmp/{name}.png'")
        machine.copy_from_machine(f"/tmp/{name}.png", "")

    def point(x, y):
        machine.succeed(ydotool + f"mousemove --absolute -x {x} -y {y}")
        machine.sleep(1)

    def click(x, y):
        point(x, y)
        machine.succeed(ydotool + "click 0xC0")
        machine.sleep(2)

    machine.succeed(alice + "'swaymsg exec btrmaps'")
    machine.wait_until_succeeds(window, timeout=60)
    machine.sleep(3)
    shot("start")
    # A narrow window: the bar's less important controls move into its "…" menu.
    machine.succeed(alice + "'swaymsg [app_id=btrmaps] floating enable, resize set 800 600, move position 0 30'")
    machine.sleep(2)
    shot("narrow")
    machine.succeed(alice + "'swaymsg [app_id=btrmaps] floating disable'")
    machine.sleep(2)
    # The only filesystem is preselected; the big button starts the scan.
    click(640, 420)  # Scan
    machine.sleep(2)
    shot("password")
    machine.succeed(ydotool + "type hunter2")
    machine.succeed(ydotool + "key 28:1 28:0")  # Enter
    # sudo accepted the typed password and ran the scan as root.
    machine.wait_until_succeeds("journalctl -t sudo | grep -q 'alice : .*COMMAND=.*btrmaps scan'", timeout=30)
    machine.sleep(20)
    machine.succeed(window)
    point(640, 790)
    shot("curve")
    # Zoom far past the scan's detail: the view refines itself from exact probes.
    for _ in range(8):
        machine.succeed(ydotool + "key 13:1 13:0")  # =, zoom in
    machine.sleep(6)
    shot("deep")
    for _ in range(8):
        machine.succeed(ydotool + "key 12:1 12:0")  # -, zoom back out
    machine.sleep(1)
    click(523, 67)  # Treemap
    point(620, 250)  # a file inside a folder: highlights its folders, fills the status bar
    shot("treemap-hover")
    point(200, 400)  # free space: described in the side pane, but nothing dims
    shot("free-hover")
    point(815, 210)  # the unique file
    machine.succeed(ydotool + "click 0xC1")  # right button
    machine.sleep(2)
    shot("menu")
    click(860, 316)  # Delete file…
    shot("confirm")
    click(453, 453)  # Delete
    # The file the menu was opened on is gone; its neighbours are not.
    machine.wait_until_fails("test -e /top/@/unique", timeout=10)
    machine.succeed("test -e /top/@/big && test -e /top/@/snapped")
    shot("deleted")
  '';
}
